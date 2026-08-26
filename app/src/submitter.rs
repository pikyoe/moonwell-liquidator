use crate::contracts::{IOevLiquidator, LiquidationJob};
use alloy::network::EthereumWallet;
use alloy::primitives::Address;
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
}

impl Submitter {
    pub async fn new(rpc_http: Url, signer: PrivateKeySigner, executor: Address) -> Result<Self> {
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(rpc_http);
        Ok(Self { provider, executor })
    }

    /// Simulasi dulu; kalau lolos baru kirim transaksi nyata.
    pub async fn simulate_and_send(&self, job: LiquidationJob) -> Result<()> {
        let contract = IOevLiquidator::new(self.executor, &self.provider);

        // 1. eth_call simulasi
        let sim = contract.execute(job.clone()).call().await;
        match sim {
            Ok(_) => info!("simulasi lolos"),
            Err(e) => {
                warn!(?e, "simulasi revert — job dibatalkan");
                return Ok(());
            }
        }

        // 2. kirim transaksi
        let pending = contract.execute(job).send().await?;
        let tx_hash = *pending.tx_hash();
        info!(?tx_hash, "tx terkirim");

        let receipt = pending.get_receipt().await?;
        info!(status = receipt.status(), gas_used = receipt.gas_used, "receipt");
        Ok(())
    }
}
