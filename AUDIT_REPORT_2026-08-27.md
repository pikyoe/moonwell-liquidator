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
   - Fix awal (commit `d2c2d62`): scan hanya **256 byte pertama** runtime code
     (daerah dispatcher; terverifikasi on-chain: offset 54 utk wrapper WETH
     Base). Aggregator raw Chainlink (9.5 KB) di-benchmark: tidak mengandung
     selector → tetap di-tolak. Panggilan `updatePriceEarlyAndLiquidate` tetap
     gerbang final.
   - Test: `testOevRejectsNonWrapperFeed` — getFeed dip-mock ke aggregator
     raw WETH/USD, `execute` harus revert "wrapper bukan OEV".
3. **Bug (gitar-bot#3) — bounded scan awal hanya cek offset kelipatan-32**
   (`OevLiquidator.sol`, commit `89b7297`). Versi 8-slot (0,32,...,224) salah:
   dispatcher Solidity menaruh selector sebagai PUSH4 pada offset **4-byte
   aligned**, bukan 32-byte aligned (wrapper produksi punya 0x16bb3b3a di
   offset 54 → lolos di antara slot). Diganti loop per-byte `i in 0..256`
   dengan buffer `extcodecopy(…, 288)` (kelipatan 32; zero-fill di luar kode)
   sehingga `mload(add(ptr, i))` selalu berada dalam buffer yang sah dan
   `and(…,0xFFFFFFFF)` tidak tertipu byte tetangga.
   - Test: `testOevDetectsNonAlignedSelector` — bytecode dummy berisi
     `6316bb3b3a` di offset 54/55 (didahului STOP); filter harus lolos dan
     eksekusi berlanjut ke call (STOP) → revert "zero seized". Bila scan
     kelipatan-32, test revert "wrapper bukan OEV" → gagal.

**Verifikasi sesi review (2026-08-27, final):**
- `forge test` (Base publik): **16/16 PASS** (14 lama + 2 test baru)
- `cargo test`: **15/15 PASS** (13 lama + 2 test baru)
- `cargo clippy --all-targets`: **0 warning**

**Status:** commit `89b7297` sudah di-push ke `fix/audit-remediation-2026-08-27`
(PR #14 auto-update); keempat thread review di-resolve, PR di-merge.
---

# Reaudit Lanjutan — 25 September 2026 (HEAD `463d99e`)

## Lingkup sesi ini

Re-verifikasi on-chain penuh (RPC publik Base `mainnet.base.org`), konfirmasi
state kode saat HEAD terhadap seluruh remediasi terdahulu, dan audit tambahan untuk
drift config/dokumentasi. Semua nilai di bawah dibaca langsung dari chain pada sesi ini
(bukan kutipan laporan lama).

## Verifikasi On-Chain Segar (2026-09-25)

| Item | Nilai terverifikasi | Konsisten? |
|------|---------------------|-------------|
| `comptroller.oracle()` | `0xEC942bE8A8114bFD0396A5052c36027f2cA6a9d0` | ✓ (sama dengan laporan lama) |
| `closeFactorMantissa()` | senilai 0.5e18 (500000000000000000) | ✓ |
| `liquidationIncentiveMantissa()` | senilai 1.1e18 (1100000000000000000) | ✓ |
| `markets(mWETH)` | listed=1, `collateralFactorMantissa` ≈ 0.84 | ✓ |
| `markets(mUSDC)` | listed=1, `collateralFactorMantissa` ≈ 0.88 | ✓ (cek ulang segar) |
| `getFeed("WETH")` | `0x57DA741aD933869cC9EBfb9668288053A0738f3c` — `liquidatorFeeBps()` 3000 | ✓ |
| `getFeed("USDC")` | `0xb7967d184907737a955f39a8067bc1a0cc313292` — fee 3000 | ✓ (wrapper USDC ada; sudah masuk config sejak `d385313`) |
| `getFeed("cbBTC")` | `0xfea70a74d94a6b6f9764db9c4867bbe4678a5da6` — fee 3000 | ✓ |
| `getFeed("USDS")` | `0xf655eedede0cd9f7a3de5e8018909c429dcfbf9b` — fee 3000 | ✓ |
| `getFeed("DAI")` | `0x0eab3b9ae08b43077ad1aeb9820462faf99bcec8` — fee 3000 | ✓ |
| `getFeed("AERO")` | `0x3623c921bfd9d7e9d88e5cbb436b68be2c2bc0b7` — fee 3000 | ✓ |
| `getFeed("EURC")` | `0x945ab3891d7963214833b4d51f54f068c8e6b55e` — fee 3000 | ✓ (alamat TIDAK ada di whitelist config — lihat temuan #9) |

## Resolusi Disrepansi Fee — docs `4000` vs chain `3000`

Disrepansi yang tercatat di task tracking sudah **terjawab**:

1. **Fakta on-chain (sesi ini):** `liquidatorFeeBps()` senilai **3000 (30%)** untuk **SEMUA** wrapper
   yang terdaftar di oracle Moonwell (WETH, USDC, cbBTC, USDS, DAI, AERO, EURC).
   Tidak ada satu pun yang mengembalikan 4000.
2. **Sumber nilai docs `4000 (40%)`:** nilai tersebut adalah **kadaluwarsa** dari dokumentasi
   Moonwell/Chainlink OEV(kemungkinan merujuk parameter pra-deploy atau template proyek
   lain dengan fee 40%. Tidak ada dasar on-chain di Base saat ini).
3. **Konsistensi kode:** `config.toml.example` memakai `liquidator_fee_bps` 3000 (default
   `config.rs::default_liquidator_fee_bps` juga 3000); `strategy.rs` memakai fee wrapper
   yang dibaca on-chain (`oev_fee_bps`) dengan fallback config 3000; `OevLiquidator.sol`
   sendiri TIDAK menghitung fee(fee ditentukan wrapper — `_calculateCollateralSplit` milik
   ChainlinkOEVWrapper); test fork menegakkan 3000 (`LIQUIDATOR_FEE_BPS` senilai 3000).
   Semua lapisan konsisten di **30%** — tidak ada bug; hanya dokumentasi Moonwell yang basi.

**Rekomendasi:** Tidak ada perubahan kode yang diperlukan untuk fee. Bila perlu, laporkan
ke Moonwell agar docs `4000` dikoreksi menjadi `3000` (atau verifikasi apakah parameter dapat
diubah governance di masa depan — bot sudah membaca fee on-chain per-market sehingga otomatis
mengikuti bila berubah).

## Temuan Baru

### 9. 🟡 RENDAH — Wrapper EURC tidak terdaftar di whitelist `oev_wrappers` config

**Lokasi:** `app/config.toml.example` baris 31–36.

**Fakta:**
- Daftar whitelist config memuat 6 wrapper (WETH, USDC, cbBTC, USDS, DAI, AERO) —
  **EURC tidak ada**, padahal oracle `getFeed("EURC")` mengembalikan wrapper OEV valid
  `0x945ab3891d7963214833b4d51f54f068c8e6b55e` (fee 3000, selector ada).
- `main.rs` **self-heal**: `build_market_map` memanggil `oracle.getFeed(m.symbol)` untuk SEMUA
  market yang dikonfigurasi (termasuk EURC) dan menambahkan `oev_wrappers_feed` ke daftar
  pantauan. Jadi trigger `PriceUpdatedEarlyAndLiquidated` untuk wrapper EURC **tetap aktif**
  walau tidak tercantum di whitelist — tidak ada dampak fungsional.
- `market_and_trigger` memakai gabungan whitelist + feed dinamis, jadi cakupan
  pantauan benar-benar lengkap.

**Dampak:** Tidak ada dampak runtime(self-healed oleh resolve dinamis). Hanya
dokumentasi/whitelist yang tidak konsisten — operator yang membaca config bisa salah
mengira EURC tidak punya wrapper OEV.

**Rekomendasi:** Tambahkan baris
```toml
"0x945ab3891d7963214833b4d51f54f068c8e6b55e",    # EURC
```
ke daftar `oev_wrappers` di `config.toml.example` (dan config produksi) agar whitelist
mencerminkan semua wrapper yang aktif on-chain. Perbaikan kosmetik/tidak mendesak.

## Konfirmasi State Remediasi pada HEAD `463d99e`

Semua perbaikan dari sesi 2026-08-27 hingga  [2026-08-29 dan commit lanjutan sampai HEAD
dikonfirmasi **masuk di kode saat ini**:

| # | Remediasi | Verifikasi di HEAD |
|---|------------|--------------------|
| 1 | Trigger OEV → event `PriceUpdatedEarlyAndLiquidated` (bukan `UpdatedPrices` fiktif) | `indexer.rs:32,270–291` + `hypersync.rs market_and_trigger` memakai signature itu ✓ |
| 2 | `classic_fallback` terpicu saat `SendOutcome::Reverted` (sim OEV revert, bukan cuma transport error) | `submitter.rs SendOutcome` + `main.rs:191–196` ✓ |
| 3 | Mode dipilih dari `coll.oev_fee_bps` — market non-OEV jatuh ke `Classic` langsung | `strategy.rs:292–297` ✓ |
| 4 | Fork test E2E: unlock borrowCaps + seed USDC cash | `_unlockBorrowCaps` + `_seedUsdcCash` tetap ada ✓ |
| 5 | `FakeOevWrapper` membagi split ala produksi (30% ke liquidator, sisanya ke FEE_RECIPIENT) | `LIQUIDATOR_FEE_BPS` senilai 3000, split `_calculateCollateralSplit` ✓ |
| 6 | Nonce race submitter — send stateful berurutan; `InFlightGuard` dedup (borrower, mtoken) | `submitter.rs InFlightGuard` + `for_each_concurrent` ✓ |
| 7 | `min_profit_wei` default `"0"` + fail-fast parse; config example kini `"300000"` | `config.rs default_zero_str` & fail-fast ✓ |
| 8 | `deadline` swap 600s tidak di-enforce (diterima oleh desain) | komentar didokumentasi ✓ |
| — | `nonReentrant` hanya di callback(bukan execute)) | `OevLiquidator.sol:96–101` ✓ |
| — | bootstrap fail-fast 3x → `bail!` | `main.rs:203–216` ✓ |
| — | binding `markets()` 2-field | `contracts.rs` ✓ |
| — | fee OEV dibaca on-chain + fallback config | `main.rs build_market_map` ✓ |

Tidak ada temuan kritis/tinggi baru di sesi re-audit ini. Temuan #9 (EURC whitelist)
satu-satunya tambahan; tingkat rendah dan self-healed.

## Status Akhir

- **Fungsi inti:** likuidasi jalur OEV & Classic, guard keamanan (`expectedCallHash`,
  `onlyOwner`, reset allowance, wrap-back ETH), fallback, mode non-OEV, dan trigger
  OEV — seluruhnya terkonfirmasi di HEAD `463d99e` tanpa regresi yang terlihat dari kode static.

- **Disrepansi fee sudah tuntas:** 3000 (30%) di chain konsisten dengan config dan kode test;
  docs 4000 basi (faktor eksternal, bukan bug kode).
- **Temuan #9** bersifat administratif (whitelist tidak lengkap; self-healed oleh resolve dinamis).
- **Pengujian:** sesi ini tidak mengeksekusi `forge`/`cargo` (lingkup konstrain RPC+Python). Angka
  pass terakhir yang tercatat: `forge 16/16`, `cargo 15/15`, clippy 0 warning (sesi
  2026-08-29/PR #20 — pada state sebelum beberapa commit konfigurasi lanjutan yang tidak
  menyentuh logika inti). Disarankan tetap menjalankan `forge test` + `cargo test` + `clippy`
  sebelum rilis produksi berikutnya, sesuai konvensi repo.---

# Deep-Dive Adversarial — Temuan Tambahan (2026-09-25, sesi lanjutan)

Sesi ini sengaja mencari bug/potensi bug yang tidak terlihat di audit permukaan.
Berikut hasilnya — 2 temuan nyata baru (#10, #11) + 3 catatan risiko operasional
(semuanya sudah diverifikasi di kode HEAD `463d99e`).

## 10. 🟠 SEDANG — Sinyal peka-waktu likuidasi kompetitor di fast-signal mati untuk akun tak dikenal

**Lokasi:** `app/src/main.rs` baris 270–306 (loop penerima `FastSignal`)

**Fakta:**
- Komentar (baris 271–274) menyatakan: *"KECUALI sinyal itu likuidasi kompetitor — kita
  BUKAN menunggu akun dikenal (lihat jalur khusus di bawah) untuk bereaksi kejadian yang
  sudah terjadi"* — tapi **tidak ada** jalur khusus semacam itu di kode.
- Satu-satunya percabangan borrower (baris 302–307) adalah:
  ```rust
  if let Some(b) = sig.borrower() {
      if state_fast.borrowers.get(&b).is_none() {
          tracing::debug!(..., "sinyal akun tak dikenal — di-skip");
          continue;
      }
  }
  ```
  yaitu **skip tanpa pandang bulu** untuk semua borrower yang belum ada di state — tanpa
  terkecualikan untuk `TxIntent::Liquidation`/`OevLiquidation` (likuidasi kompetitor).
- Jadi: saat kompetitor melikuidasi akun **baru/under-tracked** (borrower yang lewat
  dari snapshot/bootstrap, atau yang baru meminjam),sinyal `newPendingTransactions`/
  `pendingLogs` preconfirmation yang membawa `borrower` itu akan **dibuang** — scan cepat
  OEV-reflex untuk peluang yang sudah "terjadi" di tx kompetitor tidak pernah ditrigger.


**Dampak:**
- Keunggulan utama arsitektur (reaksi <1 blok terhadap likuidasi kompetitor / perubahan
  harga) hilang untuk akun yang belum masuk state `borrowers`; bot hanya menunggu
  blok canonical berikutnya (biasanya masih cukup cepat untuk scan reguler, tapi
  kecepatan preconfirmation ~200 ms tidak termanfaatkan).
- Kemungkinan nyata: akun baru yang meminjam lalu langsung underwater dalam blok
  yang sama (atau borrow kecil di market yang tidak ter-pantau oleh event karena
  dedup/rate-limit) tidak akan memicu fast-scan.



**Rekomendasi:**
```rust
// Izinkan sinyal likuidasi kompetitor walau akun belum dikenal:
if let Some(b) = sig.borrower() {
    let known = state_fast.borrowers.get(&b).is_some();
    let is_liquidation = matches!(
        &sig,
        FastSignal::Tx { intent: Some(TxIntent::Liquidation { .. }), .. }
            | FastSignal::Tx { intent: Some(TxIntent::OevLiquidation { .. }), .. }
    );
    if !known && !is_liquidation {
        tracing::debug!(sig = ?sig, "sinyal akun tak dikenal — di-skip");
        continue;
    }
}
```
Atau, bila ingin tetap hemat RPC: untuk akun tak dikenal, lakukan refresh singkat
`getAccountSnapshot` pada (mloan, mcoll) yang dibawa sinyal likuidasi tersebut (biaya
2 snapshot) lalu scan penuh hanya bila hasilnya liquidatable.



##  [11. 🟠 SEDANG — Duplikasi pemrosesan rentang saat gap reconnection

**Lokasi:** `app/src/main.rs` baris 385–409 (resync gap + `watch_block` blok kini)

**Fakta:**
```rust
if let Some(prev) = last_processed {
    if number > prev + 1 {
        let from = prev + 1;
        indexer.watch_block(from, number, ...).await?; // <— inklusif `number`
    }
}
...
let oev_trigger = indexer.watch_block(number, number, ...).await?; // <— blok sama diproses lagi}
```
Rentang resync `[from, number]` **inklusif** terhadap `number` yang baru diterima —
padahal blok `number` tsb **langsung** diproses lagi oleh `watch_block(number, number)` di
bawahnya. Konsekuensi: blok pertama setelah gap diproses **dua kali**.

**Dampak:**
- Tidak merusak state (upsert idempoten), tidak menggandakan tx (refresh/re-scan
  idempoten, gate membatasi scan). Ini murni RPC & latensi boros — satu blok ekstra
  refresh akun + pencarian log HyperSync dobel per kejadian reconnection buruk.

  Skala: setiap WS putus/terlambat >1 blok, blok pertama sebelum catch-up di-refresh
  2x (dalam 1–2 detik yang sama), tanpa manfaat.



**Rekomendasi:** Buat rentang resync **eksklusif** terhadap blok kini:
```rust
if let Some(prev) = last_processed {
    if number > prev + 1 {
        let from = prev + 1;
        let to_excl = number.saturating_sub(1); // jangan dobel proses blok kini
        if to_excl >= from {
            indexer.watch_block(from, to_excl, ...).await?;
        }
    }
}
```



## Catatan Risiko Operasional (bukan bug)

### A. Paralisa sementara saat `closeFactorMantissa`/`liquidationIncentiveMantissa` gagal dibaca

`refresh_prices()` memakai `?` pada dua panggilan comptroller tersebut. Bila RPC
mengalami gangguan sesaat ( `Err` menyebar → `refresh_prices` mengembalikan error → seluruh
refresh (termasuk harga oracle yang sudah berhasil diambil) dibatalkan → `close_factor`
tetap 0 → `evaluate()` melewati SEMUA borrower (`if snap.close_factor.is_zero() => Ok(None)`)
sampai refresh berikutnya sukses. RPC lambat/429 selama beberapa detik bisa membuat bot
"buta" sementara.

**Mitigasi yang ada:** refresh diulang tiap blok (try-acquire) — pemulihan otomatis
pada blok berikutnya bila RPC pulih. Tidak ada dana berisiko (fail-closed).
Sifat: diterima oleh desain — dicatat untuk kewaspadaan operator.

### B. `refresh_prices` di-fast-signal & OEV-trigger dijalankan inline (menunggu I/O RPC)

Saat sinyal preconfirmation/mempool/trigger OEV masuk, `refresh_prices` (N+3 panggilan
serial RPC) dieksekusi **inline** di loop sinyal sebelum scan di-spawn. Bila RPC
  lambat (1–2 dtk/blok Base), latency tambahan ini bisa menjadikan kecepatan preconf
  ~200 ms tidak lagi berarti (spin-up scan tertunda sampai refresh selesai).

**Mitigasi:** refresh per-blok sudah berjalan di task background; nilai yang di-refresh inline
hanya untuk memastikan segar tepat sebelum scan OEV. Degradasi hanya pada kasus RPC
lambat; bukan bug fungsional.

###C. Job `evaluate()` di-drop diam-diam bila snapshot batchnya tidak lengkap

`if res.len() != expected_len => warn! ... skip` dan `accrue+snapshot kandidat gagal`
membuang job tanpa retry. Dalam RPC yang kehilangan satu subcall batch (langka dengan
Multicall3, tapi mungkin) peluang dilewati ke blok berikutnya. Tidak ada dana berisiko
(fail-closed; pemindaian ulang tiap blok menggantinya otomatis).

## Ringkasan Sesi Deep-Dive

| # | Tingkat | Temuan | Status |
|---|---------|--------|--------|
| 9 | 🟡 Rendah | Wrapper EURC tidak di whitelist config | **diperbaiki** (config.toml.example baris 37) |
| 10 | 🟠 Sedang | Sinyal likuidasi kompetitor untuk akun tak dikenal di-skip (komentar menjanjikan jalur khusus yang tidak ada) | **diperbaiki** (`app/src/main.rs`: sinyal likuidasi/OEV kini memicu refresh akun tak dikenal + scan; non-likuidasi tetap di-skip) |
| 11 | 🟠 Sedang | Blok pertama setelah gap reconnection diproses 2x (boros RPC, tidak merusak) | **diperbaiki** (`app/src/main.rs`: rentang resync kini EKSLUSIF — `number.saturating_sub(1)`) |
| — | ops | `refresh_prices` menulis 0 saat RPC comptroller gagal sesaat | **diperkuat** (fallback ke nilai `comptroller_params()` terakhir — area buta sementara dihindari) |
| — | ops | Paralisa sementara / inline RPC / drop-job diam-diam | diterima oleh desain; dicatat untuk operator |

Verifikasi kode: semua lokasi di atas dibaca langsung dari HEAD `463d99e`. Sesi
ini menerapkan perbaikan temuan #9 (`app/config.toml.example`), #10 & #11 serta
pengerasan ops `refresh_prices` (`app/src/main.rs`; `app/src/strategy.rs` —
getter `comptroller_params()`). Disarankan menjalankan `forge test` + `cargo test` + `clippy`
sesuai konvensi repo sebelum merge ke produksi.