#!/usr/bin/env bash
#
# End-to-end test for vcam against a real v4l2loopback device.
#
# It starts a private daemon on a private socket, drives it through the CLI and
# checks the frames that come back out of /dev/video10.
#
# Requirements:
#   - v4l2loopback loaded (vcam doctor must pass)
#   - a built daemon and CLI (cargo build, or --release)
#   - python3 (used to inspect raw frames)
#
# Usage:
#   tests/integration.sh [path/to/target/debug]
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bindir="${1:-$root/target/debug}"
vcamd="$bindir/vcamd"
vcam="$bindir/vcam"
sample="$root/testdata/sample.mp4"

if [[ ! -x "$vcamd" || ! -x "$vcam" ]]; then
    echo "error: build first (cargo build), or pass the target directory" >&2
    exit 2
fi
if [[ ! -f "$sample" ]]; then
    echo "error: missing test video $sample" >&2
    exit 2
fi

socket="$(mktemp -d)/vcam.sock"
workdir="$(mktemp -d)"
log="$workdir/vcamd.log"
device="/dev/video10"
capture="$workdir/frame.raw"
failures=0

cleanup() {
    "$vcam" --socket "$socket" shutdown >/dev/null 2>&1 || true
    pkill -f "vcamd --config $workdir/config.toml" >/dev/null 2>&1 || true
    sleep 0.3
    rm -rf "$workdir"
}
trap cleanup EXIT

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; failures=$((failures + 1)); }

check() {
    local description="$1"; shift
    if "$@" >/dev/null 2>&1; then
        pass "$description"
    else
        fail "$description"
    fi
}

