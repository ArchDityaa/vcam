//! Diagnostics for opening the virtual camera device node.
//!
//! `/dev/video10` is a `root:video 0660` character device, so a process can
//! only open it when *its own* credentials carry the `video` group. Two
//! situations look identical to a user but need different fixes:
//!
//! * the account is not in the group at all — `sudo usermod -aG video $USER`;
//! * the account was added to the group *after* the session started, so the
//!   login session still carries the old supplementary groups. Every
//!   application started from that session (Discord, Chrome, Zoom, ...)
//!   inherits the stale set, cannot open the device, and silently drops the
//!   camera from its device list.
//!
//! The second case is invisible to `id` in a fresh shell and is the usual
//! reason a camera "disappears" right after installation, so it gets its own
//! classification ([`AccessState::StaleSession`]) instead of a generic
//! "permission denied".
//!
//! Everything here is read-only and unprivileged: the daemon and
//! `vcam doctor` can both use it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const GROUP_FILE: &str = "/etc/group";
const PASSWD_FILE: &str = "/etc/passwd";

/// Why the device can or cannot be opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccessState {
    /// The device exists and this process may read and write it.
    Ok,
    /// The device node does not exist (the module is not loaded).
    Missing,
    /// The device is owned by a group the account is not a member of.
    NotInGroup { group: String, user: String },
    /// The account is in the group, but the running session predates the
    /// change, so neither this process nor the applications started from it
    /// carry the group yet.
    StaleSession { group: String, user: String },
    /// Not writable for any other reason (mode, or an unexpected owner).
    Denied { group: String, mode: u32 },
}

impl AccessState {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// Short, human readable reason used in doctor output and log lines.
    pub fn summary(&self) -> String {
        match self {
            Self::Ok => "writable".to_string(),
            Self::Missing => "device node does not exist".to_string(),
            Self::NotInGroup { group, user } => {
                format!("'{user}' is not a member of the '{group}' group")
            }
            Self::StaleSession { group, user } => format!(
                "'{user}' is in the '{group}' group, but this session started before that change"
            ),
            Self::Denied { group, mode } => {
                format!("not writable (mode {mode:04o}, group '{group}')")
            }
        }
    }

    /// Commands that fix the situation, in the order a user should try them.
    pub fn remediation(&self) -> Vec<String> {
        match self {
            Self::Ok => Vec::new(),
            Self::Missing => vec![
                "sudo modprobe v4l2loopback video_nr=10 card_label=\"Virtual Camera\" exclusive_caps=1 max_buffers=4".to_string(),
            ],
            Self::NotInGroup { group, user } => {
                let mut steps = vec![
                    format!("sudo usermod -aG {group} {user}"),
                    "log out and back in (group changes only reach new sessions)".to_string(),
                ];
                if group != "video" {
                    // A node owned by an unrelated group usually means the udev
                    // rule never ran, not that the user should join that group.
                    steps.push(
                        "the node is not owned by the 'video' group; reinstall the udev rule (packaging/udev/70-vcam.rules), then: sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=video4linux"
                            .to_string(),
                    );
                }
                steps
            }
            Self::StaleSession { group, user: _ } => vec![
                "log out and back in — applications started from this session will keep missing the camera until they are restarted from a new one".to_string(),
                format!("or start the daemon with: sg {group} -c vcamd"),
            ],
            Self::Denied { group, .. } => vec![
                "sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=video4linux".to_string(),
                format!("sudo chgrp {group} {DEVICE_PLACEHOLDER} && sudo chmod 0660 {DEVICE_PLACEHOLDER}"),
            ],
        }
    }
}

/// Placeholder replaced by [`DeviceAccess::remediation`] so that the state
/// (which is device independent and easy to test) can name the node.
const DEVICE_PLACEHOLDER: &str = "DEVICE";

