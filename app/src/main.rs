mod config;
mod contracts;
mod health;
mod hypersync;
mod indexer;
mod state;
mod strategy;
mod submitter;
mod swap;

use crate::config::Config;
use crate::contracts::{IChainlinkOEVWrapper, IComptroller, IMToken, IOevLiquidator, IOracle, LiquidationJob, Mode, COMPTROLLER};
use crate::health::MarketInfo;
use crate::indexer::Indexer;
use crate::state::{AccountState, SharedState};
use crate::strategy::Strategy;
use crate::submitter::Submitter;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use futures::StreamExt;
use std::{collections::HashMap, time::Duration};
use std::str::FromStr;
use tokio::sync::Mutex;
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cfg_path = std::env::var("CONFIG").unwrap_or_else(|_| "config.toml".into());
    let cfg = Config::from_file(&cfg_path)?;
    info!(path = cfg_path, "config dimuat");

    let http = ProviderBuilder::new().connect_http(cfg.base_rpc_http.parse()?);
    let ws = ProviderBuilder::new().connect_ws(WsConnect::new(&cfg.base_rpc_ws)).await?;

    let signer: PrivateKeySigner = cfg.private_key.parse()?;
    let executor = cfg.executor_address()?;

    // Signer harus owner kontrak executor — execute() bertipe onlyOwner.
    // eth_call alloy mengisi `from` = alamat signer; kalau beda, semua
    // simulasi diam-diam revert "not owner". Gagal cepat dengan pesan jelas.
    let executor_contract = IOevLiquidator::new(executor, &http);
    let onchain_owner = executor_contract.owner().call().await?;
    let signer_addr = signer.address();
    anyhow::ensure!(
        onchain_owner == signer_addr,
        "signer {signer_addr:?} bukan owner executor {executor:?} (owner on-chain: {onchain_owner:?})"
    );
    info!(owner = ?signer_addr, executor = ?executor, "kepemilikan executor terverifikasi");

    // Parse whitelist wrapper OEV sekali; errors dicatat tapi tidak fatal.
    let oev_wrappers: Vec<Address> = cfg
        .oev_wrappers
        .iter()
        .filter_map(|s| {
            Address::from_str(s)
                .map_err(|e| warn!(?e, addr = s, "oev_wrappers alamat tidak valid — di-skip"))
                .ok()
        })
        .collect();
    if !oev_wrappers.is_empty() {
        info!(count = oev_wrappers.len(), "wrapper OEV dipantau untuk UpdatedPrices");
    }

    let markets = build_market_map(&http, &cfg).await?;
    info!(count = markets.len(), "market dimuat");

    let snapshot_path = "snapshot.json";
    let loaded = AccountState::load_snapshot(snapshot_path);
    let start_block = loaded.as_ref().map(|(b, _)| *b)
        .unwrap_or_else(|| 0u64);
    let initial = loaded.map(|(_, s)| s).unwrap_or_default();
    let state: SharedState = Arc::new(initial);
    let start_block = if start_block == 0 {
        http
            .get_block_number()
            .await?
            .saturating_sub(cfg.bootstrap_depth_blocks.max(1))
    } else {
        info!(block = start_block, "snapshot dimuat");
        start_block
    };

    // Log event kini dibaca dari Envio HyperSync (alasannya: RPC free-tier
    // memblokir eth_getLogs). eth_call/username tetap lewat RPC HTTP biasa.
    let hyper = crate::hypersync::HyperSync::new(&cfg.hypersync_url, &cfg.hypersync_token).await?;
    let indexer = Indexer::new(http.clone(), hyper, state.clone(), cfg.market_addresses()?)
        .with_refresh_concurrency(cfg.indexer_refresh_concurrency);
    if let Err(e) = indexer.bootstrap_borrowers(start_block).await {
        warn!(?e, "bootstrap gagal — lanjut dengan state kosong");
    }

    let strategy = Arc::new(tokio::sync::Mutex::new(
        Strategy::new(http.clone(), state.clone(), cfg.clone(), markets)?,
    ));
    // Muat harga + param comptroller sekali di start agar close_factor tidak nol
    // di scan pertama.
    if let Err(e) = refresh_prices(&http, &cfg, strategy.clone()).await {
        warn!(?e, "refresh harga awal gagal — params akan 0 sampai refresh berikutnya");
    }
    let submitter = Arc::new(
        Submitter::new(
            cfg.base_rpc_http.parse()?,
            signer,
            executor,
            cfg.flashblocks_endpoint.as_ref().map(|u| u.parse()).transpose()?,
            cfg.max_gas_cost()?,
            cfg.dry_run,
        )
        .await?,
    );

    info!("bot berjalan — memantau blok baru");

    // Submitter non-blocking TUNGGAL untuk seluruh proses: scan reguler & OEV
    // hanya mengirim job lewat channel (tanpa blok); worker pool melakukan
    // simulate + send dengan konkurensi terbatas. Dibuat di sini (di luar
    // loop reconnect) sehingga in-flight submit tidak terpotong saat WS putus.
    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel::<LiquidationJob>();
    {
        let submitter_t = submitter.clone();
        let classic_fallback = cfg.classic_fallback;
        let submitter_concurrency = cfg.submitter_concurrency.max(1);
        tokio::spawn(async move {
            futures::stream::unfold(job_rx, |mut rx| async move {
                rx.recv().await.map(|job| (job, rx))
            })
            .for_each_concurrent(submitter_concurrency, move |job| {
                let s = submitter_t.clone();
                let cfb = classic_fallback;
                async move {
                    if let Err(e) = s.simulate_and_send(job.clone()).await {
                        warn!(?e, "kirim jalur A gagal");
                        if cfb {
                            let mut jb = job;
                            jb.mode = Mode::Classic;
                            if let Err(e2) = s.simulate_and_send(jb).await {
                                warn!(?e2, "kirim jalur B gagal");
                            }
                        }
                    }
                }
            })
            .await;
        });
    }

    // Reconnect WS tanpa batas dengan backoff ringan.
    'outer: loop {
        let stream_result = ws.subscribe_blocks().await;
        let mut stream = match stream_result {
            Ok(s) => s.into_stream(),
            Err(e) => {
                warn!(?e, "gagal connect WS, coba ulang dalam 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue 'outer;
            }
        };

        // Task scan yang lahir untuk koneksi ini — di-join saat reconnect
        // agar tidak leak task.
        let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

        // Gate scan blok reguler: concurrency-1 dan MEMAKAI try-acquire —
        // kalau satu scan sudah berjalan, blok ini dilewati (state segar tetap
        // di-scan pada blok berikutnya), sehingga tidak ada backlog task yang
        // menumpuk menunggu semaphore. Semua input di-snapshot di bawah lock
        // singkat; evaluasi tidak memegang mutex strategy.
        let scan_gate = Arc::new(tokio::sync::Semaphore::new(1));
        // Gate terpisah untuk scan trigger OEV: jalur OEV yang sensitif-waktu
        // punya permit sendiri sehingga TIDAK menunggu scan blok reguler
        // (dan tidak memperebutkan permit yang sama).
        let oev_scan_gate = Arc::new(tokio::sync::Semaphore::new(1));

        // Catat blok terakhir yang diproses agar rentang terlewat
        // (drop WS / lag subscriber) bisa di-replay.
        let mut last_processed: Option<u64> = None;

        while let Some(block) = stream.next().await {
            let number = block.number;
            info!(number, "blok baru");

            // Replay rentang yang terlewat sejak blok terakhir yang diproses.
            if let Some(prev) = last_processed {
                if number > prev + 1 {
                    if let Err(e) = indexer.process_block_logs(prev + 1, number).await {
                        warn!(?e, prev, number, "resync rentang gagal");
                    }
                }
            }

            // Perbarui posisi semua akun dari event di blok ini + deteksi trigger
            // OEV (UpdatedPrices), dalam SATU kueri HyperSync. Nilai kembalian
            // `true` berarti ada trigger OEV pada blok ini.
            let oev_trigger = match indexer.watch_block(number, number, &oev_wrappers).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(?e, number, "gagal memproses log blok");
                    false
                }
            };
            last_processed = Some(number);

            // Trigger wrapper OEV — scan langsung tanpa perlu menunggu refresh 10-blok.
            // Dijalankan non-blocking: tidak men-stall loop blok menunggu scan.
            if oev_trigger && !oev_wrappers.is_empty() {
                if let Err(e) = refresh_prices(&http, &cfg, strategy.clone()).await {
                    warn!(?e, "refresh harga saat trigger OEV gagal");
                }
                let strategy = strategy.clone();
                let http = http.clone();
                let job_tx = job_tx.clone();
                let oev_gate = oev_scan_gate.clone();
                tasks.spawn(async move {
                    // Skip bila scan OEV lain sedang berjalan —
                    // trigger berikutnya akan di-scan pada blok baru.
                    let permit = match oev_gate.try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    spawn_scan(strategy, http, job_tx, permit).await;
                });
            }

            if number % 10 == 0 {
                if let Err(e) = refresh_prices(&http, &cfg, strategy.clone()).await {
                    warn!(?e, "refresh harga gagal");
                }
            }

            // Scan blok ini TANPA memegang mutex strategy: snapshot diambil di
            // bawah lock singkat lalu evaluasi dijalankan paralel di task
            // background; hasil dikirim ke worker submitter. Loop blok tidak
            // menunggu I/O evaluasi.
            {
                let strategy = strategy.clone();
                let http = http.clone();
                let job_tx = job_tx.clone();
                let gate = scan_gate.clone();
                tasks.spawn(async move {
                    // Skip bila satu scan reguler sudah berjalan; blok
                    // berikutnya akan men-scan state yang lebih segar. Ini
                    // mencegah backlog task redundant menumpuk di semaphore.
                    let permit = match gate.try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    let job = strategy.lock().await.scan(http);
                    for j in job.run().await {
                        let _ = job_tx.send(j);
                    }
                    drop(permit);
                });
            }

            // prune borrowers yang sudah tidak punya posisi; workaround sederhana
            // dengan melakukannya tiap 100 blok (sejalan snapshot).
            if number % 100 == 0 {
                if let Err(e) = state.save_snapshot(snapshot_path, number) {
                    warn!(?e, "snapshot gagal");
                }
                state.prune_inactive_borrowers();
            }
        }

        warn!("stream putus — reconnect");

        // Tunggu task scan untuk koneksi ini selesai (sendernya di-drop),
        // agar tidak leak task. Worker submitter global tetap berjalan.
        tokio::time::sleep(Duration::from_secs(2)).await;
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                warn!(?e, "scan task panic");
            }
        }
    }
}

