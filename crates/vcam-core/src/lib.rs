//! Shared types and configuration for the vcam virtual camera stack.

pub mod access;
pub mod audio;
pub mod compat;
pub mod config;
pub mod consumers;
pub mod ipc;
pub mod model;
pub mod preview;
pub mod util;

pub use access::{AccessState, DeviceAccess};
pub use config::{
    select_profile, AudioConfig, CompatibilityConfig, Config, DaemonConfig, DeviceConfig, FitMode,
    OutputConfig, OutputPatch, PixelFormat, Profile, SourceConfig, WatchdogConfig,
};
pub use consumers::{Consumer, ConsumerMode};
pub use model::{AudioStatus, Check, DoctorReport, Event, OutputSpec, PlaybackState, Status};
pub use util::{format_time_ms, parse_time_ms};
