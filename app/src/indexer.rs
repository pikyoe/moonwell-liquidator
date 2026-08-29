use crate::contracts::{IMToken, IMulticall3, MULTICALL3};
use crate::health::{health_factor, MarketInfo};
use crate::hypersync::HyperSync;
use crate::state::{Position, SharedState};
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol_types::{SolCall, SolEvent};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};



// Event yang mempengaruhi posisi. Kita pantau Mint/Borrow/Repay/Redeem/Transfer.
alloy::sol! {
    event Mint(address minter, uint256 mintAmount, uint256 mintTokens);
    event Redeem(address redeemer, uint256 redeemAmount, uint256 redeemTokens);
    event Borrow(address borrower, uint256 borrowAmount, uint256 accountBorrows, uint256 totalBorrows);
    event RepayBorrow(address payer, address borrower, uint256 repayAmount, uint256 accountBorrows, uint256 totalBorrows);
    event Transfer(address indexed from, address indexed to, uint256 amount);
    event LiquidateBorrow(address liquidator, address borrower, uint256 repayAmount, address mTokenCollateral, uint256 seizeTokens);
    /// Moonwell ChainlinkOEVWrapper — event yang BENAR-BENAR dipancarkan saat
    /// `updatePriceEarlyAndLiquidate` dijalankan (diverifikasi di bytecode
    /// seluruh wrapper Base & source resmi moonwell-contracts-v2).
    /// Kehadirannya = harga kolateral wrapper baru di-unlock + likuidasi via
    /// jalur OEV sedang/sudah terjadi → scan ulang langsung lebih akurat
    /// daripada menunggu refresh harga 10-blok.
    /// Catatan audit: kode lama memakai event fiktif `UpdatedPrices(uint256,
    /// int256,bool)` yang tidak pernah dipancarkan wrapper — trigger OEV mati.
    event PriceUpdatedEarlyAndLiquidated(
        address indexed borrower,
        uint256 repayAmount,
        address indexed mTokenCollateral,
        address indexed mTokenLoan,
        uint256 protocolFee,
        uint256 liquidatorFee
    );
}

#[derive(Clone)]
pub struct Indexer<P: Provider> {
    provider: P,
    hyper: HyperSync,
    state: SharedState,
    markets: Vec<Address>,
    /// Konkurensi refresh akun per blok. Dikurangi bila RPC rentan rate-limit.
    refresh_concurrency: usize,
}

