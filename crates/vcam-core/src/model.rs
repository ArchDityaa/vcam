use serde::{Deserialize, Serialize};

use crate::consumers::Consumer;

/// Playback state reported by the daemon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    #[default]
    Idle,
    Playing,
    Paused,
    Error,
}

/// Fixed output format negotiated with the virtual camera device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixel_format: String,
    pub fit: String,
    pub mirror: bool,
    pub rotate: u32,
}

/// Snapshot of the daemon state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub state: PlaybackState,
    pub source: Option<String>,
    pub loop_video: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub output: OutputSpec,
    pub device: String,
    pub label: String,
    pub message: Option<String>,
    pub version: String,
    /// Applications currently holding the device open.
    #[serde(default)]
    pub consumers: Vec<Consumer>,
    /// Description of an output change waiting for the device to become idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    /// Name of the profile currently applied automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Why the output format currently differs from the configured one, e.g.
    /// because a Chromium-family application is running and needs `YUYV`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
}

/// Result of a single diagnostic check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    pub fn new(name: impl Into<String>, ok: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<Check>,
}

/// Event pushed from the daemon to connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub data: serde_json::Value,
}

impl Event {
    pub fn status(status: &Status) -> Self {
        Self {
            event: "status".to_string(),
            data: serde_json::to_value(status).unwrap_or(serde_json::Value::Null),
        }
    }

    pub fn message(text: impl Into<String>) -> Self {
        Self {
            event: "message".to_string(),
            data: serde_json::json!({ "text": text.into() }),
        }
    }
}
