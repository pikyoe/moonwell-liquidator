use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use crate::contracts::{IAerodromeRouter, Route};

/// Aerodrome Router di Base — venue default dengan likuiditas terdalam.
pub const AERODROME_ROUTER: &str = "0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43";
/// Aerodrome pool factory (untuk field Route.factory).
pub const AERODROME_FACTORY: &str = "0x420DD381b31aEf6683db6B902084cB0FFECe40Da";

/// Simbol yang dianggap "stable" — pair stable-stable pakai pool stable.
pub fn is_stable_symbol(symbol: &str) -> bool {
    matches!(symbol, "USDC" | "USDbC" | "DAI" | "EURC" | "USDS" | "USDT" | "USDT0")
}

/// Bangun calldata swap direct satu-hop Aerodrome.
/// Mengembalikan (target router, calldata) siap dimasukkan ke LiquidationJob.
pub fn build_aerodrome_swap(
    from_token: Address,
    to_token: Address,
    amount_in: U256,
    amount_out_min: U256,
    recipient: Address,
    stable_pair: bool,
    deadline: U256,
) -> anyhow::Result<(Address, Bytes)> {
    let factory: Address = AERODROME_FACTORY.parse()?;
    let route = Route {
        from: from_token,
        to: to_token,
        stable: stable_pair,
        factory,
    };

    let call = IAerodromeRouter::swapExactTokensForTokensCall {
        amountIn: amount_in,
        amountOutMin: amount_out_min,
        routes: vec![route],
        to: recipient,
        deadline,
    };

    let target: Address = AERODROME_ROUTER.parse()?;
    Ok((target, Bytes::from(call.abi_encode())))
}

/// Slippage guard: amountOutMin = expected * (10000 - slippage_bps) / 10000.
pub fn apply_slippage(expected_out: U256, slippage_bps: u64) -> U256 {
    expected_out * U256::from(10_000u64 - slippage_bps) / U256::from(10_000u64)
}