/// Inspect `device` from the point of view of the current process.
pub fn inspect(device: &Path) -> DeviceAccess {
    DeviceAccess::inspect(device)
}

/// Everything known about reaching the device from this process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAccess {
    pub device: PathBuf,
    pub exists: bool,
    pub char_device: bool,
    pub uid: u32,
    pub gid: u32,
    /// Name of the group that owns the node, when it can be resolved.
    pub group: Option<String>,
    pub mode: u32,
    pub readable: bool,
    pub writable: bool,
    pub state: AccessState,
}

impl DeviceAccess {
    pub fn is_ok(&self) -> bool {
        self.state.is_ok()
    }

    /// One-line detail for `vcam doctor`.
    pub fn detail(&self) -> String {
        if !self.exists {
            return self.state.summary();
        }
        let group = self.group.clone().unwrap_or_else(|| self.gid.to_string());
        format!(
            "mode {:04o}, owner {}:{} — {}",
            self.mode,
            self.uid,
            group,
            self.state.summary()
        )
    }

    /// Multi-line, actionable explanation used when the daemon refuses to
    /// start, where a bare "permission denied" is useless.
    pub fn explain(&self) -> String {
        let mut lines = vec![format!(
            "cannot access the virtual camera {}: {}",
            self.device.display(),
            self.state.summary()
        )];
        let steps = self.remediation();
        if !steps.is_empty() {
            lines.push("How to fix it:".to_string());
            lines.extend(steps.into_iter().map(|step| format!("  {step}")));
        }
        lines.join("\n")
    }

    pub fn remediation(&self) -> Vec<String> {
        self.state
            .remediation()
            .into_iter()
            .map(|step| step.replace(DEVICE_PLACEHOLDER, &self.device.display().to_string()))
            .collect()
    }

    /// Inspect `device` from the point of view of the current process.
    pub fn inspect(device: &Path) -> Self {
        let uid = current_uid();
        let user = user_name(uid).unwrap_or_else(|| uid.to_string());
        let session_groups = session_groups();
        let account_groups = account_groups(&user);

        let Ok(metadata) = std::fs::metadata(device) else {
            return Self {
                device: device.to_path_buf(),
                exists: false,
                char_device: false,
                uid: 0,
                gid: 0,
                group: None,
                mode: 0,
                readable: false,
                writable: false,
                state: AccessState::Missing,
            };
        };

        let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o7777;
        let gid = std::os::unix::fs::MetadataExt::gid(&metadata);
        let owner_uid = std::os::unix::fs::MetadataExt::uid(&metadata);
        let group = group_name(gid);
        let readable = access_allowed(device, libc::R_OK);
        let writable = access_allowed(device, libc::R_OK | libc::W_OK);
        let char_device = std::os::unix::fs::FileTypeExt::is_char_device(&metadata.file_type());

        let state = classify(
            writable,
            owner_uid == uid,
            gid,
            group.as_deref(),
            &session_groups,
            &account_groups,
            &user,
            mode,
        );

        Self {
            device: device.to_path_buf(),
            exists: true,
            char_device,
            uid: owner_uid,
            gid,
            group,
            mode,
            readable,
            writable,
            state,
        }
    }
}

/// Decide why a device is not writable. Kept pure so the interesting cases can
/// be unit tested without a real device node.
#[allow(clippy::too_many_arguments)]
fn classify(
    writable: bool,
    owned_by_user: bool,
    gid: u32,
    group: Option<&str>,
    session_groups: &[u32],
    account_groups: &[u32],
    user: &str,
    mode: u32,
) -> AccessState {
    if writable {
        return AccessState::Ok;
    }
    let group_label = || group.map(str::to_string).unwrap_or_else(|| gid.to_string());
    if owned_by_user {
        // Owned by us but still refused: mode is the problem, not the group.
        return AccessState::Denied {
            group: group_label(),
            mode,
        };
    }
    if group.is_none() {
        return AccessState::Denied {
            group: group_label(),
            mode,
        };
    }
    if !account_groups.contains(&gid) {
        return AccessState::NotInGroup {
            group: group_label(),
            user: user.to_string(),
        };
    }
    if !session_groups.contains(&gid) {
        return AccessState::StaleSession {
            group: group_label(),
            user: user.to_string(),
        };
    }
    AccessState::Denied {
        group: group_label(),
        mode,
    }
}

