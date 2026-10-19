# Contributing to vcam

Thanks for taking the time to contribute. This document explains how to build,
test and submit changes.

## Code of conduct

By participating you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Prerequisites

- Linux with `v4l2loopback` (Ubuntu 24.04 / Debian 13 or newer recommended)
- Rust 1.83 or newer (`rustup` is the easiest way to get it)
- GStreamer 1.24 development headers
- GTK 4.14 and libadwaita 1.5 development headers (for `vcam-gui`)

```sh
sudo apt install build-essential pkg-config \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev libgtk-4-dev libadwaita-1-dev
```

## Build and run

```sh
cargo build                    # debug build
cargo build --release          # optimized build

./target/debug/vcamd --log-level debug          # terminal 1
./target/debug/vcam start --source movie.mp4    # terminal 2
./target/debug/vcam status
./target/debug/vcam-gui                         # optional
```

The daemon must run as your user. Your account needs to be in the `video`
group to open `/dev/video10`.

## Tests

```sh
cargo test --workspace         # unit tests
cargo fmt --all -- --check     # formatting
cargo clippy --workspace --all-targets -- -D warnings
```

End-to-end tests need a real device and must run while no other `vcamd` owns it:

```sh
systemctl --user stop vcamd
tests/integration.sh target/release
```

Long-run stability test (30 minutes by default):

```sh
DURATION=1800 tests/stability.sh target/release
```

## Style

- Run `cargo fmt` before committing.
- Keep `cargo clippy -- -D warnings` clean.
- Prefer small, focused commits.
- User-facing behaviour changes need a `CHANGELOG.md` entry.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(daemon): defer caps changes while readers are active
fix(gui): keep the preview alive when the device is busy
docs: explain the watchdog backoff
test: cover profile matching
chore: bump dependencies
```

## Pull requests

1. Fork the repository and create a topic branch.
2. Make your change, add tests where practical, and update the docs.
3. Make sure the checks above pass.
4. Open a pull request describing the problem and the approach.

Please keep pull requests focused; unrelated refactors are easier to review in a
separate PR.

## Reporting bugs

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.yml) and include:

- distribution and kernel version;
- `v4l2loopback` version and module parameters;
- `vcam doctor` output;
- relevant `journalctl --user -u vcamd` lines;
- the application that does not see the camera.

For security issues, follow [SECURITY.md](SECURITY.md) instead of opening a
public issue.
