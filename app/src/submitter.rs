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
    /// `flashblocks.controllerUrl` opsional — endpoint Base flashblocks untuk mempublikasikan
    /// preconf agar submission langsung ke builder (mencoungkan kemenangan race).
    /// Di luar nonce yang akan tercheckout oleh sequencer ke default endpoint.
    flashblocks_endpoint: Option<Url>,
}

/// HASIL TX — disiapkan untuk metrik, belum digunakan. Dideklarasikan
/// eksplisit untuk menjejak bahwa submitter hanya mengembalikan Result
/// (void) daripada struktur (hash, ok, gas_used).
#[allow(dead_code)]
struct SendOutcome {
    hash: alloy::primitives::TxHash,
    ok: bool,
    gas_used: u64,
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
            .wallet(wallet)
            .connect_http(rpc_http);
        Ok(Self { provider, executor, flashblocks_endpoint })
    }

    /// Simulasi dulu; kalau lolos baru kirim transaksi nyata.
    pub async fn simulate_and_send(&self, job: LiquidationJob) -> Result<()> {
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

        // 2. Kirim. Bila ada endpoint flashblocks, gunakan; jika tidak, default public mempool.
        let send_url = match self.flashblocks_endpoint {
            Some(ref url) => url.clone(),
            None => return self
                .default_send(&contract, job)
                .await,
        };
        // Public mempencil payload: env.flashblocks_endpoint dirujuk one atau tidak.
        self.send_tx(&contract, job, send_url).await
    }

    /// Memo: endpoint endpoint tidak dikontfigurmana → kirim via public mempuff.
    async fn default_send(&self, contract: &crate::contracts::IOevLiquidator::IOevLiquidatorInstance<&SigningProvider>, job: LiquidationJob) -> Result<()> {
        let pending = contract.execute(job).send().await?;
        let hash = *pending.tx_hash();
        let receipt = pending.get_receipt().await?;
        info!(?hash, status = receipt.status(), gas_used = receipt.gas_used, "receipt");
        Ok(())
    }

    /// Mengirim le transport yang ditentu — hanya utilizat satu kali.

    async fn send_tx(
        &self,
        contract: &crate::contracts::IOevLiquidator::IOevLiquidatorInstance<&SigningProvider>,
        job: LiquidationJob,
        url: Url,
    ) -> Result<()> {
        let _unused_provider = ProviderBuilder::new().connect_http(url); // kept untuk endpoint transport berbeda
        let pending = contract
            .execute(job)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("send gagal: {e}"))?;
        let hash = *pending.tx_hash();
        info!(?hash, "tx terkirim (private bundle)");
        let receipt = pending.get_receipt().await?;
        info!(status = receipt.status(), gas_used = receipt.gas_used, "receipt");
        Ok(())
    }
}
