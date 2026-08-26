use alloy::primitives::{Address, U256};

/// Health factor sederhana: sum(collateral * cf * price) / sum(borrow * price).
/// Menggunakan harga 1e36-normalized dari oracle Moonwell.
#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub underlying: Address,
    pub symbol: String,
    pub collateral_factor: U256, // 1e18
    pub price: U256,             // 1e(36-decimals)
}

impl MarketInfo {
    pub fn collateral_value_usd(&self, mtoken_balance: U256, exchange_rate: U256) -> U256 {
        // underlying = mtoken * rate / 1e18 ; value = underlying * price / 1e18 (price sudah 1e(36-dec))
        let underlying = mtoken_balance * exchange_rate / U256::from(10u64.pow(18));
        underlying * self.price / U256::from(10u64.pow(18))
    }

    pub fn borrow_value_usd(&self, borrow_balance: U256) -> U256 {
        borrow_balance * self.price / U256::from(10u64.pow(18))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Safe,
    Liquidatable,   // HF < 1
}

pub fn health_factor(
    positions: &[(Address, U256, U256, U256)], // (mtoken, mtoken_bal, borrow_bal, exchange_rate)
    markets: &std::collections::HashMap<Address, MarketInfo>,
) -> (U256, U256, U256) {
    // returns (collateral_usd, borrow_usd, hf_scaled_1e18)
    let one = U256::from(10u64.pow(18));
    let mut coll_usd = U256::ZERO;
    let mut borr_usd = U256::ZERO;

    for (mtoken, mbal, bbal, rate) in positions {
        let Some(info) = markets.get(mtoken) else { continue };
        if *mbal > U256::ZERO {
            let cv = info.collateral_value_usd(*mbal, *rate);
            coll_usd += cv * info.collateral_factor / one;
        }
        if *bbal > U256::ZERO {
            borr_usd += info.borrow_value_usd(*bbal);
        }
    }

    let hf = if borr_usd == U256::ZERO {
        U256::MAX
    } else {
        coll_usd * one / borr_usd
    };
    (coll_usd, borr_usd, hf)
}

pub fn classify(hf: U256) -> Health {
    if hf == U256::MAX {
        return Health::Safe;
    }
    let one = U256::from(10u64.pow(18));
    if hf < one {
        Health::Liquidatable
    } else {
        Health::Safe
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn market(price: U256, cf: U256) -> MarketInfo {
        MarketInfo {
            underlying: Address::ZERO,
            symbol: "X".into(),
            collateral_factor: cf,
            price,
        }
    }

    #[test]
    fn hf_liquidatable_di_bawah_satu() {
        let mkt = Address::from([1u8; 20]);
        let one = U256::from(10u64.pow(18));
        // supply 10 * rate 1 = 10 units, price 1e36 -> $10e18; CF 0.5 -> $5e18 efektif
        // borrow 7 units * $1 = $7e18 -> HF = 5/7 < 1
        let price = U256::from(10u64).pow(U256::from(36u64));
        let mut markets = HashMap::new();
        markets.insert(mkt, market(price, one / U256::from(2u64)));
        let pos = vec![(mkt, U256::from(10u64) * one, U256::from(7u64) * one, one)];
        let (_, _, hf) = health_factor(&pos, &markets);
        assert_eq!(classify(hf), Health::Liquidatable);
    }

    #[test]
    fn hf_safe_tanpa_borrow() {
        let mut markets = HashMap::new();
        let mkt = Address::from([2u8; 20]);
        markets.insert(mkt, market(U256::from(10u64).pow(U256::from(36u64)), U256::from(10u64.pow(18))));
        let one = U256::from(10u64.pow(18));
        let pos = vec![(mkt, one, U256::ZERO, one)];
        let (_, _, hf) = health_factor(&pos, &markets);
        assert_eq!(hf, U256::MAX);
        assert_eq!(classify(hf), Health::Safe);
    }
}
