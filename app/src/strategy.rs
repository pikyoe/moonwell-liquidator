use crate::config::Config;
use crate::contracts::{IComptroller, IMToken, IMulticall3, LiquidationJob, Mode, MULTICALL3, COMPTROLLER};
use crate::health::{classify, health_factor, Health, MarketInfo};
use crate::state::{Position, SharedState};
use crate::swap::{apply_slippage, build_aerodrome_swap, is_stable_symbol};
use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall;
use alloy::providers::Provider;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};


/// Parametrisasi generik P hanya untuk kompatibilitas panggilan; strategy
/// tidak memegang provider sendiri — provider dilewatkan per-panggilan ke
/// `scan()` agar ScanJob bisa di-spawn sebagai task owned ('static).
pub struct Strategy<P> {
    _provider: std::marker::PhantomData<P>,
    state: SharedState,
    cfg: Config,
    markets: HashMap<Address, MarketInfo>,
    executor: Address,
    /// Nilai comptroller yang di-cache — di-refresh sekali waktu
    /// (refresh_prices) agar tidak memanggil RPC di setiap scan.
    close_factor: U256,
    liquidation_incentive: U256,
}

/// Rekaman SEMUA input yang dibaca selama evaluasi peluang, disalin ke sini
/// DI BAWAH lock singkat (murni memori, tanpa await). Evaluasi berikutnya
/// berjalan tanpa memegang mutex strategy — jadi scan blok reguler tidak
/// men-stall scan trigger OEV, dan sebaliknya.
struct ScanSnapshot {
    state: SharedState,
    cfg: Config,
    markets: HashMap<Address, MarketInfo>,
    executor: Address,
    close_factor: U256,
    liquidation_incentive: U256,
}

/// Job ber-own untuk satu putaran scan: daftar borrower + semua data yang
/// dibutuhkan untuk membangun peluang, plus provider untuk query state segar.
/// Send + 'static => bisa di-spawn dan dijalankan paralel dengan scan lain
/// (mis. scan blok reguler dan scan trigger OEV berjalan bersamaan).
pub struct ScanJob<P: Provider + Clone> {
    snapshot: ScanSnapshot,
    provider: P,
    borrowers: Vec<Address>,
}