async fn build_market_map<P: Provider>(
    provider: &P,
    cfg: &Config,
) -> Result<HashMap<Address, MarketInfo>> {
    let comptroller = IComptroller::new(COMPTROLLER, provider);
    let oracle_addr = comptroller.oracle().call().await?;
    let oracle = IOracle::new(oracle_addr, provider);

    let mut map = HashMap::new();
    for m in &cfg.markets {
        let mtoken = Address::from_str(&m.mtoken)?;
        let underlying = Address::from_str(&m.underlying)?;
        let cf = comptroller.markets(mtoken).call().await;
        let collateral_factor = cf.map(|r| r.collateralFactorMantissa).unwrap_or_default();
        let price = oracle.getUnderlyingPrice(mtoken).call().await.unwrap_or_default();
        let seize_share = IMToken::new(mtoken, provider)
            .protocolSeizeShareMantissa()
            .call()
            .await
            .ok()
            .unwrap_or_default();
        // Fee liquidator OEV dari wrapper yang terdaftar di oracle (bila ada).
        // Config liquidator_fee_bps hanya dipakai sebagai fallback.
        let oev_fee_bps = match oracle.getFeed(m.symbol.clone()).call().await {
            Ok(feed) if feed != Address::ZERO => {
                match IChainlinkOEVWrapper::new(feed, provider).liquidatorFeeBps().call().await {
                    Ok(fee) => Some(fee as u64),
                    Err(e) => {
                        warn!(?e, symbol = m.symbol, "gagal baca liquidatorFeeBps — pakai config");
                        None
                    }
                }
            }
            _ => None,
        };
        map.insert(
            mtoken,
            MarketInfo {
                underlying,
                symbol: m.symbol.clone(),
                collateral_factor,
                price,
                protocol_seize_share: seize_share,
                oev_fee_bps,
            },
        );
    }
    Ok(map)
}

