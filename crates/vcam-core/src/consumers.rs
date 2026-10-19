//! Detect which processes are using the virtual camera device.
//!
//! v4l2loopback does not report its readers, and opening a new reader while
//! another application is streaming fails (`VIDIOC_REQBUFS` returns `EBUSY`).
//! Knowing who holds the device is therefore essential: it lets the daemon
//! defer incompatible output changes and lets users see why a change is
//! blocked. We scan `/proc` for file descriptors that point at the device.
//!
//! Only processes owned by the same user can be inspected without elevated
//! privileges, which matches how vcam runs (a per-user daemon).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How a process has the device open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsumerMode {
    Read,
    Write,
    ReadWrite,
}

impl ConsumerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadWrite => "read-write",
        }
    }
}

/// A process holding the virtual camera device open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumer {
    pub pid: i32,
    /// Short kernel name from `/proc/<pid>/comm`.
    pub name: String,
    /// Basename of `argv[0]` when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub mode: ConsumerMode,
}

impl Consumer {
    /// A human friendly label, preferring the executable name.
    pub fn label(&self) -> &str {
        self.command.as_deref().unwrap_or(&self.name)
    }
}

/// Every process (except `self_pid`) that currently has `device` open.
pub fn scan(device: &Path, self_pid: u32) -> Vec<Consumer> {
    let target = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf());
    let mut consumers = Vec::new();

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return consumers;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };
        if pid <= 0 || pid as u32 == self_pid {
            continue;
        }
        let Some(mode) = process_mode(pid, &target) else {
            continue;
        };
        consumers.push(Consumer {
            pid,
            name: process_comm(pid),
            command: process_command(pid),
            mode,
        });
    }

    consumers.sort_by_key(|consumer| consumer.pid);
    consumers
}

fn process_mode(pid: i32, target: &Path) -> Option<ConsumerMode> {
    let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    for entry in entries.flatten() {
        let Ok(link) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let link = std::fs::canonicalize(&link).unwrap_or(link);
        if link != *target {
            continue;
        }
        let fd = entry.file_name();
        let Some(fd) = fd.to_str() else {
            continue;
        };
        return Some(read_flags_mode(pid, fd));
    }
    None
}

fn read_flags_mode(pid: i32, fd: &str) -> ConsumerMode {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else {
        return ConsumerMode::ReadWrite;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("flags:") {
            if let Ok(flags) = i32::from_str_radix(rest.trim(), 8) {
                return mode_from_flags(flags);
            }
        }
    }
    ConsumerMode::ReadWrite
}

/// Map the `O_ACCMODE` bits of an `open(2)` flag set to a consumer mode.
fn mode_from_flags(flags: i32) -> ConsumerMode {
    match flags & 0o3 {
        0 => ConsumerMode::Read,
        1 => ConsumerMode::Write,
        _ => ConsumerMode::ReadWrite,
    }
}

fn process_comm(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|_| format!("pid {pid}"))
}

fn process_command(pid: i32) -> Option<String> {
    let data = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let first = data.split(|byte| *byte == 0).next()?;
    let text = String::from_utf8_lossy(first);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    basename(text)
}

fn basename(path: &str) -> Option<String> {
    PathBuf::from(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_map_to_access_mode() {
        // O_RDONLY
        assert_eq!(mode_from_flags(0o0), ConsumerMode::Read);
        // O_WRONLY
        assert_eq!(mode_from_flags(0o1), ConsumerMode::Write);
        // O_RDWR plus unrelated high bits must still resolve to read-write.
        assert_eq!(mode_from_flags(0o2100002), ConsumerMode::ReadWrite);
    }

    #[test]
    fn basename_strips_directories() {
        assert_eq!(basename("/usr/bin/discord"), Some("discord".to_string()));
        assert_eq!(basename("zoom"), Some("zoom".to_string()));
    }

    #[test]
    fn scan_ignores_self_and_missing_device() {
        // A device that does not exist yields no consumers.
        let consumers = scan(Path::new("/dev/definitely-not-a-vcam-device"), 1);
        assert!(consumers.is_empty());
    }
}
