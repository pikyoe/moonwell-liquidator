use crate::config::Config;
use crate::contracts::{IComptroller, IMToken, LiquidationJob, Mode, COMPTROLLER};
use crate::health::{classify, health_factor, Health, MarketInfo};
use crate::state::SharedState;
use crate::swap::{apply_slippage, build_aerodrome_swap, is_stable_symbol};
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use anyhow::Result;
use std::collections::HashMap;
use tracing::{info, warn};

pub struct Strategy<P: Provider> {
    provider: P,
    state: SharedState,
    cfg: Config,
    markets: HashMap<Address, MarketInfo>,
    executor: Address,
    /// Nilai comptroller yang di-cache — di-refresh sekali waktu
    /// (refresh_prices) agar tidak memanggil RPC di setiap scan.
    close_factor: U256,
    liquidation_incentive: U256,
}

impl<P: Provider + Clone> Strategy<P> {
    pub fn new(
        provider: P,
        state: SharedState,
        cfg: Config,
        markets: HashMap<Address, MarketInfo>,
    ) -> Result<Self> {
        let executor = cfg.executor_address()?;
        // 0 bila belum di-refresh — evaluate() akan skip sampai ada nilai.
        Ok(Self {
            provider,
            state,
            cfg,
            markets,
            executor,
            close_factor: U256::ZERO,
            liquidation_incentive: U256::ZERO,
        })
    }

    /// Panggilan satu kali dari refresh_prices: muat close factor &
    /// liquidation incentive dari comptroller — jangan di-hardcode.
    pub fn update_comptroller_params(&mut self, close_factor: U256, incentive: U256) {
        self.close_factor = close_factor;
        self.liquidation_incentive = incentive;
    }

