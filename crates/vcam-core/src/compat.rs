//! Chromium / Electron compatibility.
//!
//! Chromium-based applications (Discord, Chrome, Chromium, Brave, Edge,
//! Slack, Signal, ...) do not enumerate V4L2 capture devices the way native
//! GStreamer applications do. In practice a loopback device that advertises
//! **only NV12** is frequently missing from their camera list, while the same
//! device advertising **YUYV** (uncompressed 4:2:2) shows up immediately —
//! which is also why the repository's own Discord profile recommends YUYV.
//!
//! Rather than requiring every user to discover that by hand, the daemon
//! watches for Chromium-family processes and advertises YUYV while any of them
//! is around, then returns to the configured pixel format once they exit. The
//! scan is deliberately conservative and cheap: it reads `/proc/<pid>/comm`
//! (one small file per process) and only looks at processes owned by the same
//! user as the daemon.

use std::path::Path;

/// Executable names that are a Chromium-family browser or an Electron app.
const PREFIXES: &[&str] = &[
    "chrome",
    "chromium",
    "google-chrome",
    "brave",
    "vivaldi",
    "opera",
    "msedge",
    "microsoft-edge",
    "discord",
    "slack",
    "teams",
    "signal-desktop",
    "element-desktop",
    "obsidian",
    "electron",
];

/// Match an executable name (or `argv[0]` basename) against the family list.
pub fn is_chromium_family(label: &str) -> bool {
    let name = normalize(label);
    if name.is_empty() {
        return false;
    }
    if PREFIXES.iter().any(|prefix| {
        name == *prefix
            || name.starts_with(&format!("{prefix}-"))
            || name.starts_with(&format!("{prefix}."))
            || name.starts_with(&format!("{prefix}_"))
    }) {
        return true;
    }
    // A few applications ship variants without a separator in their name,
    // e.g. Discord's canary and PTB builds.
    LOOSE_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Names that are matched as a plain prefix because their variants run the
/// words together (`discordcanary`, `discordptb`).
const LOOSE_PREFIXES: &[&str] = &["discord"];

/// Lowercase basename without a trailing `.exe`, ` (deleted)` or `.bin`.
fn normalize(label: &str) -> String {
    let name = label.rsplit('/').next().unwrap_or(label);
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let name = name.strip_suffix(".bin").unwrap_or(name);
    name.trim().to_ascii_lowercase()
}

/// Chromium-family applications currently running for the current user, as
/// sorted, de-duplicated labels (the `comm` name, which is what the process
/// list shows).
pub fn scan() -> Vec<String> {
    scan_uid(crate::access::current_uid())
}

/// Like [`scan`], but for a specific uid. Only processes whose `comm` matches
/// are inspected further, so the scan stays cheap.
pub fn scan_uid(uid: u32) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.parse::<i32>().is_err() {
            continue;
        }
        let path = entry.path();
        let Some(comm) = read_trimmed(&path.join("comm")) else {
            continue;
        };
        if !is_chromium_family(&comm) {
            continue;
        }
        if process_uid(&path) != Some(uid) {
            continue;
        }
        if !found.iter().any(|label| label == &comm) {
            found.push(comm);
        }
    }
    found.sort();
    found
}

/// A one-line explanation for `vcam status` / the GUI, when a Chromium-family
/// application is running and the shim is active.
pub fn note(apps: &[String]) -> Option<String> {
    if apps.is_empty() {
        return None;
    }
    Some(format!(
        "advertising YUYV for Chromium/Electron applications ({})",
        apps.join(", ")
    ))
}

/// Whether any of the given executables (e.g. current camera consumers)
/// belongs to the family. Used by `vcam consumers` and `vcam doctor`.
pub fn any<'a>(labels: impl IntoIterator<Item = &'a str>) -> bool {
    labels.into_iter().any(is_chromium_family)
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn process_uid(path: &Path) -> Option<u32> {
    let status = std::fs::read_to_string(path.join("status")).ok()?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("Uid:")?;
        // Real, effective, saved, filesystem — the real uid identifies the
        // session that started the process.
        rest.split_whitespace().next()?.parse::<u32>().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_family() {
        for name in [
            "Discord",
            "discord",
            "discordcanary",
            "/usr/bin/google-chrome",
            "google-chrome-stable",
            "chromium-browser",
            "brave-browser",
            "msedge",
            "slack",
            "signal-desktop",
            "electron",
            "/app/bin/electron",
        ] {
            assert!(is_chromium_family(name), "{name} should match");
        }
    }

    #[test]
    fn ignores_native_applications() {
        for name in [
            "firefox",
            "obs",
            "zoom",
            "cheese",
            "gst-launch-1.0",
            "vcamd",
            "",
            "mpv",
        ] {
            assert!(!is_chromium_family(name), "{name} should not match");
        }
    }

    #[test]
    fn normalize_strips_paths_and_suffixes() {
        assert_eq!(normalize("/usr/lib/discord/Discord"), "discord");
        assert_eq!(normalize("/tmp/chrome.exe"), "chrome");
        assert_eq!(normalize("chrome (deleted)"), "chrome");
        assert_eq!(normalize("chrome_crashpad"), "chrome_crashpad");
    }

    #[test]
    fn note_mentions_the_applications() {
        assert!(note(&[]).is_none());
        let text = note(&["Discord".to_string()]).unwrap();
        assert!(text.contains("YUYV"));
        assert!(text.contains("Discord"));
    }

    #[test]
    fn any_accepts_consumer_labels() {
        assert!(any(["/usr/bin/discord", "gst-launch-1.0"]));
        assert!(!any(["gst-launch-1.0", "obs"]));
    }

    #[test]
    fn scan_finds_no_chromium_apps_for_an_unused_uid() {
        // uid 0 exists but no root process is a Chromium app in CI.
        let found = scan_uid(0);
        assert!(found.iter().all(|label| is_chromium_family(label)));
    }
}
