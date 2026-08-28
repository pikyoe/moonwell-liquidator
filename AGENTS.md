# Moonwell Liquidator — Repo Knowledge

## Build & Test
- Kontrak: `cd contracts && forge build && forge test` (fork test ke Base mainnet via `BASE_RPC_URL`, default publik). Semua 16 test harus lulus.
- Bot: `cd app && cargo build` (debug) / `cargo build --release`. Jalankan `cargo clippy` sebelum commit.
- Test fork terakhir diverifikasi: 16/16 PASS (forge, solc 0.8.24), termasuk 2 e2e happy path (borrower dibuat underwater via mockCall oracle; OEV path diuji dengan FakeOevWrapper realistis yang di-etch ke alamat wrapper asli — split 30% ke feeRecipient seperti produksi) + test penolakan aggregator non-wrapper + test deteksi selector offset non-32.
- Fork test meng-unlock `borrowCaps` (guardian `0x08eD…CF05`) dan me-seed cash mUSDC (10M) karena `getCash(mUSDC)=0` di mainnet — jangan hapus helper itu bila tests E2E mulai revert "borrow gagal".
- RPC berbayar Chainstack tidak menyediakan archive/data-trace: jalankan fork test dengan `BASE_RPC_URL=https://mainnet.base.org` (publik) untuk blok lama; bisa pin blok via `BASE_FORK_BLOCK`.

## Arsitektur
- `contracts/src/OevLiquidator.sol` — executor. Flashloan Morpho Blue (0xBBBB...FFCb di Base). Guard callback: `expectedCallHash` hanya diset oleh `execute()` (onlyOwner); callback reject hash tak dikenal.
- Jalur A (OEV): ChainlinkOEVWrapper lewat `comptroller.oracle().getFeed(symbol)`. Jalur B (Classic): `liquidateBorrow` langsung ke mToken.
- Bot Rust: indexer (event Borrow/Mint/Redeem/Transfer/LiquidateBorrow) → health (HF off-chain) → strategy (bangun LiquidationJob + calldata swap Aerodrome) → submitter (eth_call dulu, baru send). State DashMap in-memory + snapshot.json tiap 100 blok.
- Konkurensi (Fase 1-2, 2026-08-26): `Strategy::scan(P)` meng-clone semua input (markets/cfg/params) DI BAWAH lock singkat lalu mengembalikan `Arc<ScanJob>` — evaluasi borrower dijalankan paralel (JoinSet + semaphore 8) TANPA memegang mutex strategy. Di `main.rs`, loop blok me-trigger scan via `tokio::spawn` (non-blocking, gate semaphore `scan_gate`=1 agar scan tidak menumpuk per blok); hasil scan mengalir ke worker submitter global (channel unbounded + `for_each_concurrent(4)`) yang dibuat SEKALI di luar loop reconnect. Indexer `watch_block` melakukan refresh akun dengan dedup (market, akun) + konkurensi terbatas (8). Net effect: loop blok tidak pernah menunggu RPC scan/simulate.

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

## Review PR #14 (2026-08-27, komentar reviewer difix & diverifikasi)
- Fallback OEV->Classic TIDAK boleh hanya membalik `job.mode`: swapData/minLoanOut/minProfit dibangun ulang utk mode Classic via `Strategy::rebuild_classic_job` (Classic men-redeem seluruh sitaan = 100% profit, bukan split OEV 30%). Logika swap di free fn `build_swap_parts` (dipakai evaluator & rebuild).
- Selector OEV `0x16bb3b3a` discan cukup di 256 byte pertama runtime code wrapper (daerah dispatcher; verified offset 54 utk wrapper WETH Base) — bukan seluruh bytecode. Scan per-byte (dispatcher Solidity memakai PUSH4 offset 4-byte-aligned, bukan 32-byte-aligned — gitar-bot#3). Buffer copy 288B agar mload aman.
- Verifikasi akhir: forge 16/16 PASS, cargo 15/15 PASS, clippy 0 warning.

## Audit 2026-08-28 (sesi audit lanjutan — fix tanpa eksekusi test)
- 🔴 Reconnect gap: `last_processed` sekarang DI LUAR loop reconnect — rentang yang terlewat saat WS putus di-replay di koneksi berikutnya (sebelumnya nol→borrower basi & trigger OEV di gap hilang).
- 🟠 Trigger OEV otomatis: daftar `oev_wrappers` kini = whitelist config + hasil resolve dinamis `MarketInfo.oev_wrappers_feed` (feed yang `liquidatorFeeBps()` terbaca = wrapper valid). Replay gap juga memakai wrapper (bukan `&[]` yang menonaktifkan trigger).
- 🟠 Refresh akun kini retry 2x + backoff kecil + `warn!` saat gagal — tidak lagi diam ( state basi & prune akun aktif senyap).
- 🟠 `ENVIO_API_TOKEN` kini dibaca dari env (default serde) — komentar lama sudah benar.

- 🟡 Buffer amount_in Classic 95% (`amount_in_buffer_bps`: OEV 10000, Classic 9500) — jalur B tidak meng-update harga; sitaan aktual bisa < estimasi dari harga cache → buffer mencegah swap revert.

- Catatan: perubahan ini TIDAK dieksekusi test di sesi ini(lingkup static); jalankan `forge test` + `cargo test` + `clippy` sebelum merge (seperti biasa).
