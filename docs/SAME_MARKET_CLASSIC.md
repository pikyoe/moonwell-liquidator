# Same-market Classic liquidation

Branch: `feat/same-market-classic` (not on `main` until reviewed).

## Behavior

Implemented in `app/src/strategy.rs` (`ScanJob::evaluate`):

1. Prefer collateral in a **different** mToken market from the loan market (OEV-safe).
2. If the only collateral is in the **same** market as the largest borrow, still build a job.
3. Force **`Mode::Classic`** for same-market jobs (OEV `updatePriceEarlyAndLiquidate` often rejects `mTokenCollateral == mTokenLoan`).
4. **Morpho flashloan** is unchanged: borrow `loanToken` → `liquidateBorrow` → redeem mToken → same underlying repays Morpho; **no swap** when underlyings match.

## Review / merge

```bash
git fetch origin
git checkout feat/same-market-classic
cargo test -p moonwell_liquidator   # or workspace test target
# review diff on app/src/strategy.rs, then merge PR when ready
```

## Before production use

- Confirm shortfall > 0 on-chain before treating a job as real (`getAccountLiquidity`).
- Watch `simulasi revert` rate for same-market jobs after deploy.
- Optional: fork/`eth_call` one same-market `liquidateBorrow` on Moonwell Base.
