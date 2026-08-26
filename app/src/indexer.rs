use crate::contracts::IMToken;
use crate::state::SharedState;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Result;
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
}

impl<P: Provider + Clone> Indexer<P> {
    pub fn new(provider: P, state: SharedState, markets: Vec<Address>) -> Self {
        Self { provider, state, markets }
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
        while start <= latest {
            let end = (start + chunk).min(latest);
            let filter = Filter::new()
                .address(self.markets.clone())
                .event_signature(Borrow::SIGNATURE_HASH)
                .from_block(start)
                .to_block(end);
            match self.provider.get_logs(&filter).await {
                Ok(logs) => {
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
                Err(e) if chunk > 2_000 => {
                    chunk /= 5;
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
    pub async fn watch_block(&self, from_block: u64, to_block: u64) -> Result<()> {
        let filter = Filter::new()
            .address(self.markets.clone())
            .from_block(from_block)
            .to_block(to_block);
        let logs = self.provider.get_logs(&filter).await?;
        for log in logs {
            let market = log.address();
            let topics = log.topics();
            if topics.len() >= 3 && topics[0] == Transfer::SIGNATURE_HASH {
                if let Ok(ev) = Transfer::decode_log(&log.inner) {
                    // Mint/burn menghasilkan transfer dengan alamat nol — jangan refresh.
                    if ev.from != Address::ZERO {
                        let _ = self.refresh_account(market, ev.from).await;
                    }
                    if ev.to != Address::ZERO {
                        let _ = self.refresh_account(market, ev.to).await;
                    }
                }
            } else if let Ok(ev) = Mint::decode_log(&log.inner) {
                let _ = self.refresh_account(market, ev.minter).await;
            } else if let Ok(ev) = Redeem::decode_log(&log.inner) {
                let _ = self.refresh_account(market, ev.redeemer).await;
            } else if let Ok(ev) = Borrow::decode_log(&log.inner) {
                let _ = self.refresh_account(market, ev.borrower).await;
            } else if let Ok(ev) = RepayBorrow::decode_log(&log.inner) {
                let _ = self.refresh_account(market, ev.borrower).await;
            } else if let Ok(ev) = LiquidateBorrow::decode_log(&log.inner) {
                let _ = self.refresh_account(market, ev.borrower).await;
            }
        }
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