async fn refresh_prices<P: Provider + Clone>(
    provider: &P,
    cfg: &Config,
    strategy: Arc<Mutex<Strategy<P>>>,
) -> Result<()> {
    let comptroller = IComptroller::new(COMPTROLLER, provider);
    let oracle_addr = comptroller.oracle().call().await?;
    let oracle = IOracle::new(oracle_addr, provider);

    // Muat close factor & liquidation incentive dari chain — jangan hardcode.
    let close_factor = comptroller.closeFactorMantissa().call().await?;
    let incentive = comptroller.liquidationIncentiveMantissa().call().await?;

    // Ambil harga SEMUA market terlebih dahulu di luar kunci — network I/O boleh
    // lambat, sehingga mengambil mutex di dalam loop per-RPC akan men-stall scan.
    let mut prices = Vec::new();
    for m in &cfg.markets {
        let mtoken = Address::from_str(&m.mtoken)?;
        if let Ok(p) = oracle.getUnderlyingPrice(mtoken).call().await {
            prices.push((mtoken, p));
        }
    }

    // Apply cache di dalam kunci tanpa await network.
    let mut s = strategy.lock().await;
    s.update_comptroller_params(close_factor, incentive);
    for (mtoken, price) in prices {
        s.update_price(mtoken, price);
    }
    Ok(())
}

/// Jalankan scan + submit karena trigger UpdatedPrices — terpisah dari task
/// set reguler blok agar scan trigger tidak harus menunggu iterasi blok baru.
/// Hasil disalurkan ke worker submitter via channel (bukan loop blok).
async fn spawn_scan<P: Provider + Clone + Send + Sync + 'static>(
    strategy: Arc<Mutex<Strategy<P>>>,
    provider: P,
    job_tx: tokio::sync::mpsc::UnboundedSender<LiquidationJob>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let job = strategy.lock().await.scan(provider);
    for j in job.run().await {
        let _ = job_tx.send(j);
    }
}
