//! Environment diagnostics for vcam.
//!
//! This lives in its own crate so that `vcam doctor` can run without a daemon
//! — which is exactly when it is most useful, for example when the device node
//! does not exist yet.

use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use gstreamer as gst;
use vcam_core::{access, compat, Check, Config, DoctorReport};

const REQUIRED_ELEMENTS: &[&str] = &[
    "appsrc",
    "appsink",
    "v4l2sink",
    "decodebin3",
    "videoconvert",
    "videoscale",
    "videorate",
    "capsfilter",
];

pub fn run(config: &Config, device: &Path) -> DoctorReport {
    let mut checks = Vec::new();

    let module_loaded = Path::new("/sys/module/v4l2loopback").exists();
    checks.push(Check::new(
        "v4l2loopback module",
        module_loaded,
        if module_loaded {
            "loaded".to_string()
        } else {
            "not loaded; run: sudo modprobe v4l2loopback".to_string()
        },
    ));

    if module_loaded {
        let version = std::fs::read_to_string("/sys/module/v4l2loopback/version")
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let exclusive =
            std::fs::read_to_string("/sys/module/v4l2loopback/parameters/exclusive_caps")
                .map(|value| value.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
        checks.push(Check::new(
            "module configuration",
            true,
            format!("version {version}, exclusive_caps={exclusive}"),
        ));
    }

    let exists = device.exists();
    checks.push(Check::new(
        "device node",
        exists,
        if exists {
            device.display().to_string()
        } else {
            format!(
                "{} does not exist; load v4l2loopback with video_nr={}",
                device.display(),
                config.device.video_nr
            )
        },
    ));

    if exists {
        let metadata = std::fs::metadata(device);
        let char_device = metadata
            .as_ref()
            .map(|metadata| metadata.file_type().is_char_device())
            .unwrap_or(false);
        checks.push(Check::new(
            "device type",
            char_device,
            if char_device {
                "character device".to_string()
            } else {
                "not a character device".to_string()
            },
        ));

        let access = access::inspect(device);
        checks.push(Check::new(
            "device permissions",
            access.writable && access.readable,
            access.detail(),
        ));
        if !access.is_ok() {
            // A bare "permission denied" is what makes this class of problem
            // expensive to debug, so spell out the fix.
            checks.push(Check::new(
                "device access remedy",
                false,
                access.remediation().join(" | "),
            ));
        }
        if matches!(access.state, access::AccessState::StaleSession { .. }) {
            checks.push(Check::new(
                "session group membership",
                false,
                "applications started from this session (Discord, Chrome, ...) cannot see the camera until the session is restarted".to_string(),
            ));
        }

        let sys_name = device
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let card_name = std::fs::read_to_string(format!("/sys/class/video4linux/{sys_name}/name"))
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let label_ok = card_name
            .to_ascii_lowercase()
            .contains(&config.device.label.to_ascii_lowercase());
        checks.push(Check::new(
            "card label",
            label_ok,
            format!(
                "kernel reports '{card_name}', configured '{}'",
                config.device.label
            ),
        ));
    }

    for element in REQUIRED_ELEMENTS {
        let found = gst::ElementFactory::find(element).is_some();
        checks.push(Check::new(
            format!("gstreamer element {element}"),
            found,
            if found {
                "available".to_string()
            } else {
                "missing; install the appropriate gstreamer1.0-plugins package".to_string()
            },
        ));
    }

    let has_h264_decoder = gst::ElementFactory::find("avdec_h264").is_some()
        || gst::ElementFactory::find("vah264dec").is_some();
    checks.push(Check::new(
        "H.264 decoder",
        has_h264_decoder,
        if has_h264_decoder {
            "available".to_string()
        } else {
            "missing; install gstreamer1.0-libav or gstreamer1.0-plugins-bad".to_string()
        },
    ));

    // Chromium-based applications are the most common source of a "camera is
    // not listed" report, so report what the daemon will do about them.
    let chromium_apps = compat::scan();
    let shim = config.compatibility.chromium_yuyv;
    let shim_state = if shim { "enabled" } else { "disabled" };
    checks.push(Check::new(
        "chromium/electron apps",
        true,
        if chromium_apps.is_empty() {
            format!(
                "none running; the YUYV compatibility shim is {shim_state} ({} configured)",
                config.output.pixel_format
            )
        } else {
            format!(
                "{} running ({}); the YUYV compatibility shim is {shim_state}",
                chromium_apps.len(),
                chromium_apps.join(", ")
            )
        },
    ));

    if let Some(source) = &config.source.path {
        let exists = source.is_file();
        checks.push(Check::new(
            "configured source",
            exists,
            if exists {
                source.display().to_string()
            } else {
                format!("{} is not a file", source.display())
            },
        ));
    }

    // The virtual microphone is best effort: a missing sound server must not
    // make the camera look broken, so the backend check is informational and
    // only the GStreamer elements it would need are hard requirements.
    if config.audio.enabled {
        match vcam_core::audio::server_info() {
            Ok(info) => {
                let backend = vcam_core::audio::parse_backend(&info);
                let present =
                    vcam_core::audio::source_exists(&config.audio.source_name).unwrap_or(false);
                checks.push(Check::new(
                    "audio backend",
                    true,
                    format!(
                        "{backend} reachable; '{}' is {}",
                        config.audio.source_name,
                        if present {
                            "loaded"
                        } else {
                            "not loaded yet (start vcamd or run: vcam audio on)"
                        }
                    ),
                ));
            }
            Err(error) => checks.push(Check::new(
                "audio backend",
                true,
                format!("unavailable ({error}); the camera is unaffected"),
            )),
        }
        for element in ["pulsesink", "audioconvert", "audioresample", "volume"] {
            let found = gst::ElementFactory::find(element).is_some();
            checks.push(Check::new(
                format!("gstreamer element {element}"),
                found,
                if found {
                    "available".to_string()
                } else {
                    "missing; install the appropriate gstreamer1.0-plugins package".to_string()
                },
            ));
        }
    } else {
        checks.push(Check::new(
            "audio companion",
            true,
            "disabled in the configuration".to_string(),
        ));
    }

    let ok = checks.iter().all(|check| check.ok);
    DoctorReport { ok, checks }
}