impl<P: Provider + Clone + Send + Sync + 'static> ScanJob<P> {
    /// Evaluasi semua borrower secara paralel (konkurensi terbatas lewat
    /// semaphore) dan kembalikan daftar job likuidasi yang lolos simulasinya.
    pub async fn run(self: &Arc<Self>) -> Vec<LiquidationJob> {
        let sem = Arc::new(Semaphore::new(self.snapshot.cfg.eval_concurrency.max(1)));
        let mut set = tokio::task::JoinSet::new();
        for borrower in &self.borrowers {
            let borrower = *borrower;
            let me = self.clone();
            let sem = sem.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await;
                match me.evaluate(borrower).await {
                    Ok(Some(job)) => Some(job),
                    Ok(None) => None,
                    Err(e) => {
                        warn!(?borrower, ?e, "evaluasi gagal");
                        None
                    }
                }
            });
        }
        let mut jobs = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok(Some(job)) = res {
                jobs.push(job);
            }
        }
        jobs
    }

    async fn evaluate(&self, borrower: Address) -> Result<Option<LiquidationJob>> {
        let snap = &self.snapshot;
        let Some(positions) = snap.state.positions.get(&borrower) else {
            return Ok(None);
        };

        let mut tuples = Vec::new();
        for m in positions.iter() {
            tuples.push((*m.key(), m.mtoken_balance, m.borrow_balance, m.exchange_rate));
        }
        drop(positions);
        let (_, _, hf) = health_factor(&tuples, &snap.markets);
        if classify(hf) != Health::Liquidatable {
            return Ok(None);
        }
        let comptroller = IComptroller::new(COMPTROLLER, &self.provider);
        let enabled = match comptroller.getAssetsIn(borrower).call().await {
            Ok(assets) => assets,
            Err(e) => {
                warn!(?borrower, ?e, "getAssetsIn gagal — skip");
                return Ok(None);
            }
        };
        let mut best_loan: Option<(Address, U256)> = None;
        for (market, _mbal, bbal, _rate) in &tuples {
            let Some(info) = snap.markets.get(market) else { continue };
            if *bbal > U256::ZERO {
                let v = info.borrow_value_usd(*bbal);
                if best_loan.as_ref().map(|(_, bv)| v > *bv).unwrap_or(true) {
                    best_loan = Some((*market, *bbal));
                }
            }
        }
        let (mloan, _stale_borrow) = match best_loan {
            Some(x) => x,
            None => return Ok(None),
        };

        // Prefer coll di market lain; fallback same-market → Classic only.
        let mut best_coll_other: Option<(Address, U256)> = None;
        let mut best_coll_same: Option<(Address, U256)> = None;
        for (market, mbal, _bbal, rate)in &tuples {
            if !enabled.contains(market) { continue; }
            let Some(info) = snap.markets.get(market) else { continue };
            if *mbal == U256::ZERO {
                continue;
            }
            let v = info.collateral_value_usd(*mbal, *rate);
            if *market == mloan {
                if best_coll_same.as_ref().map(|(_, bv)| v > *bv).unwrap_or(true) {
                    best_coll_same = Some((*market, v));
                }
            } else if best_coll_other.as_ref().map(|(_, bv)| v > *bv).unwrap_or(true) {
                best_coll_other = Some((*market, v));
            }
        }
        let (mcoll, _) = match (best_coll_other, best_coll_same) {
            (Some(x), _) => x,
            (None, Some(x)) => {
                debug!(?mloan, "kolateral hanya di market pinjaman — same-market Classic");
                x
            }
            (None, None) => {
                debug!(?mloan, "tidak ada kolateral yang bisa disita — skip");
                return Ok(None);
            }
        };

        let mut calls: Vec<IMulticall3::Call3> = Vec::new();
        for &market in &[mloan, mcoll] {
            calls.push(IMulticall3::Call3 {
                target: market,
                allowFailure: true,
                callData: IMToken::accrueInterestCall { }.abi_encode().into(),
            });
        }
        for &market in &[mloan, mcoll] {
            calls.push(IMulticall3::Call3 {
                target: market,
                allowFailure: true,
                callData: IMToken::getAccountSnapshotCall { account: borrower }.abi_encode().into(),
            });
        }
        let mcall = IMulticall3::new(MULTICALL3, &self.provider);
        let res = mcall.aggregate3(calls).call().await?;
        if res.len() != 4 {
            warn!(?borrower, got = res.len(), "refresk kandidat multicall tidak lengkap — skip");
            return Ok(None);
        }
        for (i, market)in [mloan, mcoll].into_iter().enumerate() {
            let acc = &res[i];
            let r = &res[2 + i];
            let accrue_ok = acc.success
                && IMToken::accrueInterestCall::abi_decode_returns(&acc.returnData)
                    .map(|r| r == U256::ZERO)
                    .unwrap_or(false);
            if !accrue_ok || !r.success {
                warn!(?borrower, ?market, "accrue+snapshot kandidat gagal — skip");
                return Ok(None);
            }
            match IMToken::getAccountSnapshotCall::abi_decode_returns(&r.returnData) {
                Ok(s) => snap.state.upsert_or_remove(
                    borrower,
                    market,
                    Position {
                        mtoken_balance: s.mTokenBalance,
                        borrow_balance: s.borrowBalance,
                        exchange_rate: s.exchangeRateMantissa,
                    },
                ),
                Err(e) => {
                    warn!(?borrower, ?market, ?e, "decode snapshot kandidat gagal — skip");
                    return Ok(None);
                }
            }
        }
        let (borrow_bal, coll_bal) = {
            let Some(pos) = snap.state.positions.get(&borrower) else { return Ok(None) };
            let borrow = pos.get(&mloan).map(|p| p.borrow_balance).unwrap_or(U256::ZERO);
            let coll = pos.get(&mcoll).map(|p| p.mtoken_balance).unwrap_or(U256::ZERO);
            (borrow, coll)
        };
        if borrow_bal.is_zero() || coll_bal.is_zero() {
            return Ok(None);
        }

        // Re-check HF dengan data SEGAR setelah accrueInterest + snapshot
        // refresh. HF awal pake data stale cache; antara cache dan eth_call
        // simulasi, posisi bisa berubah (interest accrual, borrower
        // deposit/repay). Tanpa re-check ini, bot terus kirim job yang revert
        // "liquidate failed" (INSUFFICIENT_SHORTFALL) karena on-chain
        // getAccountLiquidityInternal melihat shortfall == 0.
        // NOTE: oracle prices tetap dari cache; kalau oracle-driven stale
        // reverts signifikan, refresh harga via getFeed sebelum re-check HF.
        {
            let Some(pos) = snap.state.positions.get(&borrower) else { return Ok(None) };
            let mut fresh_tuples = Vec::new();
            for m in pos.iter() {
                fresh_tuples.push((*m.key(), m.mtoken_balance, m.borrow_balance, m.exchange_rate));
            }
            drop(pos);
            let (_, _, fresh_hf) = health_factor(&fresh_tuples, &snap.markets);
            if classify(fresh_hf) != Health::Liquidatable {
                debug!(?borrower, fresh_hf = %fresh_hf, "posisi sudah tidak underwater setelah refresh — skip");
                return Ok(None);
            }
        }

        if snap.close_factor.is_zero() {
            return Ok(None);
        }
        let mut repay = borrow_bal * snap.close_factor / U256::from(10u64.pow(18));
        repay = self.cap_by_max_position(snap, mloan, repay).await?;

        let loan_info = snap.markets.get(&mloan).cloned();
        let coll_info = snap.markets.get(&mcoll).cloned();
        let (Some(loan_info), Some(coll_info)) = (loan_info, coll_info) else {
            return Ok(None);
        };

        let same_market = mloan == mcoll;
        let mode = if same_market {
            debug!(?mloan, "same-market liquidation — paksa Classic");
            Mode::Classic
        } else if coll_info.oev_fee_bps.is_some() {
            Mode::Oev
        } else {
            debug!(?mcoll, coll_symbol = %coll_info.symbol, "kolateral tanpa wrapper OEV — gunakan jalur Classic");
            Mode::Classic
        };

        let (swap_target, swap_data, min_loan_out) =
            self.build_swap(snap, &loan_info, &coll_info, repay, mode)?;

        let output_symbol = if swap_target != Address::ZERO {
            &loan_info.symbol
        } else {
            &coll_info.symbol
        };

        let job = LiquidationJob {
            mode,
            loanToken: loan_info.underlying,
            swapTarget: swap_target,
            swapData: swap_data,
            mTokenLoan: mloan,
            mTokenCollateral: mcoll,
            borrower,
            repayAmount: repay,
            minProfit: snap.cfg.min_profit_for_symbol(output_symbol)?,
            minLoanOut: min_loan_out,
        };

        info!(?borrower, ?mloan, ?mcoll, loan_symbol = %loan_info.symbol, coll_symbol = %coll_info.symbol, %repay, "peluang terdeteksi");
        Ok(Some(job))
    }

    fn build_swap(
        &self,
        snap: &ScanSnapshot,
        loan: &MarketInfo,
        coll: &MarketInfo,
        repay: U256,
        mode: Mode,
    ) -> Result<(Address, alloy::primitives::Bytes, U256)> {
        build_swap_parts(snap, loan, coll, repay, mode, amount_in_buffer_bps(mode))
    }
}

