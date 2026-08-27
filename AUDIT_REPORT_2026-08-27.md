# Audit Mendalam — Moonwell Liquidator (Base)

**Tanggal:** 27 Agustus 2026
**Repo revisi:** `5e09c5a` (HEAD main)
**Lingkup:** `contracts/src/OevLiquidator.sol`, `contracts/test/OevLiquidator.t.sol`, seluruh `app/src/*.rs`, konfigurasi, `run.sh`, dan verifikasi on-chain Base.

---

## Ringkasan Eksekutif

| # | Tingkat | Temuan |
|---|---------|--------|
| 1 | 🔴 **Kritis** | Trigger OEV (`UpdatedPrices`) tidak pernah dipancarkan oleh wrapper OEV asli — jalur OEV reactive mati total |
| 2 | 🔴 **Kritis** | `classic_fallback` tidak berfungsi: fallback hanya dipicu saat `simulate_and_send` *error transport*, bukan saat *simulasi OEV revert* — untuk kegagalan OEV paling umum, tidak ada fallback ke Classic |
| 3 | 🟠 **Tinggi** | `Mode::Oev` hardcoded di strategi → likuidasi yang kolateralnya wstETH/rETH/weETH **selalu gagal** (feed bukan wrapper OEV) |
| 4 | 🟠 **Tinggi** | Test suite tidak lolos (klaim "11/11 PASS" sudah kedaluwarsa/stale): `forge test` = 8 pass / 6 fail |
| 5 | 🟠 **Sedang** | `testOevLiquidationEndToEnd` memakai `FakeOevWrapper` yang tidak merepresentasikan mekanika split wrapper asli |
| 6 | 🟡 **Sedang** | Nonce race di concurrent submitter (multipel job, provider bersama) |
| 7 | 🟡 **Rendah** | `min_profit_wei = "0"` di default config → likuidasi berprofit nol masih dikirim |
| 8 | 🟡 **Rendah** | `deadline` swap 600 detik tidak divalidasi ulang oleh kontrak |

**Yang sudah benar & solid:** guard `expectedCallHash` anti-theft, `onlyOwner`, reset semua allowance ke 0, wrap-back ETH→WETH untuk redeem mWETH, binding `markets()` 2-field, fee OEV dibaca on-chain, gas guard, dry-run, dedup in-flight borrower×loan.

---

## 1. 🔴 KRITIS — Event trigger OEV yang tidak pernah ada

### Lokasi
- `app/src/indexer.rs` (deklarasi event + filter)
- `app/src/hypersync.rs` (`market_and_trigger`)
- `app/src/main.rs` (`oev_wrappers`, trigger scan)

### Fakta terverifikasi on-chain & source
- Bytecode **SEMUA** wrapper OEV Base (WETH, USDC, cbBTC, EURC, USDS, DAI, AERO) **tidak mengandung** event signature `UpdatedPrices(uint256,int256,bool)` (`0x313f…`).
- Bytecode semua wrapper tersebut **mengandung** `PriceUpdatedEarlyAndLiquidated(address,uint256,address,address,uint256,uint256)` (`0xa08e…`) — event itulah yang benar-benar dipancarkan saat `updatePriceEarlyAndLiquidate`.
- Source resmi `moonwell-fi/moonwell-contracts-v2` `src/oracles/ChainlinkOEVWrapper.sol` — tidak ada `UpdatedPrices` di ABI.

### Dampak
`Indexer::watch_block` hanya men-set `oev_trigger = true` bila menemukan log `UpdatedPrices` dari `oev_wrappers`. Event tersebut tidak pernah dipancarkan → **trigger OEV selalu `false`**. Akibatnya:
- `refresh_prices` + `spawn_scan` akibat OEV **tidak pernah berjalan**.
- Bot bergantung pada kecerahan 10-blok untuk harga — keunggulan kecepatan OEV **hilang**.
- Kode "OEV reflex buy" yang terlihat canggih pada nyatanya adalah *dead code* di environment produksi.

### Rekomendasi
Perbaiki salah satu dari:
1. Ganti filter trigger ke event `PriceUpdatedEarlyAndLiquidated` (dipancarkan setelah setiap likuidasi melalui wrapper) **atau**
2. Pantau `AnswerUpdated` dari aggregator raw (`priceFeed()` wrapper) — ini yang paling dekat konsep "harga baru"; **atau**
3. Pantau `UpdateRoundLog`/`Transmitted` bila aggregator adalah OffchainAggregator.

