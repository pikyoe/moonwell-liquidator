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
use std::collections::HashMap;
use std::str::FromStr;
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

    let mut strategy = Strategy::new(http.clone(), state.clone(), cfg.clone(), markets)?;
    let submitter = Submitter::new(cfg.base_rpc_http.parse()?, signer, executor).await?;

    let mut block_stream = ws.subscribe_blocks().await?.into_stream();
    info!("bot berjalan — memantau blok baru");

    while let Some(block) = block_stream.next().await {
        let number = block.number;
        info!(number, "blok baru");

        if number % 10 == 0 {
            if let Err(e) = refresh_prices(&http, &cfg, &mut strategy).await {
                warn!(?e, "refresh harga gagal");
            }
        }

        match strategy.scan_opportunities().await {
            Ok(jobs) => {
                for job in jobs {
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
                }
            }
            Err(e) => warn!(?e, "scan gagal"),
        }

        if number % 100 == 0 {
            let _ = state.save_snapshot(snapshot_path, number);
        }
    }

    Ok(())
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
    strategy: &mut Strategy<P>,
) -> Result<()> {
    let comptroller = IComptroller::new(COMPTROLLER, provider);
    let oracle_addr = comptroller.oracle().call().await?;
    let oracle = IOracle::new(oracle_addr, provider);
    for m in &cfg.markets {
        let mtoken = Address::from_str(&m.mtoken)?;
        if let Ok(p) = oracle.getUnderlyingPrice(mtoken).call().await {
            strategy.update_price(mtoken, p);
        }
    }
    Ok(())
}