fn amount_in_buffer_bps(mode: Mode) -> u64 {
    match mode {
        Mode::Oev => 10_000,
        Mode::Classic => 9_500,
        Mode::__Invalid => 9_500,
    }
}

fn build_swap_parts(
    snap: &ScanSnapshot,
    loan: &MarketInfo,
    coll: &MarketInfo,
    repay: U256,
    mode: Mode,
    amount_in_buffer_bps: u64,
) -> Result<(Address, alloy::primitives::Bytes, U256)> {
        let zero: alloy::primitives::Bytes = Default::default();
        if !snap.cfg.swap.enabled || loan.underlying == coll.underlying {
            return Ok((Address::ZERO, zero, U256::ZERO));
        }
        if loan.price == U256::ZERO || coll.price == U256::ZERO {
            return Ok((Address::ZERO, zero, U256::ZERO));
        }
        if snap.liquidation_incentive.is_zero() {
            return Ok((Address::ZERO, zero.clone(), U256::ZERO));
        }

        let one = U256::from(10u64.pow(18));
        let repay_usd = repay * loan.price / one;
        let seized_usd = repay_usd * snap.liquidation_incentive / one
            * (one - coll.protocol_seize_share) / one;
        let gross_profit_usd = seized_usd.saturating_sub(repay_usd);

        let liquidator_usd = match mode {
            Mode::Oev => {
                let fee_bps = coll.oev_fee_bps.unwrap_or(snap.cfg.swap.liquidator_fee_bps);
                repay_usd + gross_profit_usd * U256::from(fee_bps) / U256::from(10_000u64)
            }
            _ => repay_usd + gross_profit_usd,
        };

        let amount_in = liquidator_usd * one / coll.price
            * U256::from(amount_in_buffer_bps) / U256::from(10_000u64);
        let expected_loan_out = liquidator_usd * one / loan.price
            * U256::from(amount_in_buffer_bps) / U256::from(10_000u64);

        let min_out = apply_slippage(expected_loan_out, snap.cfg.swap.slippage_bps);
        let deadline = U256::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                + 600,
        );

        let router: Address = if snap.cfg.swap.router.is_empty() {
            crate::swap::AERODROME_ROUTER.parse()?
        } else {
            snap.cfg.swap.router.parse()?
        };

        let (target, data) = build_aerodrome_swap(crate::swap::SwapParams {
            router,
            from_token: coll.underlying,
            to_token: loan.underlying,
            amount_in,
            amount_out_min: min_out,
            recipient: snap.executor,
            stable_pair: is_stable_symbol(loan.symbol.as_str())
                && is_stable_symbol(coll.symbol.as_str()),
            deadline,
        })?;

        let min_loan_out = min_out.saturating_sub(repay);
        info!(%amount_in, %expected_loan_out, %min_out, from = %coll.symbol, to = %loan.symbol, "swap disiapkan");
        Ok((target, data, min_loan_out))
    }

