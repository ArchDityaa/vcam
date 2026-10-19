#!/usr/bin/env bash
#
# Long-run stability test for vcamd.
#
# Feeds a looping 1080p30 clip into the virtual camera and periodically checks
# that the daemon is still alive, still producing non-black frames, and not
# leaking memory.
#
# Usage:
#   DURATION=1800 tests/stability.sh [target/release]
#
# Environment:
#   DURATION  total run time in seconds (default 1800 = 30 minutes)
#   INTERVAL  sample interval in seconds (default 60)
#   WIDTH/HEIGHT/FPS  output format (default 1920/1080/30)
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bindir="${1:-$root/target/debug}"
vcamd="$bindir/vcamd"
vcam="$bindir/vcam"
duration="${DURATION:-1800}"
interval="${INTERVAL:-60}"
width="${WIDTH:-1920}"
height="${HEIGHT:-1080}"
fps="${FPS:-30}"

socket="$(mktemp -d)/vcam.sock"
workdir="$(mktemp -d)"
source_video="/tmp/vcam-stability-source.mp4"
log="$workdir/vcamd.log"
sample_log="$workdir/samples.log"
failures=0
daemon_pid=""

cleanup() {
    "$vcam" --socket "$socket" shutdown >/dev/null 2>&1 || true
    [[ -n "$daemon_pid" ]] && kill "$daemon_pid" >/dev/null 2>&1 || true
    sleep 0.5
    echo "daemon log: $log"
    echo "sample log: $sample_log"
}
trap cleanup EXIT

log_line() { printf '%s\n' "$*" | tee -a "$sample_log"; }

if [[ ! -f "$source_video" ]]; then
    if command -v ffmpeg >/dev/null 2>&1; then
        echo "==> generating 1080p30 source clip"
        ffmpeg -hide_banner -loglevel error -y -f lavfi \
            -i "testsrc2=size=${width}x${height}:rate=${fps}" -t 10 \
            -c:v libx264 -pix_fmt yuv420p -preset ultrafast "$source_video"
    else
        echo "==> ffmpeg missing; falling back to testdata/sample.mp4"
        source_video="$root/testdata/sample.mp4"
    fi
fi

mkdir -p "$(dirname "$socket")"
cat > "$workdir/config.toml" <<EOF
[daemon]
socket = "$socket"
log_level = "info"

[output]
width = $width
height = $height
fps = $fps
pixel_format = "NV12"
fit = "letterbox"
EOF

echo "==> starting vcamd ($width x $height @ $fps)"
"$vcamd" --config "$workdir/config.toml" >"$log" 2>&1 &
daemon_pid=$!
for _ in $(seq 1 50); do
    [[ -S "$socket" ]] && break
    sleep 0.1
done
if [[ ! -S "$socket" ]]; then
    echo "FAIL: daemon did not start"
    exit 1
fi

"$vcam" --socket "$socket" loop on >/dev/null
"$vcam" --socket "$socket" start --source "$source_video" >/dev/null
sleep 2

rss_kb() {
    awk '/VmRSS/ {print $2}' "/proc/$daemon_pid/status" 2>/dev/null || echo 0
}

first_rss=""
last_rss=""
start_time=$(date +%s)
samples=0

while :; do
    now=$(date +%s)
    elapsed=$((now - start_time))
    [[ "$elapsed" -ge "$duration" ]] && break

    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        log_line "t=${elapsed}s FAIL daemon exited"
        failures=$((failures + 1))
        break
    fi

    state=$("$vcam" --socket "$socket" status | grep -i '^State:' | awk '{print $2}')
    rss=$(rss_kb)
    [[ -z "$first_rss" ]] && first_rss=$rss
    last_rss=$rss

    frame="$workdir/frame.raw"
    if timeout 10 v4l2-ctl -d /dev/video10 --stream-mmap --stream-count=1 \
        --stream-to="$frame" >/dev/null 2>&1; then
        read -r mean distinct < <(python3 - "$frame" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
y = data[: len(data) // 3 * 2]
print(f"{sum(y)/len(y):.2f} {len(set(y))}")
PY
)
    else
        mean=0
        distinct=0
    fi

    status="ok"
    if [[ "$state" != "playing" ]]; then
        status="FAIL state=$state"
        failures=$((failures + 1))
    fi
    if [[ "${distinct:-0}" -lt 5 ]]; then
        status="FAIL flat frame (distinct=$distinct)"
        failures=$((failures + 1))
    fi

    samples=$((samples + 1))
    log_line "t=${elapsed}s rss=${rss}kB state=${state} mean=${mean} distinct=${distinct} ${status}"

    sleep "$interval"
done

echo
echo "==> summary after ${duration}s ($samples samples)"
echo "    first RSS: ${first_rss:-?} kB"
echo "    last RSS:  ${last_rss:-?} kB"
if [[ -n "$first_rss" && -n "$last_rss" && "$first_rss" -gt 0 ]]; then
    growth=$((last_rss - first_rss))
    echo "    growth:    ${growth} kB"
    if [[ "$growth" -gt 204800 ]]; then
        echo "    FAIL: RSS grew by more than 200 MB"
        failures=$((failures + 1))
    fi
fi

if [[ "$failures" -eq 0 ]]; then
    echo "==> STABILITY PASS"
    exit 0
fi
echo "==> STABILITY FAIL ($failures)"
exit 1
