use crate::contracts::IMToken;
use crate::state::SharedState;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
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
    /// Moonwell OEV wrapper — dipancarkan ketika harga wrapper diperbaharui.
    /// Saat ini berarti "harga on-chain baru ditukwal" dan liquidasi path ada.
    /// Bot menghasilkan trigger lebih cepat daripada refresh harga 10-block.
    event UpdatedPrices(uint256 roundId, int256 answer, bool isOEV);
}

pub struct Indexer<P: Provider> {
    provider: P,
    state: SharedState,
    markets: Vec<Address>,
    /// Konkurensi refresh akun per blok. Dikurangi bila RPC rentan rate-limit.
    refresh_concurrency: usize,
}

impl<P: Provider + Clone + Send + Sync + 'static> Indexer<P> {
    pub fn new(provider: P, state: SharedState, markets: Vec<Address>) -> Self {
        Self { provider, state, markets, refresh_concurrency: 2 }
    }

    /// Setter untuk konkur¬ensi refresh — di-wheel dari config saat init.
    pub fn with_refresh_concurrency(mut self, concurrency: usize) -> Self {
        self.refresh_concurrency = concurrency.max(1);
        self
    }

    /// Bootstrap: scan event Borrow historis untuk menemukan semua borrower aktif.
    /// Ini membangun daftar akun awal yang akan dipantau health-nya.
    pub async fn bootstrap_borrowers(&self, from_block: u64) -> Result<()> {
        let latest = self.provider.get_block_number().await?;
        info!(from_block, latest, "bootstrap borrower dari event Borrow");

        // Chunk agar RPC publik tidak overload. Bila ditolak (range/limit),
        // perkecil chunk dan coba ulang rentang yang sama — rentang hanya
        // dilewati setelah chunk minimum pun gagal.
        let mut chunk = 50_000u64;
        let mut start = from_block;
        let mut backoff = Duration::from_millis(1500);
        // Cap retry berturut-turut saat 429 — kalau provider terus membatasi
        // (kuota habis), jangan menjebak startup selamanya; setelah batas ini
        // fall-through ke jalur perkecil-chunk / lewati agar bot tetap mulai.
        let mut rate_limit_retries = 0u32;
        const MAX_RATE_LIMIT_RETRIES: u32 = 10;
        while start <= latest {
            let end = (start + chunk).min(latest);
            let filter = Filter::new()
                .address(self.markets.clone())
                .event_signature(Borrow::SIGNATURE_HASH)
                .from_block(start)
                .to_block(end);
            match self.provider.get_logs(&filter).await {
                Ok(logs) => {
                    backoff = Duration::from_millis(1500);
                    rate_limit_retries = 0;
                    for log in logs {
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
                // Rate limit (429) — jangan lewati rentang; tidur dulu agar
                // provider pulih, lalu coba ulang rentang yang sama. Ini
                // penting agar bootstrap TIDAK melewati blok yang belum di-scan.
                Err(e) if is_rate_limited(&e) && rate_limit_retries < MAX_RATE_LIMIT_RETRIES => {
                    rate_limit_retries += 1;
                    warn!(?e, rate_limit_retries, capped = MAX_RATE_LIMIT_RETRIES, backoff_ms = backoff.as_millis(), start, end, "rate limit — tidur lalu coba ulang");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                Err(e) if chunk > 2_000 => {
                    chunk /= 5;
                    backoff = Duration::from_millis(1500);
                    warn!(?e, chunk, "get_logs ditolak — perkecil chunk, coba ulang");
                }
                Err(e) => {
                    warn!(?e, start, end, "get_logs gagal — rentang dilewati");
                    start = end + 1;
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
        self.state.upsert(
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
    pub async fn process_block_logs(&self, from_block: u64, to_block: u64) -> Result<()> {
        self.watch_block(from_block, to_block).await
    }

    /// Handler satu blok / rentang blok. Dipanggil per blok di main loop dan juga
    /// untuk mem-replay rentang yang terlewat saat reconnect WS.
    /// Semua refresh akun dijalankan PARALEL (konkurensi terbatas) agar satu
    /// blok ramai tidak menambahkan latensi RPC beruntun pada jalur kritis
    /// deteksi. Dedup per (market, akun) dilakukan sebelum refresh.
    pub async fn watch_block(&self, from_block: u64, to_block: u64) -> Result<()> {
        let filter = Filter::new()
            .address(self.markets.clone())
            .from_block(from_block)
            .to_block(to_block);
        let logs = self.provider.get_logs(&filter).await?;
        self.process_logs(logs).await
    }

    /// Jalankan refresh akun untuk semua event di dalam daftar log, paralel.
    async fn process_logs(&self, logs: Vec<alloy::rpc::types::Log>) -> Result<()> {
        // Akumulasi (market, account) unik — satu akun bisa terlibat >1 event
        // dalam satu blok; cukup satu refresh per (market, akun).
        let mut pairs = Vec::new();
        for log in logs {
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
                if let Ok(snap) = m.getAccountSnapshot(account).call().await {
                    state.upsert(
                        account,
                        market,
                        crate::state::Position {
                            mtoken_balance: snap.mTokenBalance,
                            borrow_balance: snap.borrowBalance,
                            exchange_rate: snap.exchangeRateMantissa,
                        },
                    );
                }
            });
        }
        while (set.join_next().await).is_some() {}
        Ok(())
    }

    /// Periksa UpdatedPrices di wrapper OEV yang dikonfigurasi. Kembali `true`
    /// bila ada signal ulang scan; lebih baik daripada refresh harga periodik.
    pub async fn check_price_trigger(
        &self,
        block_number: u64,
        wrapper_addresses: &[Address],
    ) -> Result<bool> {
        if wrapper_addresses.is_empty() {
            return Ok(false);
        }
        let filter = Filter::new()
            .address(wrapper_addresses.to_vec())
            .event_signature(UpdatedPrices::SIGNATURE_HASH)
            .from_block(block_number)
            .to_block(block_number);
        let logs = self.provider.get_logs(&filter).await?;
        Ok(!logs.is_empty())
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