impl<P: Provider + Clone + Send + Sync + 'static> ScanJob<P> {
    async fn cap_by_max_position(
        &self,
        snap: &ScanSnapshot,
        market: Address,
        repay: U256,
    ) -> Result<U256> {
        let Some(info) = snap.markets.get(&market) else { return Ok(repay) };
        let cap_usd = U256::from(snap.cfg.max_position_usd) * U256::from(10u64.pow(18));
        if info.price == U256::ZERO {
            return Ok(repay);
        }
        let cap_underlying = cap_usd * U256::from(10u64.pow(18)) / info.price;
        Ok(repay.min(cap_underlying))
    }
}

impl<P> Strategy<P> {
    pub fn new(
        _provider: P,
        state: SharedState,
        cfg: Config,
        markets: HashMap<Address, MarketInfo>,
    ) -> Result<Self> {
        let executor = cfg.executor_address()?;
        Ok(Self {
            _provider: std::marker::PhantomData,
            state,
            cfg,
            markets,
            executor,
            close_factor: U256::ZERO,
            liquidation_incentive: U256::ZERO,
        })
    }

    pub fn update_comptroller_params(&mut self, close_factor: U256, incentive: U256) {
        self.close_factor = close_factor;
        self.liquidation_incentive = incentive;
    }

    pub fn scan<P2>(&self, provider: P2) -> Arc<ScanJob<P2>>
    where
        P2: Provider + Clone + Send + Sync + 'static,
    {
        Arc::new(ScanJob {
            snapshot: ScanSnapshot {
                state: self.state.clone(),
                cfg: self.cfg.clone(),
                markets: self.markets.clone(),
                executor: self.executor,
                close_factor: self.close_factor,
                liquidation_incentive: self.liquidation_incentive,
            },
            provider,
            borrowers: self.state.borrowers(),
        })
    }

    pub fn rebuild_classic_job(&self, job: &LiquidationJob) -> Result<LiquidationJob> {
        let loan_addr = job.mTokenLoan;
        let coll_addr = job.mTokenCollateral;
        let (Some(loan), Some(coll)) =
            (self.markets.get(&loan_addr), self.markets.get(&coll_addr))
        else {
            return Err(anyhow::anyhow!(
                "market {loan_addr:?}/{coll_addr:?} tak dimuat — tak bisa rebuild swap Classic"
            ));
        };

        let mut new_job = job.clone();
        new_job.mode = Mode::Classic;
        new_job.minLoanOut = U256::ZERO;
        new_job.swapTarget = Address::ZERO;
        new_job.swapData = alloy::primitives::Bytes::new();

        if self.cfg.swap.enabled && loan.underlying != coll.underlying {
            let (swap_target, swap_data, min_loan_out) =
                build_swap_parts(
                    &self.snapshot_local(),
                    loan,
                    coll,
                    job.repayAmount,
                    Mode::Classic,
                    amount_in_buffer_bps(Mode::Classic),
                )?;
            new_job.swapTarget = swap_target;
            new_job.swapData = swap_data;
            new_job.minLoanOut = min_loan_out;
            new_job.minProfit = self.cfg.min_profit_for_symbol(&loan.symbol)?;
        } else {
            new_job.minProfit = self.cfg.min_profit_for_symbol(&coll.symbol)?;
        }
        Ok(new_job)
    }

