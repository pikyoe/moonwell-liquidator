# Moonwell Liquidator — Repo Knowledge

## Build & Test
- Kontrak: `cd contracts && forge build && forge test` (fork test ke Base mainnet via `BASE_RPC_URL`, default publik). Semua 9 test harus lulus.
- Bot: `cd app && cargo build` (debug) / `cargo build --release`. Jalankan `cargo clippy` sebelum commit.
- Test fork terakhir diverifikasi: 11/11 PASS (forge 1.7.1, solc 0.8.24), termasuk 2 e2e happy path (borrower dibuat underwater via mockCall oracle; OEV path diuji dengan FakeOevWrapper yang di-etch ke alamat wrapper asli).

## Arsitektur
- `contracts/src/OevLiquidator.sol` — executor. Flashloan Morpho Blue (0xBBBB...FFCb di Base). Guard callback: `expectedCallHash` hanya diset oleh `execute()` (onlyOwner); callback reject hash tak dikenal.
- Jalur A (OEV): ChainlinkOEVWrapper lewat `comptroller.oracle().getFeed(symbol)`. Jalur B (Classic): `liquidateBorrow` langsung ke mToken.
- Bot Rust: indexer (event Borrow/Mint/Redeem/Transfer/LiquidateBorrow) → health (HF off-chain) → strategy (bangun LiquidationJob + calldata swap Aerodrome) → submitter (eth_call dulu, baru send). State DashMap in-memory + snapshot.json tiap 100 blok.

## Alamat on-chain terverifikasi (Base, 2026-08-26)
- Comptroller: `0xfBb21d0380beE3312B33c4353c8936a0F13EF26C` → oracle `0xEC942bE8A8114bFD0396A5052c36027f2cA6a9d0`
- OEV wrapper WETH: `0x57DA741aD933869cC9EBfb9668288053A0738f3c`; `liquidatorFeeBps = 3000` (cocok dengan default config 30%)
- `closeFactorMantissa = 0.5e18`; `liquidationIncentiveMantissa = 1.1e18`
- Morpho Blue: `0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb`
- Aerodrome router `0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43`, factory `0x420DD381b31aEf6683db6B902084cB0FFECe40Da`

## Gotchas
- Bot harus memakai signer yang SAMA dengan owner kontrak executor; alloy `call()` mengisi `from` = signer (WalletFiller). Kalau beda, semua simulasi revert "not owner".
- Jangan commit `app/config.toml` / `.env` (sudah di-.gitignore). `contracts/broadcast/` juga di-ignore.
- `mWETH` Moonwell mengirim **ETH native** saat `redeem` (unwrapper 0x1382...4cAf via `.send`, 2300 gas) — executor punya `receive()` ringan + wrap balik ke WETH di callback. Tanpa ini semua likuidasi berkolateral WETH revert.
- `markets()` comptroller on-chain mengembalikan 2 field (isListed, collateralFactor) — BUKAN 3. Binding 3 field gagal decode → CF 0.
- `protocolSeizeShareMantissa` = 3e16 (3%): sitaan yang diterima liquidator = 97% dari `liquidateCalculateSeizeTokens`. Bot membacanya on-chain saat startup (MarketInfo.protocol_seize_share) agar estimasi amount_in swap tidak melebihi saldo aktual.
- Fee OEV (`liquidatorFeeBps`) dibaca on-chain per market saat startup; config hanya fallback.

## Audit 2026-08-26 (semua temuan sudah difix & diverifikasi)
- HIGH (fixed): mWETH redeem mengirim ETH native → receive() + wrap WETH + sweepEth. Ditemukan via test e2e.
- HIGH (fixed): binding markets() 3 field padahal on-chain 2 field → CF selalu 0 → semua borrower tampak liquidatable. Binding dikoreksi.
- MEDIUM (fixed): trigger UpdatedPrices kini refresh harga dulu; kandidat di-refresh via getAccountSnapshot sebelum bangun job; guard max_gas_cost_wei di submitter; estimasi swap dikurangi protocol seize share; fee OEV dibaca on-chain; bootstrap retry dengan chunk menyusut.
- LOW (fixed): happy-path e2e test untuk kedua jalur; event `Liquidated` di kontrak.
- Sisa terbuka (diterima): wrapper OEV di-resolve dinamis via getFeed (risiko governance Moonwell); private key plaintext di config.toml.
- Verifikasi akhir: forge 11/11 PASS, cargo 11/11 PASS, clippy 0 warning.