fn access_allowed(path: &Path, mode: i32) -> bool {
    let Ok(path) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) else {
        return false;
    };
    // SAFETY: the CString is NUL-terminated and valid for the duration of the
    // call; access(2) only reads it.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

pub fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Supplementary groups attached to *this* process, i.e. what the session
/// inherited, not what the account is entitled to today.
pub fn session_groups() -> Vec<u32> {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        let groups = parse_groups_line(&status);
        if !groups.is_empty() {
            return groups;
        }
    }
    vec![current_gid()]
}

pub fn current_gid() -> u32 {
    // SAFETY: getegid has no preconditions and cannot fail.
    unsafe { libc::getegid() }
}

/// The groups an account *should* have, derived from `/etc/group` plus the
/// primary group from `/etc/passwd`.
pub fn account_groups(user: &str) -> Vec<u32> {
    let mut groups = Vec::new();
    if let Ok(text) = std::fs::read_to_string(PASSWD_FILE) {
        if let Some(entry) = parse_passwd_entries(&text)
            .into_iter()
            .find(|e| e.name == user)
        {
            groups.push(entry.gid);
        }
    }
    if let Ok(text) = std::fs::read_to_string(GROUP_FILE) {
        for entry in parse_group_entries(&text) {
            if entry.members.iter().any(|member| member == user) {
                groups.push(entry.gid);
            }
        }
    }
    groups.sort_unstable();
    groups.dedup();
    groups
}

pub fn group_name(gid: u32) -> Option<String> {
    let text = std::fs::read_to_string(GROUP_FILE).ok()?;
    parse_group_entries(&text)
        .into_iter()
        .find(|entry| entry.gid == gid)
        .map(|entry| entry.name)
}

pub fn group_id(name: &str) -> Option<u32> {
    let text = std::fs::read_to_string(GROUP_FILE).ok()?;
    parse_group_entries(&text)
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.gid)
}

pub fn user_name(uid: u32) -> Option<String> {
    let text = std::fs::read_to_string(PASSWD_FILE).ok()?;
    parse_passwd_entries(&text)
        .into_iter()
        .find(|entry| entry.uid == uid)
        .map(|entry| entry.name)
}

/// Parse the `Groups:` line of `/proc/<pid>/status`.
pub fn parse_groups_line(status: &str) -> Vec<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Groups:"))
        .map(|rest| {
            rest.split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupEntry {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// Parse an `/etc/group` style file.
pub fn parse_group_entries(text: &str) -> Vec<GroupEntry> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?.to_string();
            let _password = fields.next()?;
            let gid = fields.next()?.parse::<u32>().ok()?;
            let members = fields
                .next()
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|member| !member.is_empty())
                .map(str::to_string)
                .collect();
            Some(GroupEntry { name, gid, members })
        })
        .collect()
}

