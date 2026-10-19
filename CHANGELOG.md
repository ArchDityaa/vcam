# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-20

### Added

- **Chromium/Electron compatibility shim** — the daemon watches the process
  table for Chromium-family applications (Discord, Chrome, Brave, Slack,
  Signal, ...) and advertises `YUYV` while any of them is running, because they
  routinely fail to list a loopback device that only offers `NV12`. The
  configured pixel format returns when they exit. On by default; toggle with
  `vcam compat on|off`, configure with `[compatibility] chromium_yuyv`.
- **Access diagnosis** — `vcam-core::access` classifies why the device cannot
  be opened, including the case that is invisible to `id`: the account is in
  the `video` group but the current session started before the change, so every
  application in it silently misses the camera. `vcam doctor` reports it with
  the commands that fix it, and `vcamd` now refuses to start with that
  explanation instead of a bare GStreamer state-change error.
- `vcam doctor` reports the Chromium/Electron state, `vcam consumers` labels
  Chromium-family readers, and `vcam status` explains a format that differs
  from the configuration (`Status::compatibility`).
- `[[profiles]]` matching, the Chromium shim and the base `[output]` are now
  resolved by a single `desired_output` precedence rule.

### Changed

- The udev rule carries `TAG+="uaccess"`, so systemd-logind grants the active
  session user access without waiting for a re-login after `usermod -aG video`.
- `debian:postinst` explains the re-login requirement instead of only checking
  group membership.
- `Output::new` distinguishes a permission failure from an already-busy device
  in its error message.

## [0.1.0] - 2026-10-20

First public release.

### Added

- **`vcamd`** — a user daemon that holds a `v4l2loopback` device open and feeds
  it from a GStreamer pipeline. It keeps the device alive with a black
  placeholder while idle, so applications that enumerate cameras at startup
  never race the producer.
- **`vcam`** — a command line client for playback control, output shaping,
  diagnostics and daemon management.
- **`vcam-gui`** — a GTK4 / libadwaita control panel with a live preview.
- **Stable output caps** — configurable resolution, frame rate and pixel format
  (`NV12` or `YUYV`), with letterbox / fill / stretch scaling, mirroring and
  0/90/180/270 rotation.
- **Playback features** — looping, hot-swapping the source, pause/resume and
  seeking, with automatic recovery to the placeholder on errors.
- **Daemon-fed preview** — the daemon publishes a downscaled RGB frame in the
  runtime directory, so the GUI preview keeps working while another application
  is streaming the device (a second reader is rejected by v4l2loopback with
  `EBUSY`).
- **Camera-user detection** — `vcam consumers` scans `/proc` for processes that
  hold the device and reports their access mode.
- **Deferred output changes** — changing the device caps while an application is
  streaming is deferred until the camera is free, because renegotiating caps
  under a live reader can wedge v4l2loopback. Changes that do not touch the caps
  (scaling, mirroring, rotation) apply immediately. `--force` overrides.
- **Per-application profiles** — `[[profiles]]` entries select an output format
  automatically based on the applications using the camera
  (`[daemon] auto_profiles = true`).
- **Self-healing watchdog** — detects stalls, repeated push failures, bus errors
  and memory leaks, rebuilds the output pipeline with exponential backoff, and
  exits as a last resort so the systemd unit restarts a clean process.
- **Diagnostics** — `vcam doctor` checks the module, device node, permissions,
  card label and required GStreamer elements.
- **Packaging** — a self-contained `.deb`, systemd user unit, udev rule,
  `modprobe.d` / `modules-load.d` configuration, desktop entry, icon and man
  pages.
- **Testing** — unit tests, an end-to-end integration script and a long-run
  stability script.
