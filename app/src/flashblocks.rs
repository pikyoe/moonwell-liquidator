use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::{Log, Transaction};
use alloy::sol_types::SolCall
use anyhow::Result;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxIntent {
    Liquidation {
        borrower: Address,
        liquidator: Address,
        repay_amount: U256,
        m_token_collateral: Address,
    },
    OevLiquidation {
        borrower: Address,
        liquidator: Address,
        repay_amount: U256,
        m_token_collateral: Address,
        m_token_loan: Address,
    },
    Activity,
}

#[derive(Debug, Clone)]
pub enum FastSignal {
    Log {
        address: Address,
        topic0: B256,
        tx_hash: B256,
    },
    Tx {
        from: Address,
        to: Option<Address>,
        input: Vec<u8>,
        intent: Option<TxIntent>,
    },
}

impl FastSignal {
    pub fn topic0(&self) -> Option<B256> {
        match self {
            FastSignal::Log { topic0, .. } => Some(*topic0),
            FastSignal::Tx { .. } => None,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            FastSignal::Log { .. } => "flashblock-log",
            FastSignal::Tx { .. } => "mempool-tx",
        }
    }
    pub fn borrower(&self) -> Option<Address> {
        match self {
            FastSignal::Tx {
                intent: Some(TxIntent::Liquidation { borrower, .. }),
                ..
            }
            | FastSignal::Tx {
                intent: Some(TxIntent::OevLiquidation { borrower, .. }),
                ..
            } => Some(*borrower),
            _ => None,
        }
    }
    pub fn is_competitor_liquidation(&self) -> bool {
        matches!(
            self,
            FastSignal::Tx {
                intent: Some(TxIntent::Liquidation { .. }),
                ..
            }
        ) || matches!(
            self,
            FastSignal::Tx {
                intent: Some(TxIntent::OevLiquidation { .. }),
                ..
            }
        )
    }
}

alloy::sol! {
    event Mint(address indexed minter, uint256 mintAmount, uint256 mintTokens);
    event Redeem(address indexed redeemer, uint256 redeemAmount, uint256 redeemTokens);
    event Borrow(address indexed borrower, uint256 borrowAmount, uint256 accountBorrows, uint256 totalBorrows);
    event RepayBorrow(address indexed payer, address indexed borrower, uint256 repayAmount, uint256 accountBorrows, uint256 totalBorrows);
    event Transfer(address indexed from, address indexed to, uint256 amount);
    event LiquidateBorrow(address indexed liquidator, address indexed borrower, uint256 repayAmount, address mTokenCollateral, uint256 seizeTokens);
    event PriceUpdatedEarlyAndLiquidated(
        address indexed borrower,
        uint256 repayAmount,
        address indexed mTokenCollateral,
        address indexed mTokenLoan,
        uint256 protocolFee,
        uint256 liquidatorFee
    );
    function borrow(uint256);
    function redeem(uint256);
    function repayBorrow(address) returns (uint256);
    function liquidateBorrow(address borrower, uint256 repayAmount, address mTokenCollateral) returns ((uint256,uint256));
    function transfer(address, uint256) returns (bool);
    function updatePriceEarlyAndLiquidate(address borrower, uint256 repayAmount, address mTokenCollateral, address mTokenLoan);
}

pub fn watch_topics() -> Vec<B256> {
    vec![
        Mint::SIGNATURE_HASH,
        Redeem::SIGNATURE_HASH,
        Borrow::SIGNATURE_HASH,
        RepayBorrow::SIGNATURE_HASH,
        Transfer::SIGNATURE_HASH,
        LiquidateBorrow::SIGNATURE_HASH,
        PriceUpdatedEarlyAndLiquidated::SIGNATURE_HASH,
    ]
}

pub fn watch_selectors() -> Vec<[u8; 4]> {
    vec![
        borrowCall::SELECTOR,
        redeemCall::SELECTOR,
        repayBorrowCall::SELECTOR,
        liquidateBorrowCall::SELECTOR,
        transferCall::SELECTOR,
        updatePriceEarlyAndLiquidateCall::SELECTOR,
    ]
}fn decode_intent(from: Address, input: &[u8]) -> Option<TxIntent> {
    if input.len() < 4 {
        return None;
    }
    match &input[..4] {
        s if *s == liquidateBorrowCall::SELECTOR => {
            let call = liquidateBorrowCall::abi_decode(input, true).ok()?;
            Some(TxIntent::Liquidation {
                borrower: call.borrower,
                liquidator: from,
                repay_amount: call.repayAmount,
                m_token_collateral: call.mTokenCollateral,
            })
        }
        s if *s == updatePriceEarlyAndLiquidateCall::SELECTOR => {
            let call = updatePriceEarlyAndLiquidateCall::abi_decode(input, true).ok()?;
            Some(TxIntent::OevLiquidation {
                borrower: call.borrower,
                liquidator: from,
                repay_amount: call.repayAmount,
                m_token_collateral: call.mTokenCollateral,
                m_token_loan: call.mTokenLoan,
            })
        }
        _ if watch_selectors().contains(|s| s == &input[..4]) => Some(TxIntent::Activity),
        _ => None,
    }
}

fn filter_tx(t: &Transaction, addresses: &[Address], selectors: &[[u8; 4]]) -> bool {
    let input = t.input.to_vec();
    let to_hit = t.to.is_some_and(|to| addresses.contains(&to));
    let sel_hit = input.len() >= 4
        && selectors.iter().any(|s| input[..4] == *s);
    to_hit || sel_hit
}

async fn handle_message(
    text: &str,
    addresses: &[Address],
    selectors: &[[u8; 4]],
    tx: &tokio::sync::mpsc::UnboundedSender<FastSignal>,
) -> Result<()> {
    let v: Value = serde_json::from_str(text)?;
    if v["method"] != "eth_subscription" {
        return Ok(());
    }
    let result = &v["params"]["result"];
    if let Ok(log) = serde_json::from_value::<Log>(result.clone()) {
        let t0 = log.topics().first().copied();
        if let Some(t0) = t0 {
            if watch_topics().contains(&t0) {
                let _ = tx.send(FastSignal::Log {
                    address: log.address(),
                    topic0: t0,
                    tx_hash: log.tx_hash.unwrap_or_default(),
                });
            }
        }
        return Ok(());
    }
    if let Ok(t) = serde_json::from_value::<Transaction>(result.clone()) {
        if filter_tx(&t, addresses, selectors) {
            let input = t.input.to_vec();
            let intent = decode_intent(t.from, &input);
            let _ = tx.send(FastSignal::Tx {
                from: t.from,
                to: t.to,
                input,
                intent,
            });
        }
        return Ok(());
    }
    Ok(())
}

async fn connect_and_subscribe(
    url: &str,
    subscribe: &mut impl FnMut(&mut WsStream) -> impl std::future::Future<Output = Result<Value>>,
) -> Result<(WsStream, Value)> {
    let (mut ws, _resp) = connect_async(url).await?;
    let id = subscribe(&mut ws).await?;
    Ok((ws, id))
}
pub async fn run_flashblocks_monitor(
    ws_url: String,
    addresses: Vec<Address>,
    selectors: Vec<[u8; 4]>,
    tx: tokio::sync::mpsc::UnboundedSender<FastSignal>,
) -> Result<()> {
    let topics: Vec<B256> = watch_topics();
    let addr_hex: Vec<String> = addresses.iter().map(|a| format!("{a:#x}")).collect();
    let topics_hex: Vec<String> = topics.iter().map(|t| format!("{t:#x}")).collect();

    let mut subscribe = move |ws: &mut WsStream| {
        let addr_hex = addr_hex.clone();
        let topics_hex = topics_hex.clone();
        async move {
            let req = json!({
                "id": 1u64,
                "jsonrpc": "2.0",
                "method": "eth_subscribe",
                "params": ["pendingLogs", {
                    "address": addr_hex,
                    "topics": [topics_hex],
                }],
            });
            ws.send(Message::Text(req.to_string().into()).await?;
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) => {
                        let v: Value = serde_json::from_str(&t)?;
                        if v["id"] == json!(1u64) {
                            return Ok(v["result"].clone());
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(anyhow::anyhow!("{e}"))),
                    None => return Err(anyhow::anyhow!("connection closed during subscribe")),
                }
            }
        }
    };

    let mut backoff = Duration::from_millis(500);
    loop {
        match connect_and_subscribe(&ws_url, &mut subscribe).await {
            Ok((mut ws, sub_id)) => {
                info!(?sub_id, "flashblocks pendingLogs connected");
                backoff = Duration::from_millis(500);
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(t)) => {
                            if let Err(e) = handle_message(&t, &addresses, &selectors, &tx).await {
                                warn!(?e, "flashblocks message handling failed");
                            }
                        }
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                        Ok(Message::Close(_)) => {
                            warn!("flashblocks connection closed by server");
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!(?e, "flashblocks connection error");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                warn!(?e, "flashblocks connect failed - retrying in {}ms", backoff.as_millis());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn run_mempool_monitor(
    ws_url: String,
    addresses: Vec<Address>,
    selectors: Vec<[u8; 4]>,
    tx: tokio::sync::mpsc::UnboundedSender<FastSignal>,
) -> Result<()> {
    let mut subscribe = move |ws: &mut WsStream| async move {
        let req = json!({
            "id": 1u64,
            "jsonrpc": "2.0",
            "method": "eth_subscribe",
            "params": ["newPendingTransactions", true],
        });
        ws.send(Message::Text(req.to_string().into()).await?;
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t)?;
                    if v["id"] == json!(1u64) {
                        return Ok(v["result"].clone());
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(anyhow::anyhow!("{e}"))),
                None => return Err(anyhow::anyhow!("connection closed during subscribe")),
            }
        }
    };

    let mut backoff = Duration::from_millis(500);
    loop {
        match connect_and_subscribe(&ws_url, &mut subscribe).await {
            Ok((mut ws, sub_id)) => {
                info!(?sub_id, "mempool full-tx connected");
                backoff = Duration::from_millis(500);
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(t)) => {
                            if let Err(e) = handle_message(&t, &addresses, &selectors, &tx).await {
                                warn!(?e, "mempool message handling failed");
                            }
                        }
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                        Ok(Message::Close(_)) => {
                            warn!("mempool connection closed by server");
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!(?e, "mempool connection error");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                warn!(?e, "mempool connect failed - retrying in {}ms", backoff.as_millis());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
