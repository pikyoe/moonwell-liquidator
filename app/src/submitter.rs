use crate::contracts::{IOevLiquidator, LiquidationJob};
use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::providers::fillers::{FillProvider, JoinFill, WalletFiller};
use alloy::providers::{Identity, ProviderBuilder, RootProvider};
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
    ) -> Result<Self> {
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new()
            .wallet(wallet.clone())
            .connect_http(rpc_http);

        // Bila endpoint private diisi, bangun provider terpisah dengan wallet yang sama.
        let flashblocks = flashblocks_endpoint.map(|url| {
            ProviderBuilder::new().wallet(wallet).connect_http(url)
        });

        Ok(Self { provider, executor, flashblocks, in_flight: std::sync::Arc::new(dashmap::DashSet::new()) })
    }

    /// Simulasi dulu; kalau lolos baru kirim transaksi nyata.
    pub async fn simulate_and_send(&self, job: LiquidationJob) -> Result<()> {
        let key = (job.borrower, job.mTokenLoan);
        // Skip bila likuidasi identik sedang in-flight (trigger OEV + scan).
        if !self.in_flight.insert(key) {
            tracing::debug!(?key, "job sedang diproses — duplikat di-skip");
            return Ok(());
        }
        let _guard = InFlightGuard { set: self.in_flight.clone(), key };

        let contract = IOevLiquidator::new(self.executor, &self.provider);

        // 1. eth_call simulasi — skip bila gagal.
        let sim = contract.execute(job.clone()).call().await;
        match sim {
            Ok(_) => info!("simulasi lolos"),
            Err(e) => {
                warn!(?e, "simulasi revert — job dibatalkan");
                return Ok(());
            }
        }

        // 2. Kirim. Bila ada provider flashblocks, kirim lewat endpoint private;
        //    kalau tidak, mempool publik.
        match self.flashblocks {
            Some(ref fb_provider) => {
                let fb_contract = IOevLiquidator::new(self.executor, fb_provider);
                let pending = fb_contract.execute(job).send().await?;
                let hash = *pending.tx_hash();
                info!(?hash, "tx terkirim (private bundle)");
                let receipt = pending.get_receipt().await?;
                info!(status = receipt.status(), gas_used = receipt.gas_used, "receipt");
            }
            None => {
                let pending = contract.execute(job).send().await?;
                let hash = *pending.tx_hash();
                info!(?hash, "tx terkirim (mempool publik)");
                let receipt = pending.get_receipt().await?;
                info!(status = receipt.status(), gas_used = receipt.gas_used, "receipt");
            }
        }
        Ok(())
    }
}
