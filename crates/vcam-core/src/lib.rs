//! Shared types and configuration for the vcam virtual camera stack.

pub mod config;
pub mod consumers;
pub mod ipc;
pub mod model;
pub mod preview;
pub mod util;

pub use config::{
    select_profile, Config, DaemonConfig, DeviceConfig, FitMode, OutputConfig, OutputPatch,
    PixelFormat, Profile, SourceConfig, WatchdogConfig,
};
pub use consumers::{Consumer, ConsumerMode};
pub use model::{Check, DoctorReport, Event, OutputSpec, PlaybackState, Status};
pub use util::{format_time_ms, parse_time_ms};
