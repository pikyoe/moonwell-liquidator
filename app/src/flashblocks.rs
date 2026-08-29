use alloy::consensus::Transaction as ConsensusTransaction;
use alloy::network::TransactionResponse;
use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::{Log, Transaction};
use alloy::sol_types::{SolCall, SolEvent};
use anyhow::Result;
use futures::{SinkExt, StreamExt};
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
        /// Borrower yang diekstrak dari event Borrow/LiquidateBorrow/
        /// PriceUpdatedEarlyAndLiquidated (None untuk Mint/Redeem/Transfer).
        /// Dipakai main.rs agar sinyal preconfirmation akun BARU (belum ada
        /// di state) tetap bisa di-refresh + di-scan secepatnya.

        borrower: Option<Address>,
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
            FastSignal::Log { borrower: Some(b), .. } => Some(*b),
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
}

fn decode_intent(from: Address, input: &[u8]) -> Option<TxIntent> {
    if input.len() < 4 {
        return None;
    }
    match &input[..4] {
        s if *s == liquidateBorrowCall::SELECTOR => {
            let call = liquidateBorrowCall::abi_decode(input).ok()?;
            Some(TxIntent::Liquidation {
                borrower: call.borrower,
                liquidator: from,
                repay_amount: call.repayAmount,
                m_token_collateral: call.mTokenCollateral,
            })
        }
        s if *s == updatePriceEarlyAndLiquidateCall::SELECTOR => {
            let call = updatePriceEarlyAndLiquidateCall::abi_decode(input).ok()?;
            Some(TxIntent::OevLiquidation {
                borrower: call.borrower,
                liquidator: from,
                repay_amount: call.repayAmount,
                m_token_collateral: call.mTokenCollateral,
                m_token_loan: call.mTokenLoan,
            })
        }
        _ if watch_selectors().iter().any(|s| *s == input[..4]) => Some(TxIntent::Activity),
        _ => None,
    }
}

/// Hanya tx yang benar-benar menyentuh alamat yang dipantau (market,
/// comptroller, wrapper OEV, executor, signer) yang diteruskan — pencocok
/// selector saja tidak aman: selector transfer/likuidasi muncul di seluruh
/// Base mempool, dan tx tak terkait tidak boleh memicu refresh+scan.
fn filter_tx(t: &Transaction, addresses: &[Address]) -> bool {
    t.to().is_some_and(|to| addresses.contains(&to))
}

/// Jenis subscription yang sedang ditangani — dipakai sebagai diskriminator
/// tegas di `handle_message` (bukan coba-parse Log lalu Transaction berurutan
/// yang bisa salah-klasifikasi payload tidak dikenal)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionKind {
    FlashblocksLogs,
    MempoolTxs,
}

async fn handle_message(
    text: &str,
    kind: SubscriptionKind,
    addresses: &[Address],
    selectors: &[[u8; 4]],
    tx: &tokio::sync::mpsc::UnboundedSender<FastSignal>,
) -> Result<()> {
    let v: Value = serde_json::from_str(text)?;
    if v["method"] != "eth_subscription" {
        return Ok(());
    }
    let result = &v["params"]["result"];
    match kind {
        SubscriptionKind::FlashblocksLogs => {
            match serde_json::from_value::<Log>(result.clone()) {
                Ok(log) => {
                    // Ekstrak borrower untuk event yang menyangkut posisi pinjam;
                    // sinyal preconfirmation akun baru (belum ada di state) tetap bisa
                    // di-refresh + di-scan cepat oleh penerima (lihat main.rs).

                    let borrower = {
                        if let Ok(ev) = Borrow::decode_log(&log.inner) {
                            Some(ev.borrower)
                        } else if let Ok(ev) = LiquidateBorrow::decode_log(&log.inner) {
                            Some(ev.borrower)
                        } else if let Ok(ev) = PriceUpdatedEarlyAndLiquidated::decode_log(&log.inner) {
                            Some(ev.borrower)
                        } else {
                            None
                        }
                    };
                    let t0 = log.topics().first().copied();
                    if let Some(t0) = t0 {
                        if watch_topics().contains(&t0) {
                            let _ = tx.send(FastSignal::Log {
                                address: log.address(),
                                topic0: t0,
                                tx_hash: log.transaction_hash.unwrap_or_default(),
                                borrower,
                            });
                        }
                    }
                }
                Err(e) => warn!(?e, "pesan flashblocks bukan Log — diabaikan"),
            }
        }
        SubscriptionKind::MempoolTxs => {
            // Provider tertentu (mis. Base public RPC) mengabaikan argumen
            // `true` dan tetap mengirim hash alih-alih objek penuh — terima
            // tenang dan lewati (tanpa body, intent tidak bisa di-decode).
            if result.is_string() {
                return Ok(());
            }
            match serde_json::from_value::<Transaction>(result.clone()) {
                Ok(t) => {
                    if filter_tx(&t, addresses) {
                        let input = t.input().to_vec();
                        // Hanya tx yang memanggil fungsi yang diawasi (borrow/redeem/
                        // repay/transfer/liquidateBorrow/updatePriceEarlyAndLiquidate)
                        // yang diteruskan; panggilan acak lain ke kontrak kita
                        // (approve/mint/dll) tidak boleh memicu refresh+scan.
                        if input.len() < 4 || !selectors.iter().any(|s| *s == input[..4]) {
                            return Ok(());
                        }
                        let intent = decode_intent(t.from(), &input);
                        let _ = tx.send(FastSignal::Tx {
                            from: t.from(),
                            to: t.to(),
                            input,
                            intent,
                        });
                    }
                }
                Err(e) => warn!(?e, "pesan mempool bukan Transaction — diabaikan"),
            }
        }
    }
    Ok(())
}