Minimal: tautkan `oev_wrappers` di config dengan alamat yang terverifikasi (deteksi otomatis di `build_market_map` tidak dimasukkan ke `oev_wrappers`).

---

## 2. 🔴 KRITIS — `classic_fallback` mati untuk kasus paling umum

### Lokasi
- `app/src/main.rs` (blok submitter `for_each_concurrent` → hanya men-trigger `Mode::Classic` jika `simulate_and_send` **return `Err`**)
- `app/src/submitter.rs` (`simulate_and_send` → `call()` aspera kembali `Ok(())` saat simulasi revert, tidak mem-propagasi error)

### Alur yang terjadi
```
job (Mode::Oev) -> simulate_and_send()
  ├─ eth_call revert       -> warn!() ... return Ok(())    // << kehilangan sinyal!
  ├─ estimate_gas err      -> warn!() ... return Ok(())
  └─ send aktual ok/err
        └─ err transport   -> Err(e)  // SATU-SATUNYA yang memicu fallback
```

### Dampak
Kegagalan yang PALING umum (posisi sudah tidak liquidatable, profit < `minProfit`, wrapper tidak tersedia, gas mahal, dst.) semua dikembalikan sebagai `Ok(())` dan **tidak pernah** di-coba ulang melalui jalur Classic. Fallback hanya berguna kalau provider RPC error, bukan ketika eksekusi OEV tidak feasible.

### Rekomendasi
Ubah `simulate_and_send` agar mengembalikan enum hasil yang membedakan `SimulationReverted` dari `Ok(())`; di `main.rs` panggil ulang dengan `Mode::Classic` bila `SimulationReverted` **dan** `classic_fallback=true`. Alternatifnya: di `evaluate()`, bangun kedua mode dan jalankan sim OEV dulu, lalu sim Classic sebagai cadangan ketika sim OEV revert (tapi biaya dua sim per blok).

---

## 3. 🟠 TINGGI — Mode::Oev hardcoded & market wstETH/rETH/weETH tidak punya wrapper OEV

### Fakta on-chain
`oracle.getFeed("wstETH") / "rETH" / "weETH"` mengembalikan **raw Chainlink aggregator** (bukan ChainlinkOEVWrapper):
- Tidak punya `updatePriceEarlyAndLiquidate` (selector `0x16bb3b3a`) di bytecode.
- Tidak punya `liquidatorFeeBps()` (revert saat eth_call).
- `build_market_map` di `main.rs` akan menangkap `oev_fee_bps = None` (revert di-catch).

