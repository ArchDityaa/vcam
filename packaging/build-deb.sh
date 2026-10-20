#!/usr/bin/env bash
#
# Build a self-contained .deb for vcam using only dpkg-deb (no debhelper).
#
# Usage:
#   packaging/build-deb.sh [--debug]
#
# The resulting package installs:
#   /usr/bin/vcamd                     daemon (user service, no root)
#   /usr/bin/vcam                      command line client
#   /usr/bin/vcam-gui                  GTK4 control panel
#   /usr/lib/systemd/user/vcamd.service
#   /usr/lib/modprobe.d/v4l2loopback.conf
#   /usr/lib/modules-load.d/v4l2loopback.conf
#   /usr/lib/udev/rules.d/70-vcam.rules
#   /usr/share/applications/io.github.ArchDityaa.Vcam.desktop
#   /usr/share/icons/hicolor/scalable/apps/io.github.ArchDityaa.Vcam.svg
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

profile="release"
profile_flag="--release"
if [[ "${1:-}" == "--debug" ]]; then
    profile="debug"
    profile_flag=""
fi

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
arch="$(dpkg --print-architecture)"
pkg="vcam_${version}_${arch}"
stage="$root/target/deb/$pkg"

echo "==> building vcam $version ($arch, $profile)"
cargo build $profile_flag -p vcamd -p vcam-cli -p vcam-gui

echo "==> staging into $stage"
rm -rf "$stage"
mkdir -p "$stage/DEBIAN"
mkdir -p "$stage/usr/bin"
mkdir -p "$stage/usr/lib/systemd/user"
mkdir -p "$stage/usr/lib/modprobe.d"
mkdir -p "$stage/usr/lib/modules-load.d"
mkdir -p "$stage/usr/lib/udev/rules.d"
mkdir -p "$stage/usr/share/applications"
mkdir -p "$stage/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$stage/usr/share/man/man1"
mkdir -p "$stage/usr/share/doc/vcam"

install -m 0755 "target/$profile/vcamd"    "$stage/usr/bin/vcamd"
install -m 0755 "target/$profile/vcam"     "$stage/usr/bin/vcam"
install -m 0755 "target/$profile/vcam-gui" "$stage/usr/bin/vcam-gui"

install -m 0644 packaging/systemd/vcamd.service            "$stage/usr/lib/systemd/user/vcamd.service"
install -m 0644 packaging/modprobe.d/v4l2loopback.conf     "$stage/usr/lib/modprobe.d/v4l2loopback.conf"
install -m 0644 packaging/modules-load.d/v4l2loopback.conf "$stage/usr/lib/modules-load.d/v4l2loopback.conf"
install -m 0644 packaging/udev/70-vcam.rules               "$stage/usr/lib/udev/rules.d/70-vcam.rules"
install -m 0644 packaging/desktop/io.github.ArchDityaa.Vcam.desktop "$stage/usr/share/applications/io.github.ArchDityaa.Vcam.desktop"
install -m 0644 packaging/icons/io.github.ArchDityaa.Vcam.svg       "$stage/usr/share/icons/hicolor/scalable/apps/io.github.ArchDityaa.Vcam.svg"
install -m 0644 packaging/man/vcam.1 packaging/man/vcamd.1 packaging/man/vcam-gui.1 "$stage/usr/share/man/man1/"
gzip -9n "$stage"/usr/share/man/man1/*.1
install -m 0644 README.md docs/architecture.md docs/troubleshooting.md "$stage/usr/share/doc/vcam/"

cat > "$stage/DEBIAN/control" <<EOF
Package: vcam
Version: $version
Section: video
Priority: optional
Architecture: $arch
Depends: libc6, libgstreamer1.0-0, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, gstreamer1.0-libav, libgtk-4-1, libadwaita-1-0
Recommends: v4l2loopback-dkms, gstreamer1.0-tools, v4l-utils, pulseaudio-utils
Maintainer: ArchDityaa <ArchDityaa@users.noreply.github.com>
Description: Virtual camera that feeds a video into a V4L2 device
 vcam keeps a v4l2loopback device open with a userspace GStreamer
 producer so that Chrome, Firefox, Zoom, Teams, Discord, OBS and
 OpenCV see a normal webcam. It supports looping, hot swapping the
 source, pause/resume/seek, a black idle placeholder, and a stable
 output resolution and frame rate. The source's audio track can be exposed
 as a matching virtual microphone through a PulseAudio or PipeWire null sink.
 Ships a daemon, a CLI and a GTK4 control panel.
EOF

cat > "$stage/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    if command -v udevadm >/dev/null 2>&1; then
        udevadm control --reload-rules || true
        udevadm trigger --subsystem-match=video4linux || true
    fi
    if command -v systemctl >/dev/null 2>&1; then
        systemctl daemon-reload || true
    fi
    # postinst runs as root, so check the invoking user's groups, not root's.
    user="${SUDO_USER:-$(id -un)}"
    if ! id -nG "$user" 2>/dev/null | grep -qw video; then
        echo "vcam: add $user to the 'video' group: sudo usermod -aG video $user" >&2
        echo "vcam: ...then log out and back in: group changes only reach new sessions," >&2
        echo "vcam: and applications started before that cannot see the camera." >&2
    fi
    echo "vcam: load the module now with: sudo modprobe v4l2loopback" >&2
    echo "vcam: if an application does not list 'Virtual Camera', run: vcam doctor" >&2
fi
EOF
chmod 0755 "$stage/DEBIAN/postinst"

cat > "$stage/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user stop vcamd.service 2>/dev/null || true
    fi
fi
EOF
chmod 0755 "$stage/DEBIAN/prerm"

echo "==> building package"
dpkg-deb --root-owner-group --build "$stage" "$root/target/deb/$pkg.deb"
echo "==> wrote target/deb/$pkg.deb"