/// Sambung WS, kirim `eth_subscribe`, dan validasi respons JSON-RPC.
    /// Respons dengan `error` atau tanpa `result` yang valid ditolak —
    /// monitor akan reconnect dengan backoff alih-alih menunggu diam.
async fn connect_and_subscribe(url: &str, req: Value) -> Result<(WsStream, Value)> {
    let (mut ws, _resp) = connect_async(url).await?;
    ws.send(Message::Text(req.to_string().into())).await?;
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t)?;
                if v.get("id") != Some(&id) {
                    continue;
                }
                if let Some(err) = v.get("error") {
                    return Err(anyhow::anyhow!("subscription error: {err}"));
                }
                match v.get("result") {
                    Some(r) if !r.is_null() => return Ok((ws, r.clone())),
                    _ => return Err(anyhow::anyhow!("subscription response lacks valid result")),
                }
            }
            Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(anyhow::anyhow!("{e}")),
            None => return Err(anyhow::anyhow!("connection closed during subscribe")),
        }
    }
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

    let mut backoff = Duration::from_millis(500);
    loop {
        let req = json!({
            "id": 1u64,
            "jsonrpc": "2.0",
            "method": "eth_subscribe",
            "params": ["pendingLogs", {
                "address": addr_hex.clone(),
                "topics": [topics_hex.clone()],
            }],
        });
        match connect_and_subscribe(&ws_url, req).await {
            Ok((mut ws, sub_id)) => {
                info!(?sub_id, "flashblocks pendingLogs connected");
                backoff = Duration::from_millis(500);
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(t)) => {
                            if let Err(e) = handle_message(&t, SubscriptionKind::FlashblocksLogs, &addresses, &selectors, &tx).await {
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
    let mut backoff = Duration::from_millis(500);
    loop {
        // Banyak node (terutama Base public RPC) tidak mendukung argumen
        // `true` pada newPendingTransactions — coba body-penuh dulu; kalau
        // endpoint menolak (subscription error), fallback ke hash-only (sinyal
        // Tx oleh karena itu tidak ter-decode, namun monitor tetap hidup dan
        // flashblock logs (jalur utama) tetap jalan).

        let req_full = json!({
            "id": 1u64,
            "jsonrpc": "2.0",
            "method": "eth_subscribe",
            "params": ["newPendingTransactions", true],
        });
        let req_hash = json!({
            "id": 1u64,
            "jsonrpc": "2.0",
            "method": "eth_subscribe",
            "params": ["newPendingTransactions"],
        });

        let sub_result = match connect_and_subscribe(&ws_url, req_full).await {
            Ok(ok) => Ok(ok),
            Err(e) if e.to_string().contains("subscription error") => {
                warn!(?e, "mempool body-penuh ditolak — fallback ke hash-only");
                connect_and_subscribe(&ws_url, req_hash).await
            }
            Err(e) => Err(e),
        };

        match sub_result {
            Ok((mut ws, sub_id)) => {
                info!(?sub_id, "mempool full-tx connected");
                backoff = Duration::from_millis(500);
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(t)) => {
                            if let Err(e) = handle_message(&t, SubscriptionKind::MempoolTxs, &addresses, &selectors, &tx).await {
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
