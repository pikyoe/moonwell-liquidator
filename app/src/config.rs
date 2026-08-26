use alloy::primitives::Address;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub base_rpc_http: String,
    pub base_rpc_ws: String,
    /// Optional: endpoint Base Flashblocks untuk mengirim bundle secara privat
    /// (menghindari race dengan mempool). Contoh: "https://sepolia-preconf.flashbots.net"
    pub flashblocks_endpoint: Option<String>,
    pub private_key: String,
    pub executor_address: String,
    /// Batas posisi maksimum dalam USD (wei 1e18). Default $25.000.
    #[serde(default = "default_max_position")]
    pub max_position_usd: u64,
    /// Minimum profit (dalam unit collateral token wei) agar tx dikirim.
    #[serde(default)]
    pub min_profit_wei: String,
    /// Aktifkan jalur B (classic) sebagai fallback.
    #[serde(default = "default_true")]
    pub classic_fallback: bool,
    /// Swap aktif (opsional). Router & calldata diisi di strategi.
    #[serde(default)]
    pub swap: SwapConfig,
    /// Daftar mToken yang dipantau. Diisi dari Moonwell docs.
    #[serde(default)]
    pub markets: Vec<MarketConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SwapConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Alamat router Aerodrome. Kosongkan = pakai default AERODROME_ROUTER.
    #[serde(default)]
    pub router: String,
    /// Slippage swap dalam basis point (default 200 = 2%).
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u64,
    /// Bagian profit untuk liquidator di jalur OEV (default 3000 = 30%).
    #[serde(default = "default_liquidator_fee_bps")]
    pub liquidator_fee_bps: u64,
}

fn default_slippage_bps() -> u64 {
    200
}
fn default_liquidator_fee_bps() -> u64 {
    3000
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketConfig {
    pub symbol: String,
    pub mtoken: String,
    pub underlying: String,
    pub decimals: u8,
}

impl Config {
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&raw)?;
        Ok(cfg)
    }

    pub fn executor_address(&self) -> anyhow::Result<Address> {
        Address::from_str(&self.executor_address)
            .map_err(|e| anyhow::anyhow!("executor_address tidak valid: {e}"))
    }

    /// min_profit harus di-parse dengan benar. Argument ini ada di LiquidationJob;
    /// nilai 0 = terima untung nol tanpa sadar.
    pub fn min_profit(&self) -> anyhow::Result<alloy::primitives::U256> {
        self.min_profit_wei
            .parse()
            .map_err(|_| anyhow::anyhow!(
                "min_profit_wei tidak bisa di-parse: {}",
                self.min_profit_wei
            ))
    }

    pub fn market_addresses(&self) -> anyhow::Result<Vec<Address>> {
        self.markets
            .iter()
            .map(|m| Address::from_str(&m.mtoken).map_err(|e| anyhow::anyhow!("mtoken {}: {e}", m.symbol)))
            .collect()
    }
}

fn default_max_position() -> u64 {
    25_000
}
fn default_true() -> bool {
    true
}
