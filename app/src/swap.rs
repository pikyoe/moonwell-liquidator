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

/// Parameter lengkap untuk satu swap Aerodrome satu-hop.
pub struct SwapParams {
    pub router: Address,
    pub from_token: Address,
    pub to_token: Address,
    pub amount_in: U256,
    pub amount_out_min: U256,
    pub recipient: Address,
    pub stable_pair: bool,
    pub deadline: U256,
}

/// Bangun calldata swap direct satu-hop Aerodrome untuk `params.router`
/// (default kompatibel; override config dipakai per panggilan).
/// Mengembalikan (target router, calldata) siap dimasukkan ke LiquidationJob.
pub fn build_aerodrome_swap(params: SwapParams) -> anyhow::Result<(Address, Bytes)> {
    let factory: Address = AERODROME_FACTORY.parse()?;
    let route = Route {
        from: params.from_token,
        to: params.to_token,
        stable: params.stable_pair,
        factory,
    };

    let call = IAerodromeRouter::swapExactTokensForTokensCall {
        amountIn: params.amount_in,
        amountOutMin: params.amount_out_min,
        routes: vec![route],
        to: params.recipient,
        deadline: params.deadline,
    };

    Ok((params.router, Bytes::from(call.abi_encode())))
}

/// Slippage guard: amountOutMin = expected * (10000 - slippage_bps) / 10000.
pub fn apply_slippage(expected_out: U256, slippage_bps: u64) -> U256 {
    expected_out * U256::from(10_000u64 - slippage_bps) / U256::from(10_000u64)
}


#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::SolCall;

    #[test]
    fn slippage_2_persen() {
        let expected = U256::from(1_000_000u64);
        assert_eq!(apply_slippage(expected, 200), U256::from(980_000u64));
    }

    #[test]
    fn slippage_nol_dan_penuh() {
        let expected = U256::from(5_000u64);
        assert_eq!(apply_slippage(expected, 0), expected);
        assert_eq!(apply_slippage(expected, 10_000), U256::ZERO);
    }

    #[test]
    fn stable_symbol_dikenali() {
        assert!(is_stable_symbol("USDC"));
        assert!(is_stable_symbol("EURC"));
        assert!(!is_stable_symbol("WETH"));
        assert!(!is_stable_symbol("cbBTC"));
    }

    #[test]
    fn router_override_dihormati() {
        let custom: Address = "0x1111111111111111111111111111111111111111".parse().unwrap();
        let token: Address = "0x2222222222222222222222222222222222222222".parse().unwrap();
        let (target, data) = build_aerodrome_swap(SwapParams {
            router: custom,
            from_token: token,
            to_token: token,
            amount_in: U256::from(1u64),
            amount_out_min: U256::from(1u64),
            recipient: token,
            stable_pair: false,
            deadline: U256::from(1u64),
        })
        .unwrap();
        assert_eq!(target, custom, "target harus mengikuti router override");
        assert_eq!(
            &data[..4],
            &IAerodromeRouter::swapExactTokensForTokensCall::SELECTOR[..],
            "calldata harus diawali selector swapExactTokensForTokens"
        );
    }
}
