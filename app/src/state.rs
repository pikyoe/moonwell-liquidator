use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Posisi satu akun di satu market.
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    pub mtoken_balance: U256,
    pub borrow_balance: U256,
    pub exchange_rate: U256,
}

/// State off-chain semua akun yang punya pinjaman.
#[derive(Debug, Default)]
pub struct AccountState {
    /// account -> market -> posisi
    pub positions: DashMap<Address, DashMap<Address, Position>>,
    /// daftar akun yang sedang meminjam (borrow_balance > 0 di market manapun)
    pub borrowers: DashMap<Address, ()>,
}

pub type SharedState = Arc<AccountState>;

impl AccountState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&self, account: Address, market: Address, pos: Position) {
        self.positions
            .entry(account)
            .or_default()
            .insert(market, pos);
        if pos.borrow_balance > U256::ZERO {
            self.borrowers.insert(account, ());
        }
    }

    pub fn borrowers(&self) -> Vec<Address> {
        self.borrowers.iter().map(|e| *e.key()).collect()
    }

    /// Snapshot ringan ke disk untuk mempercepat restart.
    /// Bukan source of truth — selalu di-resync dari chain setelah load.
    pub fn save_snapshot(&self, path: &str, last_block: u64) -> std::io::Result<()> {
        let mut pos: HashMap<String, HashMap<String, PositionSerde>> = HashMap::new();
        for acct in self.positions.iter() {
            let mut inner = HashMap::new();
            for m in acct.value().iter() {
                inner.insert(
                    format!("{:?}", m.key()),
                    PositionSerde {
                        mtoken_balance: m.mtoken_balance.to_string(),
                        borrow_balance: m.borrow_balance.to_string(),
                        exchange_rate: m.exchange_rate.to_string(),
                    },
                );
            }
            pos.insert(format!("{:?}", acct.key()), inner);
        }
        let snap = Snapshot {
            last_block,
            positions: pos,
        };
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, serde_json::to_string(&snap).unwrap())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load_snapshot(path: &str) -> Option<(u64, Self)> {
        let raw = std::fs::read_to_string(path).ok()?;
        let snap: Snapshot = serde_json::from_str(&raw).ok()?;
        let state = Self::new();
        for (acct, markets) in snap.positions {
            let account: Address = acct.parse().ok()?;
            for (mkt, p) in markets {
                let market: Address = mkt.parse().ok()?;
                state.upsert(
                    account,
                    market,
                    Position {
                        mtoken_balance: p.mtoken_balance.parse().ok()?,
                        borrow_balance: p.borrow_balance.parse().ok()?,
                        exchange_rate: p.exchange_rate.parse().ok()?,
                    },
                );
            }
        }
        Some((snap.last_block, state))
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    last_block: u64,
    positions: HashMap<String, HashMap<String, PositionSerde>>,
}

#[derive(Serialize, Deserialize)]
struct PositionSerde {
    mtoken_balance: String,
    borrow_balance: String,
    exchange_rate: String,
}