    /// Evaluasi semua borrower; kembalikan job yang lolos simulasi profit.
    pub async fn scan_opportunities(&self) -> Result<Vec<LiquidationJob>> {
        let mut jobs = Vec::new();
        for borrower in self.state.borrowers() {
            match self.evaluate(borrower).await {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) => {}
                Err(e) => warn!(?borrower, ?e, "evaluasi gagal"),
            }
        }
        Ok(jobs)
    }

    async fn evaluate(&self, borrower: Address) -> Result<Option<LiquidationJob>> {
        let Some(positions) = self.state.positions.get(&borrower) else {
            return Ok(None);
        };

        // susun tuple untuk health engine
        let mut tuples = Vec::new();
        for m in positions.iter() {
            tuples.push((*m.key(), m.mtoken_balance, m.borrow_balance, m.exchange_rate));
        }
        let (_, _, hf) = health_factor(&tuples, &self.markets);
        if classify(hf) != Health::Liquidatable {
            return Ok(None);
        }

        // pilih market pinjaman terbesar sebagai target repay,
        // dan market kolateral terbesar sebagai sumber sitaan.
        let mut best_loan: Option<(Address, U256)> = None;
        let mut best_coll: Option<(Address, U256)> = None;
        for m in positions.iter() {
            let market = *m.key();
            let Some(info) = self.markets.get(&market) else { continue };
            if m.borrow_balance > U256::ZERO {
                let v = info.borrow_value_usd(m.borrow_balance);
                if best_loan.as_ref().map(|(_, bv)| v > *bv).unwrap_or(true) {
                    best_loan = Some((market, m.borrow_balance));
                }
            }
            if m.mtoken_balance > U256::ZERO {
                let v = info.collateral_value_usd(m.mtoken_balance, m.exchange_rate);
                if best_coll.as_ref().map(|(_, bv)| v > *bv).unwrap_or(true) {
                    best_coll = Some((market, v));
                }
            }
        }

        let (mloan, _stale_borrow) = match best_loan {
            Some(x) => x,
            None => return Ok(None),
        };
        let (mcoll, _) = match best_coll {
            Some(x) => x,
            None => return Ok(None),
        };

        // Refresh posisi kandidat di kedua market — snapshot state bisa basi
        // (bunga terakru, likuidasi lain). Bangun job dari data segar.
        for market in [mloan, mcoll] {
            let snap = IMToken::new(market, &self.provider)
                .getAccountSnapshot(borrower)
                .call()
                .await;
            match snap {
                Ok(s) => self.state.upsert(
                    borrower,
                    market,
                    crate::state::Position {
                        mtoken_balance: s.mTokenBalance,
                        borrow_balance: s.borrowBalance,
                        exchange_rate: s.exchangeRateMantissa,
                    },
                ),
                Err(e) => {
                    warn!(?borrower, ?market, ?e, "refresh kandidat gagal — skip");
                    return Ok(None);
                }
            }
        }
        let (borrow_bal, coll_bal) = {
            let Some(pos) = self.state.positions.get(&borrower) else { return Ok(None) };
            let borrow = pos.get(&mloan).map(|p| p.borrow_balance).unwrap_or(U256::ZERO);
            let coll = pos.get(&mcoll).map(|p| p.mtoken_balance).unwrap_or(U256::ZERO);
            (borrow, coll)
        };
        if borrow_bal.is_zero() || coll_bal.is_zero() {
            return Ok(None); // posisi sudah berubah sejak scan
        }

        // Pastikan market kolateral benar-benar aktif sebagai collateral bagi
        // borrower (getAssetsIn). Supply murni tanpa enable-collateral tidak bisa
        // disita — lewati alih-alih buang RPC di simulasi revert.
        let comptroller = IComptroller::new(COMPTROLLER, &self.provider);
        match comptroller.getAssetsIn(borrower).call().await {
            Ok(assets) if !assets.contains(&mcoll) => return Ok(None),
            Err(_) => {}
            Ok(_) => {}
        }

        // batasi repay <= close factor * borrow, dan <= max posisi.
        // Keluar sampai comptroller params sudah dimuat (hindari repay 0/terlalu besar).
        if self.close_factor.is_zero() {
            return Ok(None);
        }
        let mut repay = borrow_bal * self.close_factor / U256::from(10u64.pow(18));
        repay = self.cap_by_max_position(mloan, repay).await?;

        let loan_info = self.markets.get(&mloan).cloned();
        let coll_info = self.markets.get(&mcoll).cloned();
        let (Some(loan_info), Some(coll_info)) = (loan_info, coll_info) else {
            return Ok(None);
        };

        // Bangun parameter swap (kalau diaktifkan dan asetnya beda).
        let (swap_target, swap_data, min_loan_out) =
            self.build_swap(&loan_info, &coll_info, repay)?;

        // Kontrak mengukur profit di token hasil akhir: loan token bila swap
        // aktif, kolateral bila tidak. Ambil ambang per simbol agar desimal
        // (WETH 18 vs USDC 6) tidak membuat ambang salah besar.
        let output_symbol = if swap_target != Address::ZERO {
            &loan_info.symbol
        } else {
            &coll_info.symbol
        };

        let job = LiquidationJob {
            mode: Mode::Oev,
            loanToken: loan_info.underlying,
            swapTarget: swap_target,
            swapData: swap_data,
            mTokenLoan: mloan,
            mTokenCollateral: mcoll,
            borrower,
            repayAmount: repay,
            minProfit: self.cfg.min_profit_for_symbol(output_symbol)?,
            minLoanOut: min_loan_out,
        };

        info!(?borrower, ?mloan, ?mcoll, %repay, "peluang terdeteksi");
        Ok(Some(job))
    }

    /// Hitung estimasi sitaan liquidator (OEV split) lalu bangun calldata swap
    /// kolateral -> loanToken. Mengembalikan (target, calldata, minLoanOut).
    /// Jika swap nonaktif atau aset sama: (ZERO, kosong, 0).
    fn build_swap(
        &self,
        loan: &MarketInfo,
        coll: &MarketInfo,
        repay: U256,
    ) -> Result<(Address, alloy::primitives::Bytes, U256)> {
        let zero: alloy::primitives::Bytes = Default::default();
        if !self.cfg.swap.enabled || loan.underlying == coll.underlying {
            return Ok((Address::ZERO, zero, U256::ZERO));
        }
        if loan.price == U256::ZERO || coll.price == U256::ZERO {
            return Ok((Address::ZERO, zero, U256::ZERO));
        }

        // Keluar sampai liquidation incentive sudah dimuat (tak teratur bila 0).
        if self.liquidation_incentive.is_zero() {
            return Ok((Address::ZERO, zero.clone(), U256::ZERO));
        }

        let one = U256::from(10u64.pow(18));
        // nilai repay dalam USD (1e18)
        let repay_usd = repay * loan.price / one;
        // Sitaan bruto dikurangi bagian protokol (protocolSeizeShareMantissa) —
        // estimasi tanpa koreksi ini membuat amount_in melebihi saldo aktual.
        let seized_usd = repay_usd * self.liquidation_incentive / one
            * (one - coll.protocol_seize_share) / one;
        let gross_profit_usd = seized_usd.saturating_sub(repay_usd);
        // jalur OEV: liquidator dapat repay + profit * liquidatorFeeBps/10000.
        // Fee dibaca on-chain per market bila tersedia; config hanya fallback.
        let fee_bps = coll.oev_fee_bps.unwrap_or(self.cfg.swap.liquidator_fee_bps);
        let liquidator_usd = repay_usd
            + gross_profit_usd * U256::from(fee_bps) / U256::from(10_000u64);

        // konversi ke unit underlying kolateral & estimasi hasil dalam loanToken
        let amount_in = liquidator_usd * one / coll.price;
        let expected_loan_out = liquidator_usd * one / loan.price;

        let min_out = apply_slippage(expected_loan_out, self.cfg.swap.slippage_bps);
        let deadline = U256::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                + 600,
        );

        let router: Address = if self.cfg.swap.router.is_empty() {
            crate::swap::AERODROME_ROUTER.parse()?
        } else {
            self.cfg.swap.router.parse()?
        };

        let (target, data) = build_aerodrome_swap(crate::swap::SwapParams {
            router,
            from_token: coll.underlying,
            to_token: loan.underlying,
            amount_in,
            amount_out_min: min_out,
            recipient: self.executor,
            stable_pair: is_stable_symbol(loan.symbol.as_str())
                && is_stable_symbol(coll.symbol.as_str()),
            deadline,
        })?;

        // kontrak menuntut balance >= assets + minLoanOut; sisanya jadi profit
        let min_loan_out = min_out.saturating_sub(repay);

        info!(
            %amount_in, %expected_loan_out, %min_out,
            from = %coll.symbol, to = %loan.symbol,
            "swap disiapkan"
        );
        Ok((target, data, min_loan_out))
    }

    pub fn update_price(&mut self, mtoken: Address, price: U256) {
        if let Some(info) = self.markets.get_mut(&mtoken) {
            info.price = price;
        }
    }

    async fn cap_by_max_position(&self, market: Address, repay: U256) -> Result<U256> {
        let Some(info) = self.markets.get(&market) else { return Ok(repay) };
        // konversi USD cap ke unit underlying: cap_usd / price * 10^decimals
        let cap_usd = U256::from(self.cfg.max_position_usd) * U256::from(10u64.pow(18));
        if info.price == U256::ZERO {
            return Ok(repay);
        }
        let cap_underlying = cap_usd * U256::from(10u64.pow(18)) / info.price;
        Ok(repay.min(cap_underlying))
    }
}
