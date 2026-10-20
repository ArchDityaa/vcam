# Troubleshooting

Start with:

```sh
vcam doctor
```

It checks the module, the device node, permissions, the card label, the
GStreamer elements that vcam needs, and — when the virtual microphone is
enabled — the `pactl` backend and the `vcam_sink`/`vcam_mic` nodes. The sections
below explain the failures it reports.

## The module does not load

Symptoms:

```
modprobe: ERROR: could not insert 'v4l2loopback': Unknown symbol in module
```

or `vcam doctor` reports that `/dev/video10` is missing.

The loopback module needs the `videodev` kernel module, which on some Ubuntu
kernels ships in `linux-modules-extra`:

```sh
sudo apt install "linux-modules-extra-$(uname -r)"
sudo modprobe v4l2loopback
```

Load it with the right options:

```sh
sudo modprobe v4l2loopback \
  video_nr=10 card_label="Virtual Camera" exclusive_caps=1 max_buffers=4
```

`exclusive_caps=1` matters. Without it, some applications cache the device's
*output* capabilities before the writer starts and then refuse to open it.

## Permission denied opening /dev/video10

`vcam doctor` classifies this for you; run it first. The device node belongs to
`root:video` with mode `0660`, so a process can only open it when its own
credentials carry the `video` group. There are two different reasons why they
might not, and they need different fixes.

**The account is not in the group.** `vcam doctor` reports
`'you' is not a member of the 'video' group`:

```sh
sudo usermod -aG video "$USER"
```

**The account is in the group, but the session predates the change.** This is
the case that costs people hours, because `id` in a fresh shell happily lists
`video` while the running session does not actually carry it. Everything
started from that session — Discord, Chrome, your terminal — inherits the stale
set, cannot open the device, and silently drops the camera. `vcam doctor`
reports it explicitly:

```
[FAIL] session group membership
       applications started from this session (Discord, Chrome, ...) cannot
       see the camera until the session is restarted
```

Log out and back in. There is no way to inject a supplementary group into an
already running process, and that is true for every application, not just
vcam. As a stopgap for the daemon itself:

```sh
sg video -c vcamd
```

The packaged udev rule carries `TAG+="uaccess"`, which lets systemd-logind
grant the active session user an ACL on the node, so on a normal desktop the
problem does not arise in the first place. If you are on a machine without
logind (a container, a headless box), the `video` group remains the way in.

If the udev rule is not installed, the node may be `root:root`. Copy
`packaging/udev/70-vcam.rules` to `/etc/udev/rules.d/` and run:

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=video4linux
```

## Applications do not list the camera

1. `vcam doctor` — it reports the device, its permissions, why they are wrong if
   they are, and whether a Chromium/Electron application is running.
2. Confirm the daemon is running and the device is streaming:
   ```sh
   vcam status
   v4l2-ctl -d /dev/video10 --list-formats-ext
   ```
3. Restart the application after the daemon starts, or reload the page in the
   browser. Some applications only enumerate cameras at startup.
4. Make sure only one process writes to the device. Two producers fight over it
   and the result is usually `not-negotiated`.

### Discord, Chrome, Brave, Slack, Signal (Chromium and Electron)

Chromium-based applications do not use GStreamer to enumerate cameras; they
talk to V4L2 directly, and in practice they frequently fail to list a loopback
device that only advertises `NV12`. The same device advertising `YUYV`
appears immediately.

vcam handles this automatically. While any Chromium-family process is running,
the daemon advertises `YUYV` and `vcam status` says so:

```
Compat:   advertising YUYV for Chromium/Electron applications (Discord)
```

When those applications exit the configured pixel format is restored. The
switch happens while the camera is free, so it is safe to leave on; the change
is deferred like any other caps change if something is already streaming.

```sh
vcam compat off     # advertise the configured format no matter what runs
vcam compat on      # the default
```

If the camera is still missing, check in this order:

1. The compatibility shim is enabled: `vcam status`, or `vcam compat on`.
2. The application started *after* the daemon. An application that enumerated
   the camera before the daemon held the device has nothing to open. Restart it
   (in Chrome, `chrome://restart` is faster than a full restart).
3. `chrome://media-internals` — a camera blocked by site permissions is not a
   device problem.
4. In Discord: **Settings → Voice & Video → Camera**, then the refresh icon.
   Discord caches the device list aggressively; a full restart is sometimes the
   only way to clear it.

## The virtual microphone does not appear, or is silent

`vcam audio status` (and the **Audio** tab in the GUI) reports the sink and
source names, the backend and any error:

```sh
vcam audio status
pactl info
pactl list short sources | grep vcam_mic
```

If the source is missing, turn the companion on again:

```sh
vcam audio on
```