    fn snapshot_local(&self) -> ScanSnapshot {
        ScanSnapshot {
            state: self.state.clone(),
            cfg: self.cfg.clone(),
            markets: self.markets.clone(),
            executor: self.executor,
            close_factor: self.close_factor,
            liquidation_incentive: self.liquidation_incentive,
        }
    }

    pub fn update_price(&mut self, mtoken: Address, price: U256) {
        if let Some(info) = self.markets.get_mut(&mtoken) {
            info.price = price;
        }
    }

    pub fn markets_snapshot(&self) -> HashMap<Address, MarketInfo> {
        self.markets.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AccountState;

    fn addr(n: u8) -> Address {
        Address::with_last_byte(n)
    }

    fn snapshot() -> ScanSnapshot {
        let mut cfg: Config = toml::from_str(
            r#"
base_rpc_http = "https://x"
base_rpc_ws = "wss://x"
private_key = "0x"
executor_address = "0x0000000000000000000000000000000000000001"
min_profit_wei = "1000"
"#,
        )
        .unwrap();
        cfg.swap.enabled = true;
        cfg.swap.slippage_bps = 200;

        let mut markets = HashMap::new();
        markets.insert(
            addr(1),
            MarketInfo {
                underlying: addr(0x11),
                symbol: "USDC".into(),
                collateral_factor: U256::from(10u64.pow(18)),
                price: U256::from(10u64.pow(18)).pow(U256::from(2)),
                protocol_seize_share: U256::from(3 * 10u64.pow(16)),
                oev_fee_bps: Some(3000),
                oev_wrappers_feed: None,
            },
        );
        markets.insert(
            addr(2),
            MarketInfo {
                underlying: addr(0x22),
                symbol: "WETH".into(),
                collateral_factor: U256::from(10u64.pow(18)),
                price: U256::from(2000u64) * U256::from(10u64.pow(18)).pow(U256::from(2)),
                protocol_seize_share: U256::from(3 * 10u64.pow(16)),
                oev_fee_bps: Some(3000),
                oev_wrappers_feed: None,
            },
        );

        ScanSnapshot {
            state: Arc::new(AccountState::new()),
            cfg,
            markets,
            executor: addr(0x99),
            close_factor: U256::from(5 * 10u64.pow(17)),
            liquidation_incentive: U256::from(11 * 10u64.pow(17)),
        }
    }

    fn amount_in_from_swap(data: &alloy::primitives::Bytes) -> U256 {
        let bytes = data.as_ref();
        U256::from_be_slice(&bytes[4..36])
    }

    fn liquidator_usd_of(data: &alloy::primitives::Bytes, coll_price: U256, mode: Mode) -> U256 {
        let amount_in = amount_in_from_swap(data);
        let one = U256::from(10u64.pow(18));
        amount_in * coll_price * U256::from(10_000u64) / (U256::from(amount_in_buffer_bps(mode)) * one)
    }

    #[test]
    fn rebuild_classic_pakai_entitas_sitaan_penuh() {
        let snap = snapshot();
        let repay = U256::from(10_000u64) * U256::from(10u64.pow(6));
        let oev = build_swap_parts(
            &snap, &snap.markets[&addr(1)], &snap.markets[&addr(2)],
            repay, Mode::Oev, amount_in_buffer_bps(Mode::Oev),
        ).unwrap();
        let classic = build_swap_parts(
            &snap, &snap.markets[&addr(1)], &snap.markets[&addr(2)],
            repay, Mode::Classic, amount_in_buffer_bps(Mode::Classic),
        ).unwrap();
        assert!(classic.0 != Address::ZERO && oev.0 != Address::ZERO);
        let coll_price = snap.markets[&addr(2)].price;
        let oev_usd = liquidator_usd_of(&oev.1, coll_price, Mode::Oev);
        let classic_usd = liquidator_usd_of(&classic.1, coll_price, Mode::Classic);
        assert!(classic_usd > oev_usd);
        assert!(classic.2 <= oev.2);
    }

    #[test]
    fn rebuild_dengan_swap_nonaktif_tidak_pakai_swap() {
        let mut snap = snapshot();
        snap.cfg.swap.enabled = false;
        let repay = U256::from(10_000u64) * U256::from(10u64.pow(6));
        let out = build_swap_parts(
            &snap, &snap.markets[&addr(1)], &snap.markets[&addr(2)],
            repay, Mode::Classic, amount_in_buffer_bps(Mode::Classic),
        ).unwrap();
        assert_eq!(out.0, Address::ZERO);
        assert_eq!(out.1.len(), 0);
        assert_eq!(out.2, U256::ZERO);
    }
}
