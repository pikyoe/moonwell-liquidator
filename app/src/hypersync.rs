//! Pengganti `eth_getLogs` berbasis Envio HyperSync.
//!
//! RPC free-tier umumnya memblokir `eth_getLogs`. HyperSync adalah query layer
//! yang menuai event log tanpa JSON-RPC, sehingga bot tetap bisa memantau event
//! Borrow/Mint/Repay/Transfer/Liquidate/UpdatedPrices yang dibutuhkan indexer.
//!
//! Module ini hanya menghasilkan data log mentah; decoding event tetap memakai
//! `alloy::sol!` yang sudah ada (lihat `indexer.rs`).

use alloy::primitives::{Bytes, B256};
use alloy::rpc::types::Log as RpcLog;
use hypersync_client::net_types::{LogField, LogFilter, Query};
use hypersync_client::Client;

/// Konversi daftar `alloy::Address` menjadi matcher `LogFilter` (filter
/// alamat). Dipakai bersama oleh jalur log market, log OEV, dan `has_log`.
fn address_matcher(addrs: &[alloy::primitives::Address]) -> anyhow::Result<LogFilter> {
    let mut f = LogFilter::all();
    if !addrs.is_empty() {
        let arr: Vec<[u8; 20]> = addrs.iter().map(|a| (*a).into_array()).collect();
        f = f.and_address(arr)?;
    }
    Ok(f)
}

/// Konversi `B256` (event signature) menjadi daftar byte untuk `and_topic0`.
fn topic0_arr(t: &B256) -> [u8; 32] {
    let mut a = [0u8; 32];
    a.copy_from_slice(t.as_slice());
    a
}

/// Reader HyperSync — query log mentah per rentang blok.
///
/// Mengembalikan `alloy::rpc::types::Log` agar `process_logs` di indexer.rs
/// tidak perlu diubah (decode event tetap via `SolEvent::decode_log(&log.inner)`).
#[derive(Clone)]
pub struct HyperSync {
    client: Client,
}

impl HyperSync {
    pub async fn new(url: &str, token: &str) -> anyhow::Result<Self> {
        // Token wajib sejak Nov 2025. Tolak cepat bila kosong sehingga bot
        // TIDAK berjalan sebagai silent no-op (tanpa pelacakan posisi/OEV).
        if token.trim().is_empty() {
            anyhow::bail!(
                "hypersync_token wajib diisi (wajib sejak Nov 2025). \
                 Buat token di dashboard Envio dan isi di config `hypersync_token` \
                 atau env ENVIO_API_TOKEN. Endpoint: {url}"
            );
        }
        let client = Client::builder()
            .url(url)
            .api_token(token.to_string())
            .build()
            .map_err(|e| anyhow::anyhow!("gagal init HyperSync client: {e}"))?;
        let hyper = Self { client };
        // Verifikasi konektivitas + keabsahan token AKTIF: query blok 0-1.
        // Gagal di sini => token/URL salah => fail-fast, bukan silent no-op.
        hyper.ping().await?;
        Ok(hyper)
    }

    /// Query minimal (blok 0-1, tanpa filter) untuk memastikan endpoint dan
    /// token valid. Dipanggil sekali saat startup.
    async fn ping(&self) -> anyhow::Result<()> {
        let query = Query::new()
            .from_block(0)
            .to_block_excl(1)
            .where_logs(LogFilter::all())
            .select_log_fields([LogField::BlockNumber]);
        let res = self
            .client
            .get(&query)
            .await
            .map_err(|e| anyhow::anyhow!("verifikasi token HyperSync gagal — periksa hypersync_url/token: {e}"))?;
        let _ = res.data.logs.len();
        Ok(())
    }

