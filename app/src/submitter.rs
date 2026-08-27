use crate::contracts::{IOevLiquidator, LiquidationJob};
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::fillers::{FillProvider, JoinFill, WalletFiller};
use alloy::providers::{Identity, Provider, ProviderBuilder, RootProvider};
use alloy::providers::fillers::{
    BlobGasFiller, ChainIdFiller, GasFiller, NonceFiller,
};
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::http::reqwest::Url;
use anyhow::Result;
use tracing::{info, warn};

/// Provider yang sudah dilengkapi wallet (penandatangan otomatis).
pub type SigningProvider = FillProvider<
    JoinFill<
        JoinFill<Identity, JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>>,
        WalletFiller<EthereumWallet>,
    >,
    RootProvider,
>;

pub struct Submitter {
    provider: SigningProvider,
    executor: Address,
    /// Provider khusus endpoint private (Base Flashblocks). Dibangun sekali di
    /// `new` bila `flashblocks_endpoint` diisi — sehingga send benar-benar
    /// melewati endpoint itu, bukan mempool publik.
    flashblocks: Option<SigningProvider>,
    /// Dedup submission in-flight per (borrower, mTokenLoan) — mencegah dua
    /// task (scan reguler + trigger OEV) mengirim likuidasi yang sama
    /// bersamaan dan membuang gas.
    in_flight: std::sync::Arc<dashmap::DashSet<(Address, Address)>>,
    /// Batas biaya gas per tx (wei ETH). Simulasi profit yang lolos tetap
    /// bisa rugi bersih bila gas mahal — tx di atas batas ini tidak dikirim.
    max_gas_cost: U256,
    /// Mode dry-run: simulasi eth_call + estimasi gas tetap dijalankan, tapi
    /// transaksi tidak dikirim. Dipakai untuk analisa perilaku tanpa dana.
    dry_run: bool,
    /// Tip eksplisit per gas (wei) untuk tx bersaing di mempool. `None` =
    /// pakai rekomendasi node (eth_maxPriorityFeePerGas).
    priority_fee: Option<u128>,
}

/// Hasil satu percobaan submit. Dibedakan dari `Err` (transport/infra)
/// agar fallback Classic bisa dipicu SAAT simulasi OEV revert — kasus paling
/// umum kegagalan — bukan hanya saat RPC error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// eth_call revert (posisi berubah, wrapper tidak punya fungsi, dll).
    Reverted,
    /// Estimasi gas / biaya melebihi batas, gas price tidak terbaca.
    SkippedBudget,
    /// Dry-run: simulasi & guard gas lolos, tx tidak dikirim.
    DryRunOk,
    /// Transaksi dikirim (receipt diterima).
    Sent,
}

/// RAII guard: hapus key dedup saat scope submit berakhir (sukses maupun error).
struct InFlightGuard {
    set: std::sync::Arc<dashmap::DashSet<(Address, Address)>>,
    key: (Address, Address),
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.set.remove(&self.key);
    }
}

impl Submitter {
    pub async fn new(
        rpc_http: Url,
        signer: PrivateKeySigner,
        executor: Address,
        flashblocks_endpoint: Option<Url>,
        max_gas_cost: U256,
        dry_run: bool,
        priority_fee: Option<u128>,
    ) -> Result<Self> {
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new()
            .wallet(wallet.clone())
            .connect_http(rpc_http);

        // Bila endpoint private diisi, bangun provider terpisah dengan wallet yang sama.
        let flashblocks = flashblocks_endpoint.map(|url| {
            ProviderBuilder::new().wallet(wallet).connect_http(url)
        });

        Ok(Self { provider, executor, flashblocks, in_flight: std::sync::Arc::new(dashmap::DashSet::new()), max_gas_cost, dry_run, priority_fee })
    }

