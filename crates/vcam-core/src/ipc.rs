use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Event, Status};

/// Request sent by a client. `id` is echoed in the response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

impl Request {
    pub fn new(id: u64, command: Command) -> Self {
        Self { id, command }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Status,
    Start {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loop_video: Option<bool>,
    },
    Stop,
    Pause,
    Resume,
    Seek {
        position_ms: u64,
    },
    SetSource {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loop_video: Option<bool>,
    },
    SetLoop {
        enabled: bool,
    },
    SetOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fps: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pixel_format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mirror: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rotate: Option<u32>,
        /// Apply even when applications are using the camera.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        force: Option<bool>,
    },
    /// Apply one of the configured `[[profiles]]`.
    SetProfile {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        force: Option<bool>,
    },
    /// Turn automatic profile selection on or off.
    SetProfileAuto {
        enabled: bool,
    },
    /// Turn the Chromium/Electron `YUYV` compatibility shim on or off.
    SetCompatibility {
        enabled: bool,
    },
    /// Report the configured profiles and the active one.
    Profiles,
    Doctor,
    Reload,
    Shutdown,
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Start { .. } => "start",
            Self::Stop => "stop",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Seek { .. } => "seek",
            Self::SetSource { .. } => "set_source",
            Self::SetLoop { .. } => "set_loop",
            Self::SetOutput { .. } => "set_output",
            Self::SetProfile { .. } => "set_profile",
            Self::SetProfileAuto { .. } => "set_profile_auto",
            Self::SetCompatibility { .. } => "set_compatibility",
            Self::Profiles => "profiles",
            Self::Doctor => "doctor",
            Self::Reload => "reload",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: u64, data: impl Serialize) -> Self {
        Self {
            id,
            ok: true,
            data: Some(serde_json::to_value(data).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub fn empty(id: u64) -> Self {
        Self {
            id,
            ok: true,
            data: None,
            error: None,
        }
    }

    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

/// Any message the daemon can write to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(Response),
    Event(Event),
}

impl ServerMessage {
    pub fn status(status: &Status) -> Self {
        Self::Event(Event::status(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_internally_tagged() {
        let text = serde_json::to_string(&Command::Pause).unwrap();
        assert_eq!(text, r#"{"command":"pause"}"#);

        let request = Request::new(7, Command::Seek { position_ms: 1200 });
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains(r#""id":7"#));
        assert!(text.contains(r#""command":"seek""#));
        assert!(text.contains(r#""position_ms":1200"#));
    }

    #[test]
    fn response_and_event_are_distinguishable() {
        let response = ServerMessage::Response(Response::empty(1));
        let event = ServerMessage::Event(Event::message("hello"));
        assert!(serde_json::to_string(&response)
            .unwrap()
            .contains(r#""ok":true"#));
        assert!(serde_json::to_string(&event)
            .unwrap()
            .contains(r#""event":"message""#));
    }
}
