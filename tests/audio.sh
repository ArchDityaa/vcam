#!/usr/bin/env bash
#
# End-to-end test for the virtual microphone companion.
#
# Starts a private daemon (own socket, own module names) so an already running
# `vcamd` is left untouched, plays a generated clip with an audio track and
# checks that:
#   * the remapped source appears and carries signal while playing,
#   * pausing settles to silence,
#   * `vcam audio off` removes the modules and `on` recreates them,
#   * stopping the daemon cleans the modules up.
#
# Requires: pactl, parec, python3, gst-launch-1.0, vorbisenc/openh264enc,
# a loadable v4l2loopback device and a reachable PulseAudio/PipeWire server.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${VCAM_BIN:-$root/target/debug}"
device="${VCAM_DEVICE:-/dev/video10}"

for tool in pactl parec python3 gst-launch-1.0; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "skip: $tool is not installed"
        exit 0
    fi
done
if [[ ! -c "$device" ]]; then
    echo "skip: $device does not exist"
    exit 0
fi
if ! pactl info >/dev/null 2>&1; then
    echo "skip: no PulseAudio/PipeWire server is reachable"
    exit 0
fi
if pgrep -x vcamd >/dev/null 2>&1; then
    echo "skip: another vcamd is running and holds $device; stop it first"
    exit 0
fi

tmp="$(mktemp -d)"
daemon=""
cleanup() {
    if [[ -n "$daemon" ]] && kill -0 "$daemon" 2>/dev/null; then
        kill "$daemon" 2>/dev/null || true
        wait "$daemon" 2>/dev/null || true
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT

sink="vcam_test_sink"
source="vcam_test_mic"

cat >"$tmp/config.toml" <<EOF
[device]
path = "$device"

[daemon]
socket = "$tmp/vcam.sock"
log_level = "info"

[source]
path = "$tmp/av.mkv"
loop = true

[audio]
enabled = true
sink_name = "$sink"
source_name = "$source"
label = "vcam test microphone"
sample_rate = 48000
channels = 2
EOF

echo "==> generating a short clip with an audio track"
gst-launch-1.0 -q -e \
    videotestsrc num-buffers=150 ! video/x-raw,width=640,height=480,framerate=30/1 ! \
    openh264enc ! h264parse ! m.video_0 \
    audiotestsrc num-buffers=250 freq=440 ! audio/x-raw,rate=48000,channels=2 ! \
    audioconvert ! audioresample ! vorbisenc ! m.audio_0 \
    matroskamux name=m ! filesink location="$tmp/av.mkv" >/dev/null 2>&1

echo "==> starting a private daemon"
"$bin/vcamd" --config "$tmp/config.toml" >"$tmp/vcamd.log" 2>&1 &
daemon=$!
for _ in $(seq 1 50); do
    [[ -S "$tmp/vcam.sock" ]] && break
    sleep 0.1
done
[[ -S "$tmp/vcam.sock" ]] || {
    echo "FAIL: daemon did not start"
    if grep -q "failed to start virtual camera pipeline" "$tmp/vcamd.log"; then
        echo "hint: is $device already held by another producer?"
    fi
    cat "$tmp/vcamd.log"
    exit 1
}

vcam() { "$bin/vcam" --socket "$tmp/vcam.sock" "$@"; }

echo "==> checking the microphone node"
if ! pactl list short sources | grep -q "$source"; then
    echo "FAIL: $source was not created"
    pactl list short sources
    exit 1
fi
description="$(pactl list sources | sed -n "/Name: ${source}\$/,/^[^\t]/p" | sed -n 's/^\tDescription: //p')"
if [[ "$description" != "vcam test microphone" ]]; then
    echo "FAIL: description is '$description', expected 'vcam test microphone'"
    exit 1
fi

rms() {
    python3 - "$1" "${2:-0}" <<'PY'
import math, struct, sys
data = open(sys.argv[1], "rb").read()
# Skip the first N milliseconds: a capture client buffers a moment of audio
# from before it started, so the beginning of a capture is not representative.
skip = int(48000 * 2 * 2 * int(sys.argv[2]) / 1000)
data = data[skip:]
count = len(data) // 2
if count == 0:
    print(0)
    raise SystemExit
values = struct.unpack("<%dh" % count, data[: count * 2])
print(int(math.sqrt(sum(v * v for v in values) / count)))
PY
}

capture() {
    timeout 3 parec --device="$source" --format=s16le --rate=48000 --channels=2 \
        >"$1" 2>/dev/null || true
}

echo "==> playing and capturing"
vcam start --source "$tmp/av.mkv" >/dev/null
sleep 1
capture "$tmp/playing.raw"
playing="$(rms "$tmp/playing.raw" 1000)"
echo "    RMS while playing: $playing"
[[ "$playing" -gt 100 ]] || { echo "FAIL: no signal on $source"; exit 1; }

echo "==> pausing must silence the microphone"
vcam pause >/dev/null
sleep 1
capture "$tmp/paused.raw"
paused="$(rms "$tmp/paused.raw" 1000)"
echo "    RMS while paused: $paused"
[[ "$paused" -lt 50 ]] || { echo "FAIL: microphone is not silent while paused"; exit 1; }

echo "==> resuming"
vcam resume >/dev/null
sleep 1
capture "$tmp/resumed.raw"
resumed="$(rms "$tmp/resumed.raw" 1000)"
echo "    RMS after resume: $resumed"
[[ "$resumed" -gt 100 ]] || { echo "FAIL: no signal after resume"; exit 1; }

echo "==> audio off removes the modules"
vcam audio off >/dev/null
if pactl list short sources | grep -q "$source"; then
    echo "FAIL: $source still exists after 'vcam audio off'"
    exit 1
fi

echo "==> audio on recreates them, restarting does not duplicate them"
vcam audio on >/dev/null
count="$(pactl list short sources | grep -c "$source")"
[[ "$count" == "1" ]] || { echo "FAIL: expected one $source, found $count"; exit 1; }
vcam audio on >/dev/null
count="$(pactl list short sources | grep -c "$source")"
[[ "$count" == "1" ]] || { echo "FAIL: duplicate $source after repeated enable"; exit 1; }

echo "==> stopping the daemon removes the modules"
vcam shutdown >/dev/null || true
wait "$daemon" 2>/dev/null || true
daemon=""
for _ in $(seq 1 50); do
    pactl list short sources | grep -q "$source" || break
    sleep 0.1
done
if pactl list short sources | grep -q "$source"; then
    echo "FAIL: $source survived daemon shutdown"
    exit 1
fi

echo "audio companion: all checks passed"
