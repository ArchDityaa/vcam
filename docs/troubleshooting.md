# Troubleshooting

Start with:

```sh
vcam doctor
```

It checks the module, the device node, permissions, the card label and the
GStreamer elements that vcam needs. The sections below explain the failures it
reports.

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

The device node belongs to `root:video` with mode `0660`. Add yourself to the
`video` group and log out and back in:

```sh
sudo usermod -aG video "$USER"
```

If the udev rule is not installed, the node may be `root:root`. Copy
`packaging/udev/70-vcam.rules` to `/etc/udev/rules.d/` and run:

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=video4linux
```

## Applications do not list the camera

1. Confirm the daemon is running and the device is streaming:
   ```sh
   vcam status
   v4l2-ctl -d /dev/video10 --list-formats-ext
   ```
2. Restart the application after the daemon starts, or reload the page in the
   browser. Some applications only enumerate cameras at startup.
3. In Chrome, check `chrome://media-internals` and make sure the camera is not
   blocked by permissions.
4. Make sure only one process writes to the device. Two producers fight over it
   and the result is usually `not-negotiated`.

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
vcam shutdown
pkill -x vcamd
rm -f "$XDG_RUNTIME_DIR/vcam.sock"
```