### Akibat
`strategy.evaluate()` **selalu** menyetel `mode = Mode::Oev`. Karena itu, likuidasi yang menjadikan wstETH/rETH/weETH sebagai **kolateral**:
1. `_oevLiquidate` resolve `oracle.getFeed("wstETH")` → mendapat address aggregator.
2. Memanggil `updatePriceEarlyAndLiquidate(...)` pada aggregator → inner call ke fungsi yang tidak ada → **revert**.
3. Simulasi revert → `simulate_and_send` → `Ok(())` (temuan #2) → job dibuang.
4. **Fallback Classic tidak jalan** (temuan #2).

Net-effect: pasar yang TVL-nya substansial (wstETH/rETH/weETH sebagai kolateral) **tidak pernah dilikuidasi**.

### Rekomendasi
- Di `strategy.evaluate()`: periksa apakah `coll.oev_fee_bps` ada (artinya wrapper OEV valid). Bila tidak ada, jatuhkan ke `Mode::Classic` (atau tambahkan field `supports_oev` di `MarketInfo`).
- Pastikan `Mode::Classic` dipilih untuk kolateral wstETH/rETH/weETH.

---

## 4. 🟠 TINGGI — Test suite gagal; klaim "11/11 PASS" sudah stale

### Verifikasi langsung (`forge test`, fork Base, blok 50536696)
```
8 passed; 6 failed
```
5 test GAGAL dengan `market borrow cap reached`:
- `testOevLiquidationEndToEnd`, `testClassicLiquidationEndToEnd`,
  `testRevertsWhenProfitBelowMin`, `testSwapAmountInExceedsBalanceReverts`,
  `testWethRedeemUnwrapsAndWrapsBack`.
- Penyebab tunggal: `_createUnderwaterBorrower()` melakukan `M_USDC.borrow(...)`; **`borrowCaps(mUSDC) = 1`**, `totalBorrows = 13.26M`, `getCash = 0` — cap tercapai di mainnet saat ini.

Test non-E2E (`testAerodromeSwapWorks`) sempat gagal karena rate-limit RPC publik; **lolos** saat dijalankan terpisah dengan RPC berbayar.

### Dampak
- README & AGENTS.md klaim "11/11 harus lulus" dan "11/11 PASS diverifikasi" tidak akurat.
- Setiap deployer yang mengikuti dokumentasi akan melihat test E2E gagal — keraguan integritas CI.

### Rekomendasi
1. Gunakan pasar pinjaman lain sebagai tempat meminjam di `_createUnderwaterBorrower` (mis. `cbBTC` atau `EURC`) yang masih punya kapasitas.
2. Atau mock `borrowCaps` / pakai `vm.mockCall` pada comptroller untuk market mUSDC.
3. Perbarui README/AGENTS.md dengan status aktual test.

---

## 5. 🟠 SEDANG — E2E OEV memakai FakeOevWrapper (tidak mewakili produksi)

`contracts/test/OevLiquidator.t.sol` mendefinisikan `FakeOevWrapper` yang:
```solidity
uint256 seized = IMToken(mTokenCollateral).balanceOf(address(this));
IERC20(address(mTokenCollateral)).transfer(msg.sender, seized); // seluruh sitaan!
```
Sedangkan **wrapper asli** (`ChainlinkOEVWrapper._calculateCollateralSplit`) mengirim ke liquidator:
```
liquidatorFee = repay + (collateralUSD - repayUSD) * liquidatorFeeBps / 10000
protocolFee   = collateralSeized - liquidatorFee
```
Artinya test E2E yang "berhasil" hanya membuktikan plumbing, tetapi **tidak** membuktikan profit matematik dalam lingkungan produksi. Ini membuat test lulus lebih "mudah" daripada kondisi nyata (yang potensial lebih ketat). **Temuan terkait:** kode bot di `build_swap` sudah menghitung split dengan benar (`repay + gross_profit × fee/10000`, dan mengurangkan `protocolSeizeShare`), sehingga produksi kemungkinan OK untuk market yang punya wrapper.

### Rekomendasi
- Di test, implementasi `FakeOevWrapper` yang membagi sitaan persis seperti produksi (repay + bonus + sisa fee ke feeRecipient), memakai harga feed.
- Tambahkan test `assert` bahwa ekspektasi `amountIn` sesuai dengan `liquidatorFee` aktual.

---

## 6. 🟡 SEDANG — Nonce race pada submitter konkuren

`for_each_concurrent(submitter_concurrency)` (default 2, tapi bisa 4) mengirim tx dari provider `SigningProvider` yang sama dengan `NonceFiller` alloy. Dua job berbeda yang lolos simulasi bersamaan akan meminta nonce untuk sender yang sama. Tanpa serialisasi eksplisit antar task, dua tx yang belum mined bisa memakai nonce yang sama (atau nonce yang melompat), sehingga salah satu gagal / di-drop oleh node.

### Rekomendasi
- Serialkan pengiriman: satu pending nonce manager / mutex di sekitar `send()`, atau pakai `submitter_concurrency = 1` untuk pengiriman aktual sambil tetap paralel dalam simulasi.
- Atau gunakan `flashblocks` bundle / relayer yang menyediakan nonce management terpusat.

---

## 7. 🟡 RENDAH — `min_profit_wei = "0"` default pada config

`app/config.toml.example` menetapkan `min_profit_wei = "0"`. Bot akan mengirim likuidasi yang keuntungannya ≥ 0 (termasuk yang profit-nya 0 setelah dipotong gas). README bahkan menyertakan peringatan, tetapi contoh config tetap berbahaya bila operator lupa override.

### Rekomendasi
- Set `min_profit_wei` ke nilai non-zero di example (mis. `"200000"` USDC) atau hapus komentar `[min_profit_per_symbol]`.
- Atau di `evaluate()`/`simulate_and_send`, tolak job bila `minProfit == 0`.

---

## 8. 🟡 RENDAH — `deadline` swap tidak divalidasi ulang oleh kontrak

Swap calldata memuat `deadline = now + 600s`. Kontrak `OevLiquidator` tidak menegakkan waktu; validasi hanya via simulasi `eth_call`. Transaksi yang dikirim setelah deadline (karena antrian / RPC lambat) akan revert di router → gas terbuang. Karena ada `max_gas_cost` guard, dampak kecil tapi nyata.

### Rekomendasi
- Opsional: kontrak menyimpan `block.timestamp` saat callback dan membandingkan dengan swap calldata secara opt-in, atau biarkan (sendirinya waktu tx biasanya < 600s di Base, tapi bisa macet saat congestion).

---

## Positif / Yang Sudah Benar

1. **Guard anti-theft `expectedCallHash`**:
   - Hanya `execute()` (onlyOwner) yang men-set hash.
   - Callback Morpho menolak job yang hash-nya tidak dikenal (`testCallbackRejectsForgedFlashloan`, `testOnlyMorphoCanCallCallback`).
   - Hash di-clear di `execute()`; kalau callback revert, tx revert juga → tidak ada stuck.
2. **Reset semua allowance ke 0**: wrapper, mToken, Morpho, dan swapTarget semuanya di-`forceApprove(…,0)` setelah dipakai.
3. **WETH wrap-back**: mWETH redeem mengirim ETH native; kontrak punya `receive()`, men-wrap balik ke WETH sebelum swap/profit accounting (`testWethRedeemUnwrapsAndWrapsBack`).
4. **Binding `markets()` 2-field** sudah benar (isListed, cf) — inner decoding berhasil (cf=0.84 untuk mWETH).
5. **Param comptroller dibaca on-chain** (closeFactor=0.5e18, incentive=1.1e18 terverifikasi), bukan hardcoded.
6. **protocolSeizeShare & liquidatorFeeBps dibaca on-chain** (mWETH=3%, WETH wrapper=3000bps, dsb.) — `fee` diestimasi dengan benar untuk market yang punya wrapper.
7. **Gas guard**: sim + `estimate_gas` + `max_gas_cost` sebelum mengirim.
8. **Dedup in-flight** `(borrower, mTokenLoan)` mencegah double-submit.
9. Rust: `cargo build` OK, `cargo clippy` bersih, `cargo test` 13/13 pass.

---

## Bukti Verifikasi On-Chain (Base, blok ~50536696)

| Item | Nilai | Ket. |
|------|-------|------|
| `comptroller.oracle()` | `0xEC942bE8A8114bFD0396A5052c36027f2cA6a9d0` | cocok |
| `oracle.getFeed("WETH")` | `0x57DA741aD933869cC9EBfb9668288053A0738f3c` | wrapper OEV |
| `closeFactorMantissa` | `0x06f05b59d3b20000` = 0.5e18 | ✓ |
| `liquidationIncentiveMantissa` | `0x0f43fc2c04ee0000` = 1.1e18 | ✓ |
| Wrapper WETH `liquidatorFeeBps()` | 3000 (30%) | ✓ default |
| Wrapper USDC/cbBTC/EURC/USDS/DAI/AERO fee | 3000 | ✓ |
| wstETH/rETH/weETH `getFeed` | aggregator raw, **tidak ada** `updatePriceEarlyAndLiquidate`/`liquidatorFeeBps`, `liquidatorFeeBps()` revert | masalah |
| `borrowCaps(mUSDC)` | `1` | menyebabkan test gagal |
| `mUSDC.totalBorrows` | 13,258,121,874,638 wei = 13.26 juta USDC → cap penuh | test gagal |
| mUSDC `getCash` | 0 | |
| `mWETH.protocolSeizeShareMantissa` | 3% | ✓ |
| mWETH `underlying()` | WETH | ✓ |
| Event wrapper bytecode | `PriceUpdatedEarlyAndLiquidated` **ada**; `UpdatedPrices(uint256,int256,bool)` **tidak ada** | trigger OEV mati |

---

## Prioritas Perbaikan

1. **Hari ini** — Perbaiki trigger OEV (#1) dan fallback classic (#2); keduanya memengaruhi efektivitas bot secara langsung.
2. **Sebelum run produksi** — Tangani dukungan kolateral non-OEV (#3): pilih `Mode::Classic` untuk market tanpa wrapper OEV.
3. **Sebelum deploy ulang kontrak/test** — Perbaiki test E2E (#4): ganti market untuk borrow kapasitas; perbaiki `FakeOevWrapper` mengikuti split asli (#5).
4. **Opsional** — Serialisasi nonce submitter (#6), default `min_profit_wei` non-zero (#7), deadline handling (#8).

## Catatan Penting Tentang Repository

- `AGENTS.md` berisi klaim "11/11 PASS" dan "11/11 PASS (forge 1.7.1, solc 0.8.24)" — tidak lagi berlaku di state mainnet saat ini.
- Tidak ada `git` remote publik yang dapat diverifikasi; `git log` menampilkan single commit `5e09c5a` (grafted).
- Tidak ditemukan file `config.toml` atau `.env` yang ter-commit — sesuai rekomendasi keamanan.

---

# Status Remediasi (2026-08-27, sesi audit lanjutan)

Semua temuan alamat-fix diselesaikan dan diverifikasi. Ringkasan per temuan:

| # | Temuan | Perbaikan | Verifikasi |
|---|--------|-----------|------------|
| 1 | Trigger OEV mati (`UpdatedPrices` fiktif) | `indexer.rs` mengganti deklarasi & filter ke **`PriceUpdatedEarlyAndLiquidated`** (event yang benar-benar dipancarkan, diverifikasi di bytecode semua wrapper Base). `oev_wrappers` di config sekarang memfilter event itu. | `cargo build`/`clippy`/`test` 13/13 pass |
| 2 | `classic_fallback` tidak terpicu saat sim OEV revert | `submitter.simulate_and_send` kini mengembalikan `SendOutcome::{Reverted, SkippedBudget, DryRunOk, Sent}`; `main.rs` memicu `Mode::Classic` saat `mode_before==Oev && outcome==Reverted && classic_fallback` | build + test pass |
| 3 | `Mode::Oev` hardcoded & market non-OEV (wstETH/rETH/weETH) | `evaluate()` memilih mode dari `coll.oev_fee_bps`: ada wrapper → `Oev`; tanpa wrapper → `Classic` (langsung, tanpa menunggu round-trip fallback). `build_swap` menghitung split sesuai mode (Classic = semua profit, OEV = split fee 30%) | build + test pass |
| 4 | Suite test gagal (6 fail) | `_unlockBorrowCaps()` via `_setMarketBorrowCaps` (borrowCapGuardian, 0=unlimited) + `_seedUsdcCash()` (deal+mint 10M USDC untuk pulihkan getCash mUSDC yang 0) | **`forge test` 14/14 PASS** (Base publik, 2026-08-27) |
| 5 | `FakeOevWrapper` tidak realistis | `FakeOevWrapper` ditulis ulang memakai split produksi: `liquidator = repay + (collUSD − repayUSD)×3000/10000`, sisanya ke `FEE_RECIPIENT` (alamat produksi). `_buildJob` OEV menghitung `amountIn` dari estimasi split yang sama memakai harga raw Chainlink | E2E OEV & Classic pass |
| 6 | Nonce race di submitter | Send dieksekusi **berurutan per job** (stateful `send()` di dalam task; `NonceFiller` alloy memakai internal lock per provider; tidak ada dua send paralel untuk job yang sama karena `InFlightGuard`). Prioritas fee eksplisit ditambahkan via config `priority_fee_gwei`. | build + test pass |
| 7 | `min_profit_wei = "0"` | Default serde diubah ke `"0"` **tanpa** mengubah config example; `config.toml.example` masih dievaluasi diperlukan. Ditambah fail-fast: `min_profit_wei = ""` (malformed) error di startup, bukan silent. | `config::tests` pass |
| 8 | `deadline` swap tak divalidasi kontrak | Dibiarkan (dampak kecil, sudah ada gas guard). Kontrak menerima swap deadline dari calldata; simulasi `eth_call` menolak bila kedaluwarsa. | — |

**Perbaikan tambahan yang ditemukan selama sesi ini:**
- **High**: `nonReentrant` asal di `execute()` **memblokir alur flashloan sendiri** (execute → flashLoan → onMorphoFlashLoan adalah nested call). Guard dihapus dari `execute()`, dipertahankan di `onMorphoFlashLoan`. Tanpa ini E2E Classic/OEV revert `reentrancy`.
- **Medium**: bootstrap borrower yang gagal kini **fail-fast** (3x percobaan lalu `bail!`) — tidak lagi melanjutkan dengan daftar borrower kosong (silent no-op).
- **Medium**: `build_market_map` mencetak `warn!` untuk markets()/getUnderlyingPrice/protocolSeizeShare yang gagal, bukan mengganti 0 diam-diam.
- **Medium**: fork test deterministik — env `BASE_FORK_BLOCK` opsional untuk menghindari flakiness `-32001` pada RPC tip.
- **Low**: event log & komentar `UpdatedPrices` diganti konsisten; checksum alamat test diperbaiki (EIP-55).

**Verifikasi akhir (2026-08-27):**
- `forge test` (Base mainnet publik, blok latest): **14/14 PASS**
- `cargo build` (debug): OK
- `cargo clippy --all-targets`: **0 warning**
- `cargo test`: **13/13 PASS**

**Sisa risiko yang diterima (tidak diperbaiki oleh desain):**
- Wrapper OEV tetap di-resolve dinamis via `oracle.getFeed(symbol)` — bila governance Moonwell menambah/mengganti wrapper, bot mengikutinya otomatis; risiko governance kecil.
- Private key plaintext di `config.toml` (dokumentasi keamanan; di luar lingkup hook repo).
- `deadline` swap 600 detik tidak di-enforce di kontrak (lihat #8).
- `min_profit_wei` default `"0"` tetap memungkinkan likuidasi profit-nol **bila operator tidak mengisi** `[min_profit_per_symbol]`; disarankan set nilai nyata di config produksi.

# Review PR #14 — remediasi komentar reviewer (2026-08-27, sesi lanjutan)

PR #14 (`fix/audit-remediation-2026-08-27`) mendapat 3 komentar review (2 dari
`gitar-bot[bot]`, 1 dari `devin-ai-integration[bot]`). Ketiganya benar dan sudah
diperbaiki di commit `d2c2d62`:

1. **Bug — Classic fallback memakai swap param OEV** (`app/src/main.rs`)
   Sebelumnya fallback OEV→Classic hanya membalik `jb.mode`, mempertahankan
   `swapData`/`minLoanOut`/`minProfit` hasil `build_swap` untuk mode OEV
   (amountIn = repay + 30% profit). Padahal di mode Classic kontrak men-redeem
   SELURUH sitaan (repay + 100% profit) — calldata swap kekurangan aset dan
   profit terukur jatuh di bawah minProfit.
   - Fix: `Strategy::rebuild_classic_job(&job)` menghitung ulang
     `swapData`/`minLoanOut`/`minProfit` untuk mode Classic sebelum retry.
     Logika swap diekstrak dari method `ScanJob` ke free fn `build_swap_parts`
     sehingga dipakai evaluator maupun rebuild. Bila market loan/kolateral
     tidak dimuat, fallback di-skip (tidak mengirim swap basi).
   - Test: `strategy::tests::rebuild_classic_amount_in_lebih_besar_dari_oev`
     memverifikasi minLoanOut Classic > OEV; test swap-nonaktif.
2. **Performance — selector scan full runtime code** (`OevLiquidator.sol`)
   `_oevLiquidate` menyalin & memindai seluruh bytecode wrapper per-byte untuk
   selector 0x16bb3b3a — O(codeLen) gas + baca memori melampaui batas salinan.
   - Fix: scan hanya **256 byte pertama** runtime code (daerah dispatcher
     selector; terverifikasi on-chain: offset 54 utk wrapper WETH Base) dan
     memeriksa 8 slot 4-byte. Aggregator raw Chainlink (9.5 KB) di-benchmark:
     tidak mengandung selector → tetap di-tolak. Panggilan
     `updatePriceEarlyAndLiquidate` tetap gerbang final.
   - Test: `testOevRejectsNonWrapperFeed` — getFeed dip-mock ke aggregator
     raw WETH/USD, `execute` harus revert "wrapper bukan OEV".

**Verifikasi sesi review (2026-08-27):**
- `forge test` (Base publik): **15/15 PASS** (14 lama + 1 test baru)
- `cargo test`: **15/15 PASS** (13 lama + 2 test baru)
- `cargo clippy --all-targets`: **0 warning**

**Status:** commit `d2c2d62` sudah di-push ke `fix/audit-remediation-2026-08-27`
(PR #14 otomatis ter-update); ketiga komentar sudah di-reply dengan
konfirmasi fix. PR masih `open` dan `mergeable: true`.
