#!/usr/bin/env bash
# Jalankan bot likuidasi Moonwell sebagai proses latar (nohup) yang persistent.
#
# Fitur:
#   - mencegah dua instance berjalan bersamaan (lockfile)
#   - menulis log ke bot.log
#   - menyimpan PID ke moonwell.pid
#   - stop/restart via argumen opsional
#
# Penggunaan:
#   ./run.sh            # start (atau restart kalau sudah jalan)
#   ./run.sh start      # sama seperti di atas
#   ./run.sh stop       # hentikan proses (graceful)
#   ./run.sh restart    # stop lalu start
#   ./run.sh status     # tampilkan status & PID

set -euo pipefail

cd "$(dirname "$0")"

BIN="${BIN:-./target/release/moonwell-liquidator}"
LOG="bot.log"
PIDFILE="moonwell.pid"

start() {
  if [ ! -x "$BIN" ]; then
    echo "Binary tidak ditemukan: $BIN"
    echo "Jalankan dulu: cargo build --release"
    exit 1
  fi

  # Cegah double-instance: kalau PID lama masih hidup, tolak start.
  if [ -f "$PIDFILE" ]; then
    old_pid="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
      echo "Bot sudah berjalan (PID $old_pid). Hentikan dulu: ./run.sh stop"
      exit 1
    fi
    rm -f "$PIDFILE"
  fi

  nohup "$BIN" >>"$LOG" 2>&1 &
  echo $! >"$PIDFILE"
  echo "Bot dimulai. PID=$(cat "$PIDFILE")"
  echo "Log: $LOG (ikuti dengan: tail -f $LOG)"
}

stop() {
  if [ ! -f "$PIDFILE" ]; then
    echo "Tidak ada PID file — mungkin belum berjalan."
    return
  fi
  pid="$(cat "$PIDFILE")"
  if kill -0 "$pid" 2>/dev/null; then
    echo "Menghentikan bot (PID $pid)..."
    kill "$pid"                      # graceful SIGTERM
    local i
    for i in $(seq 1 20); do
      kill -0 "$pid" 2>/dev/null || { echo "Berhenti."; rm -f "$PIDFILE"; return; }
      sleep 0.5
    done
    echo "Tidak berhenti setelah SIGTERM — memaksa kill -9."
    kill -9 "$pid" 2>/dev/null || true
    sleep 0.2
    rm -f "$PIDFILE"
    echo "Dipaksa berhenti."
  else
    echo "PID $pid tidak hidup lagi; bersihkan PID file."
    rm -f "$PIDFILE"
  fi
}

status() {
  if [ -f "$PIDFILE" ]; then
    pid="$(cat "$PIDFILE")"
    if kill -0 "$pid" 2>/dev/null; then
      echo "Bot BERJALAN (PID $pid)."
    else
      echo "PID file ada ($pid) tapi proses tidak hidup."
    fi
  else
    echo "Bot TIDAK berjalan (tanpa file PID)."
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