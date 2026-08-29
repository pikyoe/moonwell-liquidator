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
        // Konkurensi minimal 1 — bila config set eval_concurrency=0 semaphore
        // jadi 0 permit dan semua task block selamanya. Konsisten dengan
        // indexer (.max(1)) dan submitter (.max(1)).
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

    /// Bangun satu LiquidationJob untuk satu borrower bila posisinya sehat
    /// untuk dilikuidasi OEV. Tidak memegang mutex strategy — semua input
    /// dibaca dari `self.snapshot` (cloned) dan state DashMap (aman konkuren).
    async fn evaluate(&self, borrower: Address) -> Result<Option<LiquidationJob>> {
        let snap = &self.snapshot;
        let Some(positions) = snap.state.positions.get(&borrower) else {
            return Ok(None);
        };

        // susun tuple untuk health engine
        let mut tuples = Vec::new();
        for m in positions.iter() {
            tuples.push((*m.key(), m.mtoken_balance, m.borrow_balance, m.exchange_rate));
        }
        let (_, _, hf) = health_factor(&tuples, &snap.markets);
        if classify(hf) != Health::Liquidatable {
            return Ok(None);
        }

        // pilih market pinjaman terbesar sebagai target repay.
        let mut best_loan: Option<(Address, U256)> = None;
        for m in positions.iter() {
            let market = *m.key();
            let Some(info) = snap.markets.get(&market) else { continue };
            if m.borrow_balance > U256::ZERO {
                let v = info.borrow_value_usd(m.borrow_balance);
                if best_loan.as_ref().map(|(_, bv)| v > *bv).unwrap_or(true) {
                    best_loan = Some((market, m.borrow_balance));
                }
            }
        }
        let (mloan, _stale_borrow) = match best_loan {
            Some(x) => x,
            None => return Ok(None),
        };

        // Pilih market kolateral terbesar sebagai sumber sitaan, DENGAN syarat
        // market-nya BERBEDA dari mloan. Likuidasi dengan mloan == mcoll selalu
        // revert (wrapper mencoba men-seize kolateral dari market pinjaman itu
        // sendiri) — sebelumnya best_loan & best_coll bisa jatuh di market yang
        // sama, menghasilkan peluang yang pasti gagal.
        let mut best_coll: Option<(Address, U256)> = None;
        for m in positions.iter() {
            let market = *m.key();
            if market == mloan {
                continue;
            }
            let Some(info) = snap.markets.get(&market) else { continue };
            if m.mtoken_balance > U256::ZERO {
                let v = info.collateral_value_usd(m.mtoken_balance, m.exchange_rate);
                if best_coll.as_ref().map(|(_, bv)| v > *bv).unwrap_or(true) {
                    best_coll = Some((market, v));
                }
            }
        }
        let (mcoll, _) = match best_coll {
            Some(x) => x,
            None => {
                // Tak ada kolateral di market selain mloan → tidak bisa
                // dilikuidasi secara valid.
                debug!(
                    ?mloan,
                    "tidak ada kolateral berbeda dari market pinjaman — skip"
                );
                return Ok(None);
            }
        };
        // Lepaskan read guard pada map positions SEBELUM upsert() — upsert
        // mengambil write lock pada shard yang sama; menahannya di sini akan
        // deadlock tepat saat kandidat likuidasi ditemukan.
        drop(positions);

        // Refresh posisi kandidat di kedua market — snapshot state bisa basi
        // (bunga terakru, likuidasi lain). Bangun job dari data segar. Kedua
        // market di-accrue-dulu dalam SATU batch + snapshot borrower —
        // `getAccountSnapshot` view TIDAK meng-accrue bunga, jadi state
        // on-chain yang di-accrue lewat `Multicall3` memberi nilai eksak
        // (bukan proyeksi off-chain) sambil hanya 1 round-trip RPC.
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
            warn!(
                ?borrower, got = res.len(),
                "refresk kandidat multicall tidak lengkap — skip"
            );
            return Ok(None);
        }
        for (i, market)in [mloan, mcoll].into_iter().enumerate() {
            let acc = &res[i];
            let r = &res[2 + i];
            // `accrueInterest()` mengembalikan uint256 error code — call yang
            // "sukses" tetap bisa membawa kode non-zero (state tak terakru penuh.
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

        // Pilih moda eksekusi berdasarkan ketersediaan wrapper OEV pada market
        // KOLATERAL. Feed yang bukan ChainlinkOEVWrapper (wstETH/rETH/weETH)
        // TIDAK punya updatePriceEarlyAndLiquidate — eksekusi OEV pasti revert,
        // jadi untuk market itu langsung pakai Classic (tanpa menunggu fallback
        // dari submitter yang menambah satu round-trip sim).
        let mode = if coll_info.oev_fee_bps.is_some() {
            Mode::Oev
        } else {
            debug!(
                ?mcoll,
                coll_symbol = %coll_info.symbol,
                "kolateral tanpa wrapper OEV — gunakan jalur Classic"
            );
            Mode::Classic
        };

        // Bangun parameter swap (kalau diaktifkan dan asetnya beda).
        let (swap_target, swap_data, min_loan_out) =
            self.build_swap(snap, &loan_info, &coll_info, repay, mode)?;

        // Kontrak mengukur profit di token hasil akhir: loan token bila swap
        // aktif, kolateral bila tidak. Ambil ambang per simbol agar desimal
        // (WETH 18 vs USDC 6) tidak membuat ambang salah besar.
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

        info!(
            ?borrower,
            ?mloan,
            ?mcoll,
            loan_symbol = %loan_info.symbol,
            coll_symbol = %coll_info.symbol,
            %repay,
            "peluang terdeteksi"
        );
        Ok(Some(job))
    }

    /// Hitung estimasi sitaan liquidator (split sesuai mode) lalu bangun calldata
    /// swap kolateral -> loanToken. Mengembalikan (target, calldata, minLoanOut).
    /// Jika swap nonaktif atau aset sama: (ZERO, kosong, 0).
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

/// Versi bebas (tanpa &self) dari logika swap — dipakai oleh evaluator
/// (`ScanJob::build_swap`) dan oleh `Strategy::rebuild_classic_job` untuk
/// fallback OEV->Classic yang menghitung ulang calldata sesuai mode.
/// Buffer amount_in & expected_loan_out dalam basis point (dari estimasi
/// sitaan) — diterapkan sama pada estimasi output agar guard minLoanOut
/// konsisten dengan input yang di-buffer.
///   OEV     : 100% (wrapper updatePriceEarlyAndLiquidate menyegarkan
///             harga on-chain tepat sebelum likuidasi, jadi estimasi akurat).
///   Classic :  95% (jalur B TIDAK meng-update harga; estimasi memakai
///             harga oracle cache (refresh 10-blok. Harga kolateral bisa
///             turun sejak refresh → sitaan aktual < estimasi → amount_in yang
///             terlalu besar membuat swap revert di router (likuidasi hilang).
///             Buffer 5% menyerap geseran harga sampai 5%; guard output
///             ikut di-buffer agar swap tidak revert pada amountOutMin).
fn amount_in_buffer_bps(mode: Mode) -> u64 {
    match mode {
        Mode::Oev => 10_000,
        Mode::Classic => 9_500,
        // `sol!` menambah varian sentinel `__Invalid` untuk nilai enum tak dikenal.

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

        // Keluar sampai liquidation incentive sudah dimuat (tak teratur bila 0).
        if snap.liquidation_incentive.is_zero() {
            return Ok((Address::ZERO, zero.clone(), U256::ZERO));
        }

        let one = U256::from(10u64.pow(18));
        // nilai repay dalam USD (1e18)
        let repay_usd = repay * loan.price / one;
        // Sitaan bruto dikurangi bagian protokol (protocolSeizeShareMantissa) —
        // estimasi tanpa koreksi ini membuat amount_in melebihi saldo aktual.
        let seized_usd = repay_usd * snap.liquidation_incentive / one
            * (one - coll.protocol_seize_share) / one;
        let gross_profit_usd = seized_usd.saturating_sub(repay_usd);

        // Beda moda:
        //  - OEV:     liquidator dapat repay + profit * liquidatorFeeBps/10000
        //            (wrapper membagi sisa dengan feeRecipient protokol).
        //  - Classic: liquidator menerima SEMUA profit (tidak ada split).
        let liquidator_usd = match mode {
            Mode::Oev => {
                let fee_bps = coll.oev_fee_bps.unwrap_or(snap.cfg.swap.liquidator_fee_bps);
                repay_usd
                    + gross_profit_usd * U256::from(fee_bps) / U256::from(10_000u64)
            }
            // Classic maupun variant tak dikenal dianggap penuh (tanpa split).
            _ => repay_usd + gross_profit_usd,
        };

        // konversi ke unit underlying kolateral & estimasi hasil dalam loanToken
        // amount_in dikurangi buffer (lihat amount_in_buffer_bps) agar tidak
        // melebihi saldo aktual pasca-split yang bergeser dari harga cache.
        let amount_in = liquidator_usd * one / coll.price
            * U256::from(amount_in_buffer_bps) / U256::from(10_000u64);
        // expected_loan_out ikut di-buffer SELARAS dengan amount_in: hanya
        // 95% sitaan estimasi yang benar-benar di-swap (sisa kolateral tak
        // ter-swap tidak dihitung sebagai profit di mode swap), jadi menjanjikan
        // minLoanOut berdasarkan nilai penuh membuat likuidasi yang masih profitabel
        // (harga bergeser < buffer) ditolak guard. minProfit (config) tetap
        // menjadi lantai ekonomi yang sebenarnya.

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

        // kontrak menuntut balance >= assets + minLoanOut; sisanya jadi profit
        let min_loan_out = min_out.saturating_sub(repay);

        info!(
            %amount_in, %expected_loan_out, %min_out,
            from = %coll.symbol, to = %loan.symbol,
            "swap disiapkan"
        );
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
        // konversi USD cap ke unit underlying: cap_usd / price * 10^decimals
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
        // 0 bila belum di-refresh — evaluate() akan skip sampai ada nilai.
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

    /// Panggilan satu kali dari refresh_prices: muat close factor &
    /// liquidation incentive dari comptroller — jangan di-hardcode.
    pub fn update_comptroller_params(&mut self, close_factor: U256, incentive: U256) {
        self.close_factor = close_factor;
        self.liquidation_incentive = incentive;
    }

    /// Ambil pustaka evaluator DI BAWAH lock singkat lalu lepaskan lock.
    /// Pemanggil bebas mengevaluasi semua borrower paralel lewat
    /// `ScanJob::run` — jadi penahan lock tidak lagi mencakup I/O RPC
    /// (markets/cfg/params di-clone; state dipakai via Arc DashMap).
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

    /// Rebuild field swap/profit sebuah job untuk mode lain (Classic) dari data
    /// pasar yang sama. Dipakai submitter saat fallback OEV->Classic: hanya
    /// membalik `job.mode` membuat amountIn/expectedOut selisih split OEV
    /// (repay + 30% profit) padahal kontrak redeem SELURUH sitaan pada jalur
    /// Classic (repay + 100% profit) — calldata swap kekurangan aset dan
    /// profit terukur jatuh di bawah minProfit. Di sini swapData/minLoanOut/
    /// minProfit dihitung ulang sesuai mode tanpa scan/RPC.
    ///
    /// Err bila data pasar untuk token loan/kolateral tidak dimuat — pemanggil
    /// TIDAK boleh mengirim dengan field swap yang basi.
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
            // Dengan swap, profit diukur di loan token.
            new_job.minProfit = self.cfg.min_profit_for_symbol(&loan.symbol)?;
        } else {
            // Tanpa swap profit diukur di kolateral; ambang untuk simbol kolateral.
            new_job.minProfit = self.cfg.min_profit_for_symbol(&coll.symbol)?;
        }
        Ok(new_job)
    }

    // Snapshot field strategy sebagai ScanSnapshot sehingga build_swap_parts
    // (yang menerima &ScanSnapshot) bisa dipakai dari rebuild_classic_job
    // tanpa mengubah signature evaluasi. Clone memori murni, tanpa await.
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

    /// Snapshot markets ter-lock saat ini (harga terbaru hasil refresh_prices),
    /// dipakai pemanggil yang butuh MarketInfo segar tanpa memegang lock lama
    /// (mis. sweep_marginal_borrowers) — clone di bawah lock singkat, tanpa I/O.
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

    /// Snapshot dengan dua market: mUSDC (loan, harga 1 USD) & mWETH
    /// (kolateral, harga 2000 USD), incentive 1.1e18, seize share 3%.
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
                price: U256::from(10u64.pow(18)).pow(U256::from(2)), // 1e36
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
                price: U256::from(2000u64) * U256::from(10u64.pow(18)).pow(U256::from(2)), // 2000e36
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

    /// Fallback OEV->Classic harus membangun ulang swap dengan entitas Classic
    /// (men-redeem SELURUH sitaan: liquidator menerima 100% profit, bukan
    /// split OEV 30%). Kalau tidak, calldata swap menukar terlalu sedikit
    /// relatif terhadap sitaan & profit < minProfit sehingga fallback selalu gagal.
    /// (Fix PR #14 comment #1.)
    ///
    /// Invariant yang benar BUKAN "Classic menukar lebih banyak dari OEV":
    /// buffer amount_in Classic (95%) bisa lebih besar dari selisih split profit
    /// (buffer 5% × nilai-sitaan vs 70% × profit-tipis), sehingga jumlah
    /// kolateral yang ditukar Classic bisa saja lebih kecil dari OEV. Yang
    /// dijamin rebuild: (1) swap disiapkan untuk entitas sitaan penuh
    /// (dikurangi buffer pengaman agar tidak melebihi saldo aktual);
    /// (2) `minLoanOut` longgar (tidak memblokir likuidasi yang masih
    /// profitabel — guard sesungguhnya minProfit).
    fn amount_in_from_swap(data: &alloy::primitives::Bytes) -> U256 {
        // swapExactTokensForTokens(uint256 amountIn, ...): amountIn = arg pertama
        // (word-32 setelah selector 4B).
        let bytes = data.as_ref();
        U256::from_be_slice(&bytes[4..36])
    }

    /// liquidator_usd (skala 1e18) yang dikodekan ke dalam swap untuk satu
    /// mode — dibalik dari amount_in & buffer, membuktikan entitas yang dipakai.
    fn liquidator_usd_of(data: &alloy::primitives::Bytes, coll_price: U256, mode: Mode) -> U256 {
        let amount_in = amount_in_from_swap(data);
        // amount_in = liquidator_usd * buffer(mode)/10000 / coll_price → balik.

        let one = U256::from(10u64.pow(18));
        amount_in * coll_price * U256::from(10_000u64) / (U256::from(amount_in_buffer_bps(mode)) * one)
    }

    #[test]
    fn rebuild_classic_pakai_entitas_sitaan_penuh() {
        let snap = snapshot();
        // repay ~ $10.000 di mUSDC, collateral mWETH.
        let repay = U256::from(10_000u64) * U256::from(10u64.pow(6)); // USDC 6 desimal

        let oev = build_swap_parts(
            &snap,
            &snap.markets[&addr(1)],
            &snap.markets[&addr(2)],
            repay,
            Mode::Oev,
            amount_in_buffer_bps(Mode::Oev),).unwrap();
        let classic = build_swap_parts(
            &snap,
            &snap.markets[&addr(1)],
            &snap.markets[&addr(2)],
            repay,
            Mode::Classic,
            amount_in_buffer_bps(Mode::Classic),).unwrap();

        assert!(
            classic.0 != Address::ZERO && oev.0 != Address::ZERO,
            "swap aktif untuk kedua mode"
        );

        // Rebuild harus menyiapkan swap untuk entitas sitaan penuh (Classic:
        // repay + 100% profit), bukan entitas OEV (30% split). Terlihat dari
        // liquidator_usd ter-rekonstruksi (Classic ≈ $10.667k > OEV ≈ $10.201k).
        let coll_price = snap.markets[&addr(2)].price;
        let oev_usd = liquidator_usd_of(&oev.1, coll_price, Mode::Oev);
        let classic_usd = liquidator_usd_of(&classic.1, coll_price, Mode::Classic);
        assert!(
            classic_usd > oev_usd,
            "Classic harus mengkodekan entitas sitaan penuh ({} > {}",
            classic_usd, oev_usd
        );

        // minLoanOut Classic diharapkan longgar (tidak lebih ketat dari OEV);
        // guard ekonomi yang sebenarnya ada di minProfit (config). Jika guard
        // minLoanOut malah lebih ketat dari OEV, likuidasi profitabel bisa
        // diblokir (fix review PR #16).
        assert!(
            classic.2 <= oev.2,
            "minLoanOut Classic harus longgar (tidak > OEV): {} vs {}",
            classic.2, oev.2
        );
    }

    /// Bila swap nonaktif, rebuild menonaktifkan swapTarget dan memakai ambang
    /// minProfit kolateral — job Classic tetap valid.
    #[test]
    fn rebuild_dengan_swap_nonaktif_tidak_pakai_swap() {
        let mut snap = snapshot();
        snap.cfg.swap.enabled = false;
        let repay = U256::from(10_000u64) * U256::from(10u64.pow(6));
        let out = build_swap_parts(
            &snap,
            &snap.markets[&addr(1)],
            &snap.markets[&addr(2)],
            repay,
            Mode::Classic,
            amount_in_buffer_bps(Mode::Classic),).unwrap();
        assert_eq!(out.0, Address::ZERO);
        assert_eq!(out.1.len(), 0);
        assert_eq!(out.2, U256::ZERO);
    }
}
