#!/usr/bin/env bash
# Jalankan bot likuidasi Moonwell sebagai proses latar (nohup) yang persistent.
#
# Fitur:
#   - mencegah dua instance berjalan bersamaan: flock atomic (anti TOCTOU)
#     + cek PID file dengan verifikasi identitas proses (anti stale-PID reuse)
#   - menulis log ke bot.log, dengan rotasi sederhana (5 x hingga ~50MB)
#   - menyimpan PID ke moonwell.pid
#   - stop/restart/status via argumen
#
# Penggunaan:
#   ./run.sh            # start (atau tolak bila sudah jalan)
#   ./run.sh start      # sama seperti di atas
#   ./run.sh stop       # hentikan proses (graceful, fallback kill -9)
#   ./run.sh restart    # stop lalu start
#   ./run.sh status     # status + identitas + log terakhir

set -euo pipefail

cd "$(dirname "$0")"

BIN="${BIN:-./target/release/moonwell-liquidator}"
LOG="bot.log"
PIDFILE="moonwell.pid"
LOCKFILE="moonwell.lock"          # untuk flock atomik (tidak dihapus, cuma marker)
PROC_MATCH="*moonwell-liquidator*"
ROTATION_BYTES=52428800           # 50 MB

# true bila proses yang tercatat di PIDFILE masih hidup DAN benar-benar bot ini.
is_running() {
  [ -f "$PIDFILE" ] || return 1
  local pid
  pid="$(cat "$PIDFILE" 2>/dev/null || true)"
  [ -n "$pid" ] || return 1
  if kill -0 "$pid" 2>/dev/null; then
    local cmd
    cmd="$(ps -p "$pid" -o args= 2>/dev/null || true)"
    case "$cmd" in
      $PROC_MATCH) return 0 ;;
      *) return 1 ;;   # PID hidup tapi bukan bot (PID reuse → anggap mati)
    esac
  fi
  return 1
}

# Rotasi dasar: geser log lama bila bot.log sudah melewati ambang.
rotate_logs() {
  [ -f "$LOG" ] || return 0
  local size
  size="$(stat -c%s "$LOG" 2>/dev/null || echo 0)"
  [ "$size" -lt "$ROTATION_BYTES" ] && return 0
  echo "Rotasi log (${LOG} > batas)."
  rm -f "$LOG.3"
  [ -f "$LOG.2" ] && mv "$LOG.2" "$LOG.3"
  [ -f "$LOG.1" ] && mv "$LOG.1" "$LOG.2"
  mv "$LOG" "$LOG.1"
}

start() {
  if [ ! -x "$BIN" ]; then
    echo "Binary tidak ditemukan: $BIN"
    echo "Jalankan dulu: cargo build --release"
    exit 1
  fi

  # Ambil flock atomik dulu: dua `./run.sh start` bersamaan tidak boleh dua-duanya
  # lolos (TOCTOU). Buka fd 9 dan kunci segera; lock dilepas saat script berakhir.
  exec 9>"$LOCKFILE"
  if ! flock -n 9; then
    echo "Lockfile '$LOCKFILE' terkunci — start lain sedang berjalan."
    exit 1
  fi
  trap 'rm -f "$PIDFILE"' EXIT   # bersihkan PID bila launcher gagal di tengah

  if is_running; then
    echo "Bot sudah berjalan (PID $(cat "$PIDFILE")). Hentikan dulu: ./run.sh stop"
    exit 1
  fi

  rotate_logs
  nohup "$BIN" >>"$LOG" 2>&1 &
  echo $! >"$PIDFILE"
  echo "Bot dimulai. PID=$(cat "$PIDFILE")"
  echo "Log: $LOG (ikuti dengan: tail -f $LOG)"
  trap - EXIT                   # sukses — lepas trap bersih-bersih
}

stop() {
  if [ ! -f "$PIDFILE" ]; then
    echo "Tidak ada PID file — mungkin belum berjalan."
    return
  fi
  local pid
  pid="$(cat "$PIDFILE")"
  if is_running; then
    echo "Menghentikan bot (PID $pid)..."
    kill "$pid"                  # graceful SIGTERM
    local i
    for i in $(seq 1 20); do
      is_running || { echo "Berhenti."; rm -f "$PIDFILE"; return; }
      sleep 0.5
    done
    echo "Tidak berhenti setelah SIGTERM — memaksa kill -9."
    kill -9 "$pid" 2>/dev/null || true
    sleep 0.2
    rm -f "$PIDFILE"
    echo "Dipaksa berhenti."
  else
    echo "PID $pid tidak hidup / bukan proses bot; bersihkan PID file."
    rm -f "$PIDFILE"
  fi
}

status() {
  if is_running; then
    echo "Bot BERJALAN (PID $(cat "$PIDFILE"))."
  elif [ -f "$PIDFILE" ]; then
    echo "PID file ada ($(cat "$PIDFILE")) tapi bot tidak berjalan."
  else
    echo "Bot TIDAK berjalan (tanpa PID file)."
  fi
  if [ -f "$LOG" ]; then
    echo "--- log terakhir ---"
    tail -n 15 "$LOG"
  fi
}

case "${1:-start}" in
  start)    start ;;
  stop)     stop ;;
  restart)  stop; start ;;
  status)   status ;;
  *)        echo "Argumen tidak dikenal: $1 (lihat atas file)"; exit 1 ;;
esac