# mean luma and number of distinct luma values of the first frame.
# frame_stats FILE [nv12|yuyv]
frame_stats() {
    python3 - "$1" "${2:-nv12}" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
if sys.argv[2] == "yuyv":
    y = data[0::2]
else:
    y = data[: len(data) // 3 * 2]  # Y plane of a 4:2:0 frame
print(f"{sum(y)/len(y):.2f} {len(set(y))}")
PY
}

capture_frame() {
    timeout 10 v4l2-ctl -d "$device" --stream-mmap --stream-count=1 \
        --stream-to="$1" >/dev/null 2>&1
}

status_field() {
    "$vcam" --socket "$socket" status | grep -i "^$1" | head -1
}

# ---------------------------------------------------------------------------

say "start a private daemon"
mkdir -p "$(dirname "$socket")"
cat > "$workdir/config.toml" <<EOF
[daemon]
socket = "$socket"

[[profiles]]
name = "ctl-fill"
match = ["v4l2-ctl"]
fit = "fill"

[[profiles]]
name = "small"
match = ["ffmpeg"]
width = 320
height = 240
EOF
"$vcamd" --config "$workdir/config.toml" --log-level info >"$log" 2>&1 &
for _ in $(seq 1 40); do
    [[ -S "$socket" ]] && break
    sleep 0.1
done
check "daemon is listening on $socket" test -S "$socket"
check "vcam status responds" "$vcam" --socket "$socket" status

say "doctor"
"$vcam" --socket "$socket" doctor | sed 's/^/  /'
if "$vcam" --socket "$socket" doctor >/dev/null 2>&1; then
    pass "doctor passed"
else
    fail "doctor failed (see output above)"
fi

say "idle placeholder"
sleep 0.5
if capture_frame "$capture"; then
    read -r mean distinct < <(frame_stats "$capture")
    if [[ "$mean" == "16.00" && "$distinct" == "1" ]]; then
        pass "idle frame is black (mean=$mean)"
    else
        fail "idle frame is not black (mean=$mean distinct=$distinct)"
    fi
else
    fail "could not capture the idle frame"
fi

say "play, pause, resume"
"$vcam" --socket "$socket" start --source "$sample" >/dev/null
sleep 1.5
check "state is playing" grep -qi '^State: *playing' <("$vcam" --socket "$socket" status)
if capture_frame "$capture"; then
    read -r mean distinct < <(frame_stats "$capture")
    if [[ "$distinct" -gt 10 ]]; then
        pass "playing frame has content (mean=$mean distinct=$distinct)"
    else
        fail "playing frame looks flat (mean=$mean distinct=$distinct)"
    fi
fi

"$vcam" --socket "$socket" pause >/dev/null
sleep 0.3
capture_frame "$workdir/pause_a.raw"
sleep 1.0
capture_frame "$workdir/pause_b.raw"
check "pause holds the last frame" cmp -s "$workdir/pause_a.raw" "$workdir/pause_b.raw"
"$vcam" --socket "$socket" resume >/dev/null
sleep 0.5
check "state is playing after resume" grep -qi '^State: *playing' <("$vcam" --socket "$socket" status)

say "seek"
"$vcam" --socket "$socket" seek 00:04 >/dev/null
sleep 0.5
if grep -q 'Position: 00:04' <("$vcam" --socket "$socket" status); then
    pass "seek moved to 00:04"
else
    fail "seek did not move to 00:04: $(status_field Position)"
fi

say "loop toggle"
"$vcam" --socket "$socket" loop off >/dev/null
if grep -q '(loop)' <("$vcam" --socket "$socket" status); then
    fail "loop off is still reported as looping"
else
    pass "loop off is reported"
fi
"$vcam" --socket "$socket" loop on >/dev/null
check "loop on is reported" grep -q '(loop)' <("$vcam" --socket "$socket" status)

say "hot swap"
if command -v ffmpeg >/dev/null 2>&1; then
    ffmpeg -hide_banner -loglevel error -y -f lavfi \
        -i color=c=red:s=1280x720:r=30 -t 2 -c:v libx264 -pix_fmt yuv420p \
        -preset ultrafast "$workdir/red.mp4"
    "$vcam" --socket "$socket" source "$workdir/red.mp4" >/dev/null
    sleep 1.0
    capture_frame "$capture"
    read -r mean distinct < <(frame_stats "$capture")
    if [[ "${mean%.*}" -ge 70 && "${mean%.*}" -le 95 ]]; then
        pass "hot swap shows the red source (mean=$mean)"
    else
        fail "hot swap did not switch sources (mean=$mean distinct=$distinct)"
    fi
else
    echo "  skip hot swap (ffmpeg not installed)"
fi

say "output format change"
"$vcam" --socket "$socket" output --pixel-format YUYV >/dev/null
sleep 0.8
check "device advertises YUYV" grep -q 'YUYV' <(v4l2-ctl -d "$device" --list-formats-ext)
"$vcam" --socket "$socket" output --pixel-format NV12 >/dev/null
sleep 0.8
check "device advertises NV12" grep -q 'NV12' <(v4l2-ctl -d "$device" --list-formats-ext)
"$vcam" --socket "$socket" output --width 640 --height 480 --fps 15 >/dev/null
sleep 0.8
check "device advertises 640x480" grep -q '640x480' <(v4l2-ctl -d "$device" --list-formats-ext)
"$vcam" --socket "$socket" output --width 1280 --height 720 --fps 30 >/dev/null
sleep 0.8

say "camera users"
if "$vcam" --socket "$socket" consumers | grep -q 'No applications'; then
    pass "no consumers while idle"
else
    fail "unexpected consumers while idle"
fi

timeout 8 v4l2-ctl -d "$device" --stream-mmap --stream-count=200 >/dev/null 2>&1 &
reader_pid=$!
sleep 2
if "$vcam" --socket "$socket" consumers | grep -q 'v4l2-ctl'; then
    pass "consumer is listed"
else
    fail "consumer was not listed"
fi

say "deferred output change"
"$vcam" --socket "$socket" output --width 640 --height 480 >/dev/null
sleep 1
if grep -qi '^Pending:' <("$vcam" --socket "$socket" status); then
    pass "change is deferred while a consumer is active"
else
    fail "change was not deferred"
fi
if grep -q '640x480' <("$vcam" --socket "$socket" status | grep '^Output:'); then
    fail "output changed even though it was deferred"
else
    pass "reported output is unchanged while deferred"
fi
wait "$reader_pid"
sleep 3
if grep -q '640x480' <(v4l2-ctl -d "$device" --list-formats-ext); then
    pass "deferred change applied after the consumer left"
else
    fail "deferred change was not applied"
fi
if grep -qi '^Pending:' <("$vcam" --socket "$socket" status); then
    fail "pending change was not cleared"
else
    pass "pending change cleared"
fi

say "profiles"
check "profiles are listed" grep -q 'small' <("$vcam" --socket "$socket" profile list)

"$vcam" --socket "$socket" profile use small >/dev/null
sleep 1
check "manual profile sets 320x240" grep -q '320x240' <(v4l2-ctl -d "$device" --list-formats-ext)

"$vcam" --socket "$socket" profile auto on >/dev/null
sleep 2
if grep -q '640x480' <(v4l2-ctl -d "$device" --list-formats-ext); then
    pass "auto mode restored the base output"
else
    fail "auto mode did not restore the base output"
fi

timeout 6 v4l2-ctl -d "$device" --stream-mmap --stream-count=150 >/dev/null 2>&1 &
reader_pid=$!
sleep 2
if grep -q 'Profile: *ctl-fill' <("$vcam" --socket "$socket" status); then
    pass "auto profile selected for the consumer"
else
    fail "auto profile was not selected"
fi
wait "$reader_pid"
sleep 3
if grep -q 'Profile:' <("$vcam" --socket "$socket" status); then
    fail "profile still reported after the consumer left"
else
    pass "profile released after the consumer left"
fi
"$vcam" --socket "$socket" profile auto off >/dev/null

# Restore a predictable format for the placeholder check below.
"$vcam" --socket "$socket" output --width 1280 --height 720 --fps 30 --pixel-format NV12 >/dev/null
sleep 1

say "stop and shutdown"
"$vcam" --socket "$socket" stop >/dev/null
sleep 0.5
capture_frame "$capture"
read -r mean distinct < <(frame_stats "$capture")
if [[ "$mean" == "16.00" ]]; then
    pass "placeholder is restored after stop"
else
    fail "placeholder not restored (mean=$mean)"
fi
"$vcam" --socket "$socket" shutdown >/dev/null
sleep 0.5
if [[ ! -S "$socket" ]]; then
    pass "daemon removed its socket on shutdown"
else
    fail "socket still present after shutdown"
fi

say "result"
if [[ "$failures" -eq 0 ]]; then
    printf '\033[32mall integration checks passed\033[0m\n'
    exit 0
fi
printf '\033[31m%d check(s) failed\033[0m\n' "$failures"
echo "daemon log: $log"
exit 1
