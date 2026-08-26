use alloy::primitives::Address;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub base_rpc_http: String,
    pub base_rpc_ws: String,
    /// Optional: endpoint Base Flashblocks (mainnet) untuk pengiriman privat,
    /// menghindari race dengan mempool publik. Contoh mainnet:
    /// "https://mainnet.flashblocks.base.org" (atau endpoint provider privat).
    pub flashblocks_endpoint: Option<String>,
    /// Optional: whitelist alamat ChainlinkOEVWrapper yang diperiksa untuk event
    /// UpdatedPrices. Bila kosong, trigger OEV di-nonaktifkan (selalu false).
    #[serde(default)]
    pub oev_wrappers: Vec<String>,
    pub private_key: String,
    pub executor_address: String,
    /// Batas posisi maksimum dalam USD (wei 1e18). Default $25.000.
    #[serde(default = "default_max_position")]
    pub max_position_usd: u64,
    /// Minimum profit GLOBAL dalam unit token HASIL AKHIR (loan token bila
    /// swap aktif, kolateral bila tidak) agar tx dikirim. Dipertahankan untuk
    /// kompatibilitas config lama; lebih baik gunakan `min_profit_per_symbol`.
    #[serde(default)]
    pub min_profit_wei: String,
    /// Minimum profit per simbol market (unit wei token hasil akhir), mengatasi
    /// perbedaan desimal antar token (WETH 18, USDC 6, cbBTC 8). Contoh:
    ///   min_profit_per_symbol = { WETH = "1000000000000000", USDC = "300000" }
    /// Simbol yang tidak tercantum akan pakai `min_profit_wei`.
    #[serde(default)]
    pub min_profit_per_symbol: std::collections::HashMap<String, String>,
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
    /// Hanya dokumentasi di config (semua harga oracle sudah 1e36-normalized,
    /// sehingga kalkulasi tidak memakai field ini). Dipertahankan agar
    /// config.toml existing tetap valid.
    #[allow(dead_code)]
    pub decimals: u8,
}

impl Config {
    /// Baca file dan validasi semua field yang bisa gagal di runtime —
    /// lebih baik startup gagal cepat dengan pesan jelas daripada panic
    /// di tengah scan loop.
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&raw)?;
        cfg.min_profit()?;
        cfg.validate_min_profit_map()?;
        cfg.executor_address()?;
        cfg.market_addresses()?;
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

    /// Ambang profit untuk simbol market tertentu: override dari
    /// `min_profit_per_symbol` bila ada, fallback ke `min_profit_wei`.
    /// Gagal parse -> Err (sudah divalidasi di from_file; ini defensif).
    pub fn min_profit_for_symbol(&self, symbol: &str) -> anyhow::Result<alloy::primitives::U256> {
        match self.min_profit_per_symbol.get(symbol) {
            Some(raw) => raw.parse().map_err(|_| {
                anyhow::anyhow!("min_profit_per_symbol[{symbol}] tidak bisa di-parse: {raw}")
            }),
            None => self.min_profit(),
        }
    }

    /// Validasi semua entri per-symbol saat startup agar tidak ada panic/Err
    /// baru di tengah scan loop.
    fn validate_min_profit_map(&self) -> anyhow::Result<()> {
        for (sym, raw) in &self.min_profit_per_symbol {
            raw.parse::<alloy::primitives::U256>().map_err(|_| {
                anyhow::anyhow!("min_profit_per_symbol[{sym}] tidak bisa di-parse: {raw}")
            })?;
        }
        Ok(())
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


#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg(min_profit_wei: &str) -> Config {
        toml::from_str(&format!(
            r#"
base_rpc_http = "https://x"
base_rpc_ws = "wss://x"
private_key = "0x"
executor_address = "0x0000000000000000000000000000000000000001"
min_profit_wei = "{min_profit_wei}"
"#
        ))
        .unwrap()
    }

    #[test]
    fn min_profit_valid() {
        let cfg = base_cfg("1000000000000000");
        assert_eq!(cfg.min_profit().unwrap(), alloy::primitives::U256::from(10u64.pow(15)));
    }

    #[test]
    fn min_profit_malformed_error_bukan_panic() {
        let cfg = base_cfg("bukan-angka");
        assert!(cfg.min_profit().is_err());
    }

    #[test]
    fn min_profit_per_symbol_override_dan_fallback() {
        let mut cfg = base_cfg("1000");
        cfg.min_profit_per_symbol
            .insert("USDC".into(), "300000".into());
        assert_eq!(
            cfg.min_profit_for_symbol("USDC").unwrap(),
            alloy::primitives::U256::from(300_000u64)
        );
        // simbol tak terdaftar jatuh ke global
        assert_eq!(
            cfg.min_profit_for_symbol("WETH").unwrap(),
            alloy::primitives::U256::from(1_000u64)
        );
    }

    #[test]
    fn min_profit_per_symbol_malformed_error() {
        let mut cfg = base_cfg("1000");
        cfg.min_profit_per_symbol
            .insert("WETH".into(), "bukan-angka".into());
        assert!(cfg.min_profit_for_symbol("WETH").is_err());
    }
}