    /// Simulasi dulu; kalau lolos baru kirim transaksi nyata.
    /// Kembalian `SendOutcome` membedakan transport-error (`Err`) dari hasil
    /// fungsional (revert/budget/dry-run/terkirim) supaya main bisa memutuskan
    /// kapan menjalankan fallback Classic.
    pub async fn simulate_and_send(&self, job: LiquidationJob) -> Result<SendOutcome> {
        let key = (job.borrower, job.mTokenLoan);
        // Skip bila likuidasi identik sedang in-flight (trigger OEV + scan).
        if !self.in_flight.insert(key) {
            tracing::debug!(?key, "job sedang diproses — duplikat di-skip");
            return Ok(SendOutcome::DryRunOk);
        }
        let _guard = InFlightGuard { set: self.in_flight.clone(), key };

        let contract = IOevLiquidator::new(self.executor, &self.provider);

        // 1. eth_call simulasi — kalau revert, laporkan Reverted agar caller
        //    bisa mencoba jalur Classic (fallback).
        let sim = contract.execute(job.clone()).call().await;
        match sim {
            Ok(_) => info!("simulasi lolos"),
            Err(e) => {
                warn!(?e, "simulasi revert — job dibatalkan");
                return Ok(SendOutcome::Reverted);
            }
        }

        // 2. Guard biaya gas: estimasi gas x gas price tidak boleh melebihi
        //    batas — profit on-chain tidak menghitung ETH yang keluar untuk gas.
        match contract.execute(job.clone()).estimate_gas().await {
            Ok(gas) => {
                // Gas price gagal dibaca -> batalkan. unwrap_or(0) akan
                // meloloskan guard tepat saat harga gas tidak diketahui.
                let gas_price = match self.provider.get_gas_price().await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(?e, "gagal baca gas price — job dibatalkan");
                        return Ok(SendOutcome::SkippedBudget);
                    }
                };
                let cost = U256::from(gas) * U256::from(gas_price);
                if cost > self.max_gas_cost {
                    warn!(gas, gas_price, %cost, "estimasi gas melebihi batas — job dibatalkan");
                    return Ok(SendOutcome::SkippedBudget);
                }
            }
            Err(e) => {
                warn!(?e, "estimasi gas gagal — job dibatalkan");
                return Ok(SendOutcome::SkippedBudget);
            }
        }

        // 3. Kirim — KECUALI mode dry_run: simulasi & guard gas tetap jalan,
        //    tapi tidak ada tx yang dikirim (hanya lapor bahwa job siap kirim).
        if self.dry_run {
            info!("dry-run: simulasi lolos, tx TIDAK dikirim");
            return Ok(SendOutcome::DryRunOk);
        }

        // Bila ada provider flashblocks, kirim lewat endpoint private;
        // kalau tidak, mempool publik. Tip prioritas eksplisit diterapkan
        // pada CALL BUILDER (sebelum .send()) sehingga GasFiller tidak
        // menimpa fee yang sudah kita set. Match inline (bukan helper)
        // agar type inference alloy bisa menyimpulkan P/D yang eksak.
        match self.flashblocks {
            Some(ref fb_provider) => {
                let fb_contract = IOevLiquidator::new(self.executor, fb_provider);
                let pending = match self.priority_fee {
                    Some(fee) => fb_contract.execute(job).max_priority_fee_per_gas(fee).send().await?,
                    None => fb_contract.execute(job).send().await?,
                };
                let hash = *pending.tx_hash();
                info!(?hash, "tx terkirim (private bundle)");
                let receipt = pending.get_receipt().await?;
                info!(status = receipt.status(), gas_used = receipt.gas_used, "receipt");
            }
            None => {
                let pending = match self.priority_fee {
                    Some(fee) => contract.execute(job).max_priority_fee_per_gas(fee).send().await?,
                    None => contract.execute(job).send().await?,
                };
                let hash = *pending.tx_hash();
                info!(?hash, "tx terkirim (mempool publik)");
                let receipt = pending.get_receipt().await?;
                info!(status = receipt.status(), gas_used = receipt.gas_used, "receipt");
            }
        }
        Ok(SendOutcome::Sent)
    }
}