impl<P: Provider + Clone + Send + Sync + 'static> Indexer<P> {
    pub fn new(provider: P, hyper: HyperSync, state: SharedState, markets: Vec<Address>) -> Self {
        Self { provider, hyper, state, markets, refresh_concurrency: 2 }
    }

    /// Setter untuk konkur¬ensi refresh — di-wheel dari config saat init.
    pub fn with_refresh_concurrency(mut self, concurrency: usize) -> Self {
        self.refresh_concurrency = concurrency.max(1);
        self
    }

    /// Bootstrap: scan event Borrow historis untuk menemukan semua borrower aktif.
    /// Ini membangun daftar akun awal yang akan dipantau health-nya.
    ///
    /// Pembacaan event kini via HyperSync (bukan `eth_getLogs` yang diblokir
    /// RPC free-tier). Chunk dipertahankan agar satu permintaan besar tidak
    /// pernah menahan startup lama; HyperSync pagination menangani sisanya.
    pub async fn bootstrap_borrowers(&self, from_block: u64) -> Result<()> {
        let latest = self.provider.get_block_number().await?;
        info!(from_block, latest, "bootstrap borrower dari event Borrow (HyperSync)");

        let mut chunk = 200_000u64;
        let mut start = from_block;
        let mut backoff = Duration::from_millis(1500);
        let mut rate_limit_retries = 0u32;
        const MAX_RATE_LIMIT_RETRIES: u32 = 10;
        while start <= latest {
            let end = (start + chunk).min(latest);
            match self
                .hyper
                .get_logs_raw(&self.markets, start, end, Some(&Borrow::SIGNATURE_HASH))
                .await
            {
                Ok(logs) => {
                    backoff = Duration::from_millis(1500);
                    rate_limit_retries = 0;
                    for log in logs {
                        if log.removed {
                            continue;
                        }
                        if let Ok(ev) = Borrow::decode_log(&log.inner) {
                            let borrower = ev.borrower;
                            // snapshot akun ini di market tsb
                            let market = log.address();
                            if let Err(e) = self.refresh_account(market, borrower).await {
                                warn!(?e, "refresh awal gagal");
                            }
                        }
                    }
                    start = end + 1;
                }
                // Rate limit — jangan lewati rentang; tidur dulu agar HyperSync
                // pulih, lalu coba ulang rentang yang sama.
                Err(e) if is_rate_limited(&e) && rate_limit_retries < MAX_RATE_LIMIT_RETRIES => {
                    rate_limit_retries += 1;
                    warn!(?e, rate_limit_retries, capped = MAX_RATE_LIMIT_RETRIES, backoff_ms = backoff.as_millis(), start, end, "rate limit — tidur lalu coba ulang");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                Err(e) if chunk > 5_000 => {
                    chunk /= 4;
                    backoff = Duration::from_millis(1500);
                    warn!(?e, chunk, "HyperSync ditolak — perkecil chunk, coba ulang");
                }
                Err(e) => {
                    warn!(?e, start, end, "HyperSync gagal — rentang dilewati");
                    start = end + 1;
                    rate_limit_retries = 0;
                    backoff = Duration::from_millis(1500);
                }
            }
        }
        info!(count = self.state.borrowers().len(), "bootstrap selesai");
        Ok(())
    }

    /// Sweep akun marginal-safe: refresh paksa lewat accrue-fresh agar
    /// staleness bunga (getAccountSnapshot TIDAK meng-accrue) tidak
    /// menyembunyikan borrower yang sebenarnya sudah underwater. Kerja:
    /// (1) kumpulkan akun yang cached HF-nya di bawah ambang (marginal,
    /// belum liquidatable) dari state off-chain;
    /// (2) dalam SATU kueri `Multicall3::aggregate3`, panggil
    /// `accrueInterest()` sekali per market DIKUTI `getAccountSnapshot`
    /// per (akun, market) — di dalam eth_call simulasi, accrue ditulis ke
    /// state transaksi dan snapshot-snapshot berikutnya membaca state segar;
    /// (3) decode hasil batch dan upsert ke state. Jadi 1 round-trip RPC
    /// menyegarkan semua kandidat mepet, tanpa proyeksi bunga manual.
    /// hf_threshold (dari config) menentukan siapa "marginal"; MarketInfo
    /// dibutuhkan untuk menilai HF dari state cached — harus konsisten
    pub async fn sweep_marginal_borrowers(
        &self,
        markets: HashMap<Address, MarketInfo>,
        hf_threshold: U256,
    ) -> Result<()> {
        let borrowed: Vec<Address> = self.state.borrowers();
        let mut candidates: Vec<(Address, Vec<Address>)> = Vec::new();
        for account in borrowed {
            let Some(positions) = self.state.positions.get(&account) else { continue };
            let mut tuples = Vec::new();
            for m in positions.iter() {
                tuples.push((*m.key(), m.mtoken_balance, m.borrow_balance, m.exchange_rate));
            }
            let (_, _, hf) = health_factor(&tuples, &markets);
            // < ambang (termasuk yang sudah liquidatable) disegarkan paksa;
            // yang >= ambang dibiarkan ke refresh berbasis event biasa.

            if hf < hf_threshold && hf != U256::MAX {
                let mut mlist = Vec::new();
                for m in positions.iter() {
                    mlist.push(*m.key());
                }
                candidates.push((account, mlist));
            }
        }
        if candidates.is_empty() {
            return Ok(());
        }

        // Bangun batch: accrue per market unik dulu (agar tiap snapshot
        // melihat state ter-accrue dalam batch yang sama), lalu snapshot per akun.

        let mut accrue_calls: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut snapshot_calls: Vec<(Address, Vec<u8>, Address, Address)> = Vec::new(); // (target, data, account, market)
        for (account, mlist) in &candidates {
            for &market in mlist {
                if !accrue_calls.iter().any(|(m, _)| *m == market) {
                    accrue_calls.push((market, IMToken::accrueInterestCall { }.abi_encode()));
                }
                let data = IMToken::getAccountSnapshotCall { account: *account }.abi_encode();
                snapshot_calls.push((market, data, *account, market));
            }
        }

        let mut calls = Vec::new();
        for (target, calldata) in &accrue_calls {

            calls.push(IMulticall3::Call3 {
                target: *target,
                allowFailure: true,
                callData: calldata.clone().into(),
            });
        }
        for (target, calldata, _, _)in &snapshot_calls {
            calls.push(IMulticall3::Call3 {
                target: *target,
                allowFailure: true,
                callData: calldata.clone().into(),
            });
        }

        let mcall = IMulticall3::new(MULTICALL3, &self.provider);
        let results = mcall.aggregate3(calls).call().await?;
        if results.len() != accrue_calls.len() + snapshot_calls.len() {
            anyhow::bail!("aggregate3 hasil tidak lengkap: {} != {}", results.len(), accrue_calls.len() + snapshot_calls.len());
        }

        // Pakai index snapshot_calls (setelah blok accrue) untuk upsert state.

        // `accrueInterest()` mengembalikan uint256 error code (0 = sukses) —
        // call yang "sukses" (allowFailure diterima) tetap bisa membawa kode
        // non-zero. Prekomputasi per market supaya cek sekali (bukan per akun).
        let mut accrue_result_ok: HashMap<Address, bool> = HashMap::new();
        for (i, (market, _))in accrue_calls.iter().enumerate() {
            let accrue_ok = results[i].success
                && IMToken::accrueInterestCall::abi_decode_returns(&results[i].returnData)
                    .map(|r| r == U256::ZERO)
                    .unwrap_or(false);
            accrue_result_ok.insert(*market, accrue_ok);
        }

        let mut updated = 0usize;
        for (i, (_, _, account, market))in snapshot_calls.iter().enumerate() {
            let idx = accrue_calls.len() + i;
            if !accrue_result_ok.get(market).copied().unwrap_or(false) || !results[idx].success {
                warn!(?account, ?market, "accrue+snapshot sweep gagal — state dibiarkan");
                continue;
            }
            let snap = IMToken::getAccountSnapshotCall::abi_decode_returns(&results[idx].returnData);
            match snap {
                Ok(snap) => {
                    self.state.upsert_or_remove(
                        *account,
                        *market,
                        Position {
                            mtoken_balance: snap.mTokenBalance,
                            borrow_balance: snap.borrowBalance,
                            exchange_rate: snap.exchangeRateMantissa,
                        },
                    );
                    updated += 1;
                }
                Err(e) => warn!(?e, ?account, "decode snapshot sweep gagal"),
            }
        }
        info!(candidates = candidates.len(), updated, "sweep akun marginal selesai");
        Ok(())
    }

    /// Refresh satu posisi akun di satu market dari chain.
    pub async fn refresh_account(&self, market: Address, account: Address) -> Result<()> {
        let m = IMToken::new(market, &self.provider);
        let snap = m.getAccountSnapshot(account).call().await?;
        self.state.upsert_or_remove(
            account,
            market,
            crate::state::Position {
                mtoken_balance: snap.mTokenBalance,
                borrow_balance: snap.borrowBalance,
                exchange_rate: snap.exchangeRateMantissa,
            },
        );
        Ok(())
    }

    /// Proses rentang blok — decode **semua** event posisi (Mint/Redeem/Borrow/
    /// Repay/Liquidate/Transfer), dipakai untuk replay rentang yang terlewat
    /// saat reconnect WS.
    ///
    /// Tanpa `oev_wrappers` (replay/backfill) tidak ada sinyal trigger OEV.
    pub async fn process_block_logs(&self, from_block: u64, to_block: u64) -> Result<()> {
        self.watch_block(from_block, to_block, &[]).await.map(|_| ())
    }

    /// Handler satu blok / rentang blok. Dipanggil per blok di main loop dan juga
    /// untuk mem-replay rentang yang terlewat saat reconnect WS.
    ///
    /// SATU kueri HyperSync mencakup log market (untuk refresh posisi) DAN
    /// event `PriceUpdatedEarlyAndLiquidated` wrapper OEV (untuk trigger scan)
    /// — jadi hanya `1 request` per blok, bukan 2, agar tetap di bawah batas
    /// RPM plan.
    ///
    /// Semua refresh akun dijalankan PARALEL (konkurensi terbatas) agar satu
    /// blok ramai tidak menambahkan latensi RPC beruntun pada jalur kritis
    /// deteksi. Dedup per (market, akun) dilakukan sebelum refresh.
    ///
    /// Kembali `true` bila ada event `PriceUpdatedEarlyAndLiquidated` pada
    /// rentang tsb (sinyal untuk scan jalur OEV).
    pub async fn watch_block(
        &self,
        from_block: u64,
        to_block: u64,
        oev_wrappers: &[Address],
    ) -> Result<bool> {
        let (logs, oev_trigger) = self
            .hyper
            .market_and_trigger(
                &self.markets,
                oev_wrappers,
                &PriceUpdatedEarlyAndLiquidated::SIGNATURE_HASH,
                from_block,
                to_block,
            )
            .await?;
        self.process_logs(logs).await?;
        Ok(oev_trigger)
    }

    /// Jalankan refresh akun untuk semua event di dalam daftar log, paralel.
    async fn process_logs(&self, logs: Vec<alloy::rpc::types::Log>) -> Result<()> {
        // Akumulasi (market, account) unik — satu akun bisa terlibat >1 event
        // dalam satu blok; cukup satu refresh per (market, akun).
        let mut pairs = Vec::new();
        for log in logs {
            // Log yang dibuang karena reorg tidak boleh men-trigger refresh
            // (akun di posisi blok non-final tidak valid untuk snapshot).
            if log.removed {
                continue;
            }
            let market = log.address();
            let topics = log.topics();
            if topics.len() >= 3 && topics[0] == Transfer::SIGNATURE_HASH {
                if let Ok(ev) = Transfer::decode_log(&log.inner) {
                    // Mint/burn menghasilkan transfer dengan alamat nol — jangan refresh.
                    if ev.from != Address::ZERO {
                        pairs.push((market, ev.from));
                    }
                    if ev.to != Address::ZERO {
                        pairs.push((market, ev.to));
                    }
                }
            } else if let Ok(ev) = Mint::decode_log(&log.inner) {
                pairs.push((market, ev.minter));
            } else if let Ok(ev) = Redeem::decode_log(&log.inner) {
                pairs.push((market, ev.redeemer));
            } else if let Ok(ev) = Borrow::decode_log(&log.inner) {
                pairs.push((market, ev.borrower));
            } else if let Ok(ev) = RepayBorrow::decode_log(&log.inner) {
                pairs.push((market, ev.borrower));
            } else if let Ok(ev) = LiquidateBorrow::decode_log(&log.inner) {
                pairs.push((market, ev.borrower));
            }
        }

        // Dedup: cukup satu refresh per (market, akun) per blok.
        pairs.sort_unstable();
        pairs.dedup();

        // Refresh paralel dengan konkur¬ensi terbatas — hindari saturasi RPC
        // saat satu blok men-trigger banyak akun. Provider & state di-clone
        // (owned) agar task bisa di-spawn `'static`.
        let provider = self.provider.clone();
        let state = self.state.clone();
        let sem = Arc::new(tokio::sync::Semaphore::new(self.refresh_concurrency));
        let mut set = tokio::task::JoinSet::new();
        for (market, account) in pairs {
            let provider = provider.clone();
            let state = state.clone();
            let sem = sem.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await;
                let m = IMToken::new(market, &provider);
                let mut attempt = 0u32;
                loop {
                    match m.getAccountSnapshot(account).call().await {
                        Ok(snap) => {
                            state.upsert_or_remove(
                                account,
                                market,
                                crate::state::Position {
                                    mtoken_balance: snap.mTokenBalance,
                                    borrow_balance: snap.borrowBalance,
                                    exchange_rate: snap.exchangeRateMantissa,
                                },
                            );
                            break;
                        }
                        Err(e) if attempt < 2 => {
                            attempt += 1;
                            warn!(
                                ?e, ?account, ?market, attempt,
                                "refresh snapshot gagal — coba ulang"
                            );
                            tokio::time::sleep(Duration::from_millis(200u64 << attempt)).await;
                        }
                        Err(e) => {
                            warn!(
                                ?e, ?account, ?market,
                                "refresh snapshot gagal — state tetap basi (akan disegarkan event berikutnya)"
                            );
                            break;
                        }
                    }
                }
            });
        }
        while (set.join_next().await).is_some() {}
        Ok(())
    }
}

/// Deteksi HTTP 429 (rate limit) dari error transport alloy. Alih-alih
/// melakukan pattern-match pada enum generik (rapuh antar versi), kita cek
/// representasi string: 429 / "Too Many Requests" / status 4xx+body.
fn is_rate_limited(e: &dyn std::fmt::Display) -> bool {
    let s = e.to_string();
    s.contains("429")
        || s.contains("Too Many Requests")
        || s.contains("rate limit")
        || s.contains("429 Too Many Requests")
}
