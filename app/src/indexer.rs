use crate::contracts::IMToken;
use crate::hypersync::HyperSync;
use crate::state::SharedState;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::sol_types::SolEvent;
use anyhow::Result;
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
