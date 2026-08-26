mod config;
mod contracts;
mod health;
mod indexer;
mod state;
mod strategy;
mod submitter;
mod swap;

use crate::config::Config;
use crate::contracts::{IComptroller, IOracle, Mode, COMPTROLLER};
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

    let markets = build_market_map(&http, &cfg).await?;
    info!(count = markets.len(), "market dimuat");

    let snapshot_path = "snapshot.json";
    let loaded = AccountState::load_snapshot(snapshot_path);
    let start_block = loaded.as_ref().map(|(b, _)| *b)
        .unwrap_or_else(|| 0u64);
    let initial = loaded.map(|(_, s)| s).unwrap_or_default();
    let state: SharedState = Arc::new(initial);
    let start_block = if start_block == 0 {
        http.get_block_number().await?.saturating_sub(500_000)
    } else {
        info!(block = start_block, "snapshot dimuat");
        start_block
    };

    let indexer = Indexer::new(http.clone(), state.clone(), cfg.market_addresses()?);
    if let Err(e) = indexer.bootstrap_borrowers(start_block).await {
        warn!(?e, "bootstrap gagal — lanjut dengan state kosong");
    }

    let strategy = Arc::new(tokio::sync::Mutex::new(
        Strategy::new(http.clone(), state.clone(), cfg.clone(), markets)?,
    ));
    let submitter = Arc::new(Submitter::new(cfg.base_rpc_http.parse()?, signer, executor).await?);

    info!("bot berjalan — memantau blok baru");

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

        // Submitter non-blocking: enqueue di sini, join di akhir.
        let mut tasks = tokio::task::JoinSet::new();

        while let Some(block) = stream.next().await {
            let number = block.number;
            info!(number, "blok baru");

            if number % 10 == 0 {
                if let Err(e) = refresh_prices(&http, &cfg, strategy.clone()).await {
                    warn!(?e, "refresh harga gagal");
                }
            }

            // clone config untuk digunakan dalam task
            let cfg = cfg.clone();
            let strategy = strategy.clone();
            let submitter = submitter.clone();

            match strategy.lock().await.scan_opportunities().await {
                Ok(jobs) => {
                    for job in jobs {
                        let submitter = submitter.clone();
                        let cfg = cfg.clone();
                        tasks.spawn(async move {
                            if let Err(e) = submitter.simulate_and_send(job.clone()).await {
                                warn!(?e, "kirim jalur A gagal");
                                if cfg.classic_fallback {
                                    let mut jb = job;
                                    jb.mode = Mode::Classic;
                                    if let Err(e2) = submitter.simulate_and_send(jb).await {
                                        warn!(?e2, "kirim jalur B gagal");
                                    }
                                }
                            }
                        });
                    }
                }
                Err(e) => warn!(?e, "scan gagal"),
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
        tokio::time::sleep(Duration::from_secs(2)).await;

        // tunggu task submitter sebelum reconnect untuk menghindari leak
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                warn!(?e, "submitter task panic");
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
        map.insert(
            mtoken,
            MarketInfo {
                mtoken,
                underlying,
                symbol: m.symbol.clone(),
                decimals: m.decimals,
                collateral_factor,
                price,
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
    let mut s = strategy.lock().await;
    for m in &cfg.markets {
        let mtoken = Address::from_str(&m.mtoken)?;
        if let Ok(p) = oracle.getUnderlyingPrice(mtoken).call().await {
            s.update_price(mtoken, p);
        }
    }
    Ok(())
}