If that reports `pactl was not found`, install the control client
(`sudo apt install pulseaudio-utils`); PipeWire also needs `pipewire-pulse` for
the PulseAudio compatibility server. The camera itself does not need either, so
a missing sound server never prevents playback.

**It is silent while nothing is playing, and for sources without an audio
track.** The microphone stays connected on purpose, so applications can select
it before playback starts and keep it selected across sources. It is also silent
while paused, and the short pipeline run used by paused seeks is muted by
design.

If the node exists but applications do not list it, restart the application (or
reload the page in Chrome, or use the refresh button in Discord's voice
settings); many applications enumerate microphones only at startup.

If a crashed daemon left modules behind with names from an older configuration,
adopt or remove them instead of stacking duplicates:

```sh
pactl list short modules | grep -E 'null-sink|remap-source'
vcam audio off && vcam audio on
```

After a sound-server restart (for example a PipeWire upgrade) the modules are
gone even though the daemon is still running. `vcam audio on` recreates them
without restarting the daemon; `systemctl --user restart vcamd` also works.

## `not-negotiated` or `pipeline doesn't want to preroll`

This means the caps the daemon offers are not a subset of what `v4l2sink`
accepts. vcam already adds `pixel-aspect-ratio=1/1`, `interlace-mode` and
`colorimetry`; if you changed the output code and removed them, negotiation
fails. See [architecture.md](architecture.md#why-the-caps-spell-everything-out).

## The camera shows black

- Nothing is playing and the placeholder is being shown. Run
  `vcam start --source FILE`.
- The source failed to decode. Check `vcam status` for an error message and the
  daemon log (`journalctl --user -u vcamd`).

## Playback is too fast or position jumps around

This is what happens when the decoder `appsink` does not sync to the pipeline
clock: it decodes as fast as it can while the controller only keeps the newest
frame. vcam sets `sync=true` on the decoder appsink for exactly this reason.

## Seek snaps back to the beginning

A key-unit seek on a clip whose only keyframe is at zero always lands at zero.
vcam uses accurate seeks instead, so this should not happen; if you see it, make
sure you are not calling `seek_simple` with `KEY_UNIT` somewhere.

## The GUI preview is empty but applications work

The preview is rendered from `$XDG_RUNTIME_DIR/vcam-preview.raw`, which the
daemon rewrites about ten times per second. If that file is missing or stale the
daemon is not running, or has not produced a frame yet:

```sh
systemctl --user status vcamd
vcam status
```

When the daemon is idle the preview shows the black placeholder, which is
exactly what the virtual camera is sending. Press **Play** to start a source.

## An output change does nothing

If `vcam status` shows a `Pending:` line, the change was deferred because an
application is using the camera:

```sh
vcam consumers
```

The format can only be renegotiated once those applications release the camera
(close the camera in Discord/Zoom/... or quit them). Changes that do not touch
the device caps — `--fit`, `--mirror`, `--rotate` — apply immediately. Use
`vcam output ... --force` to apply anyway; this can wedge the device, in which
case the watchdog rebuilds it or restarts the daemon.

## A profile is not applied

Automatic profiles only run when `daemon.auto_profiles` is on:

```sh
vcam profile auto on
vcam profile list
```

A profile that changes the resolution or pixel format is deferred exactly like a
manual `vcam output` change, and shows up in `Pending:`. A profile applied by
hand with `vcam profile use` pins it and disables automatic selection.

## The daemon restarts itself

That is the watchdog. Look at the reason:

```sh
journalctl --user -u vcamd -n 50
```

A line such as `watchdog is rebuilding the output pipeline` means it detected a
stall, a run of failed frame pushes, or a memory leak; `watchdog is restarting
vcamd` means it exhausted `max_recoveries` and exited so systemd could start a
clean process. If restarts are frequent, raise `max_rss_mb`, or set
`[watchdog] enabled = false` to investigate manually.

## High CPU usage

The daemon converts every decoded frame into the output format and copies it
into `appsrc`. At 1080p30 that is roughly 3 MB per frame. If CPU is a problem:

- lower the output resolution or frame rate with `vcam output`;
- use `NV12` instead of `YUYV` (YUV 4:2:0 is half the bandwidth of 4:2:2).

## Collecting logs

```sh
vcamd --log-level debug            # foreground, debug logging
journalctl --user -u vcamd -f     # when running as a user service
GST_DEBUG=3 vcamd                  # GStreamer element logging
```

## Resetting

```sh
vcam stop
vcam audio off      # unload vcam_sink/vcam_mic before stopping the daemon
vcam shutdown
pkill -x vcamd
rm -f "$XDG_RUNTIME_DIR/vcam.sock"
```

If a crashed daemon left the audio modules behind, remove them by name with
`pactl unload-module`; the command in
[the microphone section](#the-virtual-microphone-does-not-appear-or-is-silent)
shows how to find them.