    /// Ambil log dari `addresses` pada rentang [from, to]. Bila `topic0`
    /// diberikan, hanya kembalikan log dengan event signature tsb (mis. hanya
    /// `Borrow` saat bootstrap). Bila `None`, kembalikan SEMUA log — dipakai
    /// `watch_block` (Transfer/Mint/Redeem/Borrow/Repay/Liquidate).
    /// Pagination ditangani via `next_block`.
    pub async fn get_logs_raw(
        &self,
        addresses: &[alloy::primitives::Address],
        from: u64,
        to: u64,
        topic0: Option<&B256>,
    ) -> anyhow::Result<Vec<RpcLog>> {
        let mut matcher = address_matcher(addresses)?;
        if let Some(topic0) = topic0 {
            matcher = matcher.and_topic0([topic0_arr(topic0)])?;
        }
        let mut query = Query::new().from_block(from).where_logs(matcher);
        query = query.select_log_fields([
            LogField::Address,
            LogField::Data,
            LogField::Topic0,
            LogField::Topic1,
            LogField::Topic2,
            LogField::Topic3,
            LogField::BlockNumber,
            LogField::Removed,
        ]);
        let to_excl = to.saturating_add(1);

        let mut out = Vec::new();
        let mut q = query.to_block_excl(to_excl);
        let mut prev_from = from;
        loop {
            let res = self.client.get(&q).await?;
            for batch in &res.data.logs {
                for log in batch {
                    if let Some(rpc_log) = convert(log) {
                        out.push(rpc_log);
                    }
                }
            }
            // Berhenti bila sudah melampaui blok tujuan (pagination selesai).
            if res.next_block >= to_excl {
                break;
            }
            // Guard tanpa-laju: bila HyperSync belum mengindeks tip (blok yang
            // baru tiba via WS belum tersedia), `next_block` tidak maju melewati
            // awal rentang. Putus alih-alih busy-loop men-hammer endpoint.
            if res.next_block <= prev_from {
                break;
            }
            prev_from = res.next_block;
            q = q.clone().from_block(res.next_block);
        }
        Ok(out)
    }

    /// Satu kueri untuk seluruh blok: (1) log semua market untuk refresh posisi
    /// DAN (2) log `UpdatedPrices` dari wrapper OEV untuk trigger scan.
    /// Menghemat menjadi SATU request per blok (bukan 2) — penting agar berada
    /// di bawah batas RPM plan.
    ///
    /// Mengembalikan `(market_logs, oev_trigger)` — `oev_trigger` `true` bila
    /// ada event UpdatedPrices pada blok tsb dari salah satu `oev_addresses`.
    pub async fn market_and_trigger(
        &self,
        market_addrs: &[alloy::primitives::Address],
        oev_addrs: &[alloy::primitives::Address],
        oev_topic0: &B256,
        from: u64,
        to: u64,
    ) -> anyhow::Result<(Vec<RpcLog>, bool)> {
        let market_matcher = address_matcher(market_addrs)?;
        let matcher = if !oev_addrs.is_empty() {
            let oev = address_matcher(oev_addrs)?.and_topic0([topic0_arr(oev_topic0)])?;
            // OR: log semua event market ATAU event UpdatedPrices dari wrapper OEV.
            market_matcher.or(oev)
        } else {
            market_matcher.into()
        };
        let mut query = Query::new().from_block(from).where_logs(matcher);
        query = query.select_log_fields([
            LogField::Address,
            LogField::Data,
            LogField::Topic0,
            LogField::Topic1,
            LogField::Topic2,
            LogField::Topic3,
            LogField::BlockNumber,
            LogField::Removed,
        ]);
        let to_excl = to.max(from).saturating_add(1);
        query = query.to_block_excl(to_excl);

        let mut market_logs: Vec<RpcLog> = Vec::new();
        let mut oev_trigger = false;
        let mut prev_from = from;
        loop {
            let res = self.client.get(&query).await?;
            for batch in &res.data.logs {
                for log in batch {
                    if let Some(rpc_log) = convert(log) {
                        // Log dari alamat market → refresh posisi. Log lain yang
                        // cocok (alamat wrapper OEV) → sinyal UpdatedPrices.
                        if market_addrs.contains(&rpc_log.address()) {
                            market_logs.push(rpc_log);
                        } else {
                            oev_trigger = true;
                        }
                    }
                }
            }
            if res.next_block >= to_excl {
                break;
            }
            // Guard tanpa-laju — lihat get_logs_raw.
            if res.next_block <= prev_from {
                break;
            }
            prev_from = res.next_block;
            query = query.clone().from_block(res.next_block);
        }
        Ok((market_logs, oev_trigger))
    }
}

fn convert(log: &hypersync_client::simple_types::Log) -> Option<RpcLog> {
    let address = log
        .address
        .as_ref()
        .map(|a| alloy::primitives::Address::from_slice(a.as_ref()));
    let address = address?;

    let mut topics: Vec<B256> = Vec::new();
    for t in log.topics.iter().flatten() {
        let slice: &[u8] = t.as_ref();
        if slice.is_empty() {
            break;
        }
        topics.push(B256::from_slice(slice));
    }
    if topics.is_empty() {
        return None;
    }

    let data = Bytes::copy_from_slice(
        log.data.as_ref().map_or(&[][..], |d| d.as_ref()),
    );
    Some(RpcLog {
        inner: alloy::primitives::Log::new_unchecked(address, topics, data),
        block_hash: None,
        block_number: log.block_number.map(|b| *b),
        block_timestamp: None,
        transaction_hash: None,
        transaction_index: None,
        log_index: None,
        removed: log.removed.unwrap_or(false),
    })
}