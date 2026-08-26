# Moonwell Liquidator

Bot liquidasi backrun untuk Moonwell (Base), ditulis dalam Rust, dengan executor
Solidity yang memakai flashloan Morpho. Mendukung jalur OEV (Moonwell auction,
Route A) dan fallback klasik `liquidateBorrow` (Route B). Swap kolateral →
loan token via Aerodrome (opsional, aktif default).

**Semua dana berada di kontrak executor; bot hanya membangun dan mengirim
`LiquidationJob`. Private key di config hanya dipakai untuk menandatangani
transaksi.**

## Arsitektur

```
app/          Bot Rust
  src/
    main.rs       Entry point — wire semuanya, listen blok baru via WS
    config.rs     Config.toml + parsing alamat
    contracts.rs  ABI bindings (sol!) untuk Moonwell, Morpho, Aerodrome
    health.rs     Health factor off-chain (collateral×CF×price vs borrow×price)
    indexer.rs    Bootstrap borrower dari event Borrow + refresh snapshot
    strategy.rs   Bangun LiquidationJob, pilih target, siapkan calldata swap
    swap.rs       Pembangun calldata Aerodrome (route, slippage guard)
    submitter.rs  eth_call dulu; kirim hanya kalau lolos; fallback Route B
    state.rs      DashMap in-memory + snapshot JSON opsional (tanpa DB)

contracts/    Solidity executor (Foundry)
  src/OevLiquidator.sol   Flashloan → liquidate → redeem → swap → repay, atomik
  test/OevLiquidator.t.sol Fork tests (Base mainnet state)
```

## Cara kerja satu siklus

1. **Indexer** membaca event `Borrow` dari blok historis 500.000 blok lalu
   memantau blok baru; posisi tiap borrower di-refresh via
   `getAccountSnapshot`.
2. **Health engine** menghitung HF off-chain; kalau HF < 1 (liquidatable),
   strategi pilih market pinjaman terbesar dan kolateral terbesar.
3. **Strategi** membangun `LiquidationJob`: repay dibatasi ≤ close factor
   50% dan ≤ `max_position_usd` (default $25.000); swap disiapkan dengan
   estimasi sitaan OEV (`repay + profit × liquidator_fee_bps`,
   default 30%).
4. **Submitter** menjalankan `eth_call` — kalau revert / profit < `minProfit`,
   tidak ada gas keluar. Kalau lolos, transaksi dikirim.

Kontrak executor melakukan dalam satu transaksi:
Morpho flashloan → `updatePriceEarlyAndLiquidate` (OEV) atau `liquidateBorrow`
(klasik) → redeem mToken → swap via calldata generik (Aerodrome) →
kembalikan flashloan → sisa jadi profit di kontrak.

## Setup

### Prasyarat
- OS: Linux / macOS (WSL2 untuk Windows)
- Rust (rustup) + Foundry (forge)
- RPC Base dengan websocket + flashblocks (HTTP juga dipakai)
- Sedikit ETH di wallet signer untuk gas

### 0. Instal toolchain (sekali)

Jalankan perintah ini satu per satu di terminal:

```bash
# 1) Rust — compiler bot
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env          # atau buka shell baru agar `cargo` tersedia

# 2) Foundry — compiler & test kontrak Solidity
curl -L https://foundry.paradigm.xyz | bash
source ~/.bashrc             # muat ulang PATH agar `foundryup` tersedia
foundryup                    # install forge/cast/anvil

# 3) Git (biasanya sudah ada)
sudo apt-get install -y git  # Ubuntu/Debian; macOS: xcode-select --install
```

Cek hasilnya (harus menampilkan versi):

```bash
cargo --version   # contoh: cargo 1.8x
forge --version   # contoh: forge v1.7.x
git --version

# Clone repo (sesuaikan URL Anda) dan siapkan submodule lib kontrak
git clone <URL_REPO> && cd moonwell-liquidator
git submodule update --init --recursive
```

### 1. Deploy kontrak

```bash
cd contracts
BASE_RPC_URL="https://mainnet.base.org" forge test          # 9/9 harus lulus
forge create --rpc-url "$BASE_RPC_URL" \
  --private-key "$DEPLOYER_KEY" \
  src/OevLiquidator.sol:OevLiquidator
# catat alamat terdeploy
```

### 2. Konfigurasi bot

```bash
cd app
cp config.toml.example config.toml
```

Isi:
- `base_rpc_http`, `base_rpc_ws` — RPC Anda
- `private_key` — kunci signer bot (bukan pemilik kontrak; keduanya boleh sama)
- `executor_address` — alamat kontrak dari langkah 1
- `min_profit_wei` — ambang profit minimal (wei token hasil akhir); gunakan
  `min_profit_per_symbol` untuk override per market (desimal beda-beda)
- `[swap]` — router Aerodrome, `slippage_bps` (default 200 = 2%), 
  `liquidator_fee_bps` (default 3000 = 30% bagian liquidator OEV)

Direkomendasikan memindahkan `private_key` ke variabel lingkungan:
```bash
export BOT_PK="0x..."
# ganti di config: private_key = "${BOT_PK}" tidak otomatis — mudahnya pakai .env + dotenv
```

### 3. Jalankan

```bash
cd app
cargo build --release
RUST_LOG=info ./target/release/moonwell-liquidator
```

Log akan menunjukkan: market dimuat → bootstrap borrower → listen blok baru →
peluang → simulasi → tx terkirim / revert aman.

Snapshot state ditulis tiap 100 blok ke `snapshot.json` (restart lebih cepat);
hapus file itu untuk bootstrap ulang dari chain.

## Menyetar profit & fee

- **OEV split**: Moonwell OEV wrapper menyimpan sebagian insentif untuk
  MENGaO/backer; bot mengestimasi bagian liquidator dengan
  `liquidator_fee_bps` (30% default, disesuaikan dari fee aktual wrapper).
- **Min profit**: kontrak revert bila net < `minProfit`; submitter sudah
  menyaring lewat `eth_call`.
- **Max position**: `$25.000` default — batasi ukuran agar slippage swap &
  risiko terkendali.

## Pengujian

- `cd contracts && forge test` — fork test di state Base asli, termasuk
  validasi calldata swap Aerodrome (`testAerodromeSwapWorks`).
- `cd app && cargo build` — compile bot.

## Keamanan operasional

- **Jangan commit `config.toml` atau `.env`** — sudah masuk `.gitignore`.
- Private key di config hanya dipakai menandatangani; profit disimpan di
  kontrak (bisa di-sweep oleh owner kontrak).
- `contracts/broadcast/` dari `forge create` berisi alamat & kunci deployer —
  ikut di-ignore.
