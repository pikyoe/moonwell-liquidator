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
#   ./run.sh            # start + langsung ikuti log bot (Ctrl+C keluar dari
#                       #   log, bot tetap berjalan di latar)
#   ./run.sh start      # sama seperti di atas
#   ./run.sh start -d   # start tanpa ikuti log (detach)
#   ./run.sh stop       # hentikan proses (graceful, fallback kill -9)
#   ./run.sh restart    # stop lalu start (ikut log; gunakan `restart -d` utk detach)
#   ./run.sh status     # status + identitas + log terakhir
#   ./run.sh logs       # ikuti log bot (tail -f bot.log)

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
  local detach=0
  # Ambil opsi -d/--detach bila ada (bisa di posisi mana pun).
  for arg in "$@"; do
    case "$arg" in
      -d|--detach) detach=1 ;;
    esac
  done

  if [ ! -x "$BIN" ]; then
    echo "Binary tidak ditemukan: $BIN"
    echo "Jalankan dulu: cargo build --release"
    exit 1
  fi

  # Ambil flock atomik dulu: dua `./run.sh start` bersamaan tidak boleh dua-duanya
  # lolos (TOCTOU). Buka fd 9 dan kunci; lock hanya menjaga window launch —
  # dilepas (exec 9>&-) segera setelah bot ter-launch, sebelum menampilkan log.
  exec 9>"$LOCKFILE"
  if ! flock -n 9; then
    echo "Lockfile '$LOCKFILE' terkunci — start lain sedang berjalan."
    exit 1
  fi

  # Cek dulu apakah bot sudah berjalan; JANGAN pasang trap EXIT sebelum titik ini,
  # karena trap tersebut akan menghapus PID file bot yang masih hidup saat start
  # ditolak (exit 1) — membuat bot jadi orphan.
  if is_running; then
    echo "Bot sudah berjalan (PID $(cat "$PIDFILE")). Hentikan dulu: ./run.sh stop"
    exit 1
  fi

  # Arm trap hanya setelah dipastikan launcher ini yang OWN lahan PID — cleanup
  # hanya berlaku bila peluncuran gagal di tengah.
  trap 'rm -f "$PIDFILE"' EXIT

  rotate_logs

  # Luncurkan bot di SESSION tersendiri (setsid): tanpa job control di shell
  # skrip, bot berbagi process group dengan skrip, jadi Ctrl+C pada `tail -f`
  # (SIGINT ke foreground process group) akan ikut membunuh bot. setsid
  # memisahkan session sehingga sinyal terminal tidak menjangkau bot.
  # Fallback ke nohup bila setsid tidak tersedia.
  if command -v setsid >/dev/null 2>&1; then
    setsid "$BIN" >>"$LOG" 2>&1 9<&- &
  else
    nohup "$BIN" >>"$LOG" 2>&1 9<&- &
  fi
  echo $! >"$PIDFILE"
  trap - EXIT                   # sukses — lepas trap bersih-bersih
  exec 9>&-                     # lepas flock — lock hanya menjaga window launch

  echo "Bot dimulai. PID=$(cat "$PIDFILE")"
  if [ "$detach" -eq 1 ]; then
    echo "Mode detach — log: $LOG (ikuti dengan: ./run.sh logs)"
  else
    echo "Menampilkan log (Ctrl+C untuk keluar; bot tetap berjalan)."
    echo "Log: $LOG"
    tail -f "$LOG"
  fi
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
  start)    start "${@:2}" ;;
  stop)     stop ;;
  restart)  stop; start "${@:2}" ;;
  status)   status ;;
  logs)     [ -f "$LOG" ] || { echo "Belum ada log: $LOG"; exit 1; }
            tail -f "$LOG" ;;
  *)        echo "Argumen tidak dikenal: $1 (lihat atas file)"; exit 1 ;;
esac