/// Parse an `/etc/passwd` style file.
pub fn parse_passwd_entries(text: &str) -> Vec<PasswdEntry> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?.to_string();
            let _password = fields.next()?;
            let uid = fields.next()?.parse::<u32>().ok()?;
            let gid = fields.next()?.parse::<u32>().ok()?;
            Some(PasswdEntry { name, uid, gid })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_group_lines() {
        let status = "Name:\tvcamd\nUid:\t1001\t1001\t1001\t1001\nGroups:\t4 100 118 999 1001 \n";
        assert_eq!(parse_groups_line(status), vec![4, 100, 118, 999, 1001]);
        assert!(parse_groups_line("Name:\tvcamd\n").is_empty());
    }

    #[test]
    fn parses_group_file() {
        let text = "# comment\nwheel:x:10:alice,bob\nvideo:x:44:runner\nempty:x:45:\n";
        let entries = parse_group_entries(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].name, "video");
        assert_eq!(entries[1].gid, 44);
        assert_eq!(entries[1].members, vec!["runner"]);
        assert!(entries[2].members.is_empty());
    }

    #[test]
    fn parses_passwd_file() {
        let text = "root:x:0:0:root:/root:/bin/bash\nrunner:x:1001:1001::/home/runner:/bin/bash\n";
        let entries = parse_passwd_entries(text);
        assert_eq!(entries[1].name, "runner");
        assert_eq!(entries[1].uid, 1001);
        assert_eq!(entries[1].gid, 1001);
    }

    #[test]
    fn writable_device_is_ok() {
        let state = classify(true, false, 44, Some("video"), &[], &[], "runner", 0o660);
        assert!(state.is_ok());
    }

    #[test]
    fn session_that_predates_the_group_change_is_reported() {
        // The account is in `video` (per /etc/group) but the session groups do
        // not include it — the Discord/Chrome "camera is missing" case.
        let state = classify(
            false,
            false,
            44,
            Some("video"),
            &[4, 100, 999],
            &[4, 44, 100, 999],
            "runner",
            0o660,
        );
        assert_eq!(
            state,
            AccessState::StaleSession {
                group: "video".to_string(),
                user: "runner".to_string(),
            }
        );
        let steps = state.remediation();
        assert!(steps[0].contains("log out and back in"));
    }

    #[test]
    fn missing_group_membership_is_reported() {
        let state = classify(
            false,
            false,
            44,
            Some("video"),
            &[4, 100],
            &[4, 100],
            "runner",
            0o660,
        );
        assert_eq!(
            state,
            AccessState::NotInGroup {
                group: "video".to_string(),
                user: "runner".to_string(),
            }
        );
        assert!(state.remediation()[0].contains("usermod -aG video runner"));
    }

    #[test]
    fn group_member_with_bad_mode_is_denied_not_stale() {
        let state = classify(
            false,
            false,
            44,
            Some("video"),
            &[44],
            &[44],
            "runner",
            0o600,
        );
        assert_eq!(
            state,
            AccessState::Denied {
                group: "video".to_string(),
                mode: 0o600,
            }
        );
    }

    #[test]
    fn unrelated_group_suggests_the_udev_rule() {
        let state = AccessState::NotInGroup {
            group: "root".to_string(),
            user: "runner".to_string(),
        };
        assert!(state.remediation().iter().any(|step| step.contains("udev")));
        let video = AccessState::NotInGroup {
            group: "video".to_string(),
            user: "runner".to_string(),
        };
        assert!(!video.remediation().iter().any(|step| step.contains("udev")));
    }

    #[test]
    fn unresolvable_group_is_denied() {
        let state = classify(false, false, 4242, None, &[], &[], "runner", 0o660);
        assert!(matches!(state, AccessState::Denied { .. }));
    }

    #[test]
    fn missing_device_is_reported() {
        let access = DeviceAccess::inspect(Path::new("/dev/definitely-not-a-vcam-device"));
        assert!(!access.exists);
        assert_eq!(access.state, AccessState::Missing);
        assert!(access.explain().contains("modprobe"));
    }

    #[test]
    fn remediation_names_the_device() {
        let state = AccessState::Denied {
            group: "video".to_string(),
            mode: 0o600,
        };
        let device = PathBuf::from("/dev/video10");
        let steps: Vec<String> = state
            .remediation()
            .into_iter()
            .map(|step| step.replace(DEVICE_PLACEHOLDER, &device.display().to_string()))
            .collect();
        assert!(steps.iter().any(|step| step.contains("/dev/video10")));
    }
}
