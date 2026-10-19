use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::model::OutputSpec;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Top-level configuration, normally stored in `~/.config/vcam/config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device: DeviceConfig,
    pub output: OutputConfig,
    pub source: SourceConfig,
    pub daemon: DaemonConfig,
    pub watchdog: WatchdogConfig,
    /// Compatibility shims for applications that do not follow the V4L2
    /// conventions to the letter.
    pub compatibility: CompatibilityConfig,
    /// Per-application output profiles, matched in order.
    pub profiles: Vec<Profile>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        // Relative source paths in a config file are resolved against the
        // directory that contains the config file, not the daemon's cwd.
        if let Some(source) = config.source.path.clone() {
            if source.is_relative() {
                let base = path.parent().unwrap_or_else(|| Path::new("."));
                config.source.path = Some(base.join(source));
            }
        }
        Ok(config)
    }

    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn default_path() -> PathBuf {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(dir).join("vcam/config.toml")
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".config/vcam/config.toml")
        } else {
            PathBuf::from("vcam.toml")
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        if let Some(path) = &self.daemon.socket {
            return path.clone();
        }
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(dir).join("vcam.sock");
        }
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        std::env::temp_dir().join(format!("vcam-{uid}.sock"))
    }

    pub fn device_path(&self) -> PathBuf {
        self.device.device_path()
    }

    pub fn validate(&self) -> Result<(), String> {
        self.device.validate()?;
        self.output.validate()?;
        if self.source.loop_video {
            // no-op: documents intent, loop is always valid
        }
        if self.daemon.log_level.is_empty() {
            return Err("daemon.log_level must not be empty".into());
        }
        self.watchdog.validate()?;
        for profile in &self.profiles {
            profile.validate(&self.output)?;
        }
        Ok(())
    }

    pub fn output_spec(&self) -> OutputSpec {
        self.output.spec()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    /// V4L2 loopback device number, i.e. `/dev/video10` for `10`.
    pub video_nr: u32,
    /// Explicit device path. When set, overrides `video_nr`.
    pub path: Option<PathBuf>,
    /// Card label shown to applications.
    pub label: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            video_nr: 10,
            path: None,
            label: "Virtual Camera".to_string(),
        }
    }
}

impl DeviceConfig {
    pub fn device_path(&self) -> PathBuf {
        match &self.path {
            Some(path) => path.clone(),
            None => PathBuf::from(format!("/dev/video{}", self.video_nr)),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.label.trim().is_empty() {
            return Err("device.label must not be empty".into());
        }
        if self.path.is_none() && self.video_nr > 255 {
            return Err("device.video_nr must be between 0 and 255".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    #[serde(rename = "NV12")]
    #[default]
    Nv12,
    #[serde(rename = "YUYV")]
    Yuyv,
}

impl PixelFormat {
    /// GStreamer video format name.
    pub fn gst_name(self) -> &'static str {
        match self {
            Self::Nv12 => "NV12",
            Self::Yuyv => "YUY2",
        }
    }

    /// V4L2 fourcc as displayed by `v4l2-ctl`.
    pub fn fourcc(self) -> &'static str {
        match self {
            Self::Nv12 => "NV12",
            Self::Yuyv => "YUYV",
        }
    }

    pub fn frame_size(self, width: u32, height: u32) -> usize {
        let pixels = width as usize * height as usize;
        match self {
            Self::Nv12 => pixels * 3 / 2,
            Self::Yuyv => pixels * 2,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "NV12" => Some(Self::Nv12),
            "YUYV" | "YUY2" => Some(Self::Yuyv),
            _ => None,
        }
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.fourcc())
    }
}

impl FromStr for PixelFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unsupported pixel format '{s}' (use NV12 or YUYV)"))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    #[default]
    Letterbox,
    Fill,
    Stretch,
}

impl FitMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Letterbox => "letterbox",
            Self::Fill => "fill",
            Self::Stretch => "stretch",
        }
    }
}

impl fmt::Display for FitMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FitMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "letterbox" | "fit" => Ok(Self::Letterbox),
            "fill" | "crop" => Ok(Self::Fill),
            "stretch" => Ok(Self::Stretch),
            _ => Err(format!(
                "unsupported fit mode '{s}' (use letterbox, fill, or stretch)"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixel_format: PixelFormat,
    pub fit: FitMode,
    pub mirror: bool,
    /// Clockwise rotation in degrees: 0, 90, 180, or 270.
    pub rotate: u32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            pixel_format: PixelFormat::Nv12,
            fit: FitMode::Letterbox,
            mirror: false,
            rotate: 0,
        }
    }
}

impl OutputConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(160..=3840).contains(&self.width) {
            return Err("output.width must be between 160 and 3840".into());
        }
        if !(120..=2160).contains(&self.height) {
            return Err("output.height must be between 120 and 2160".into());
        }
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err("output.width and output.height must be even".into());
        }
        if !(1..=60).contains(&self.fps) {
            return Err("output.fps must be between 1 and 60".into());
        }
        if !matches!(self.rotate, 0 | 90 | 180 | 270) {
            return Err("output.rotate must be 0, 90, 180, or 270".into());
        }
        Ok(())
    }

    pub fn frame_size(&self) -> usize {
        self.pixel_format.frame_size(self.width, self.height)
    }

    pub fn frame_duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(1.0 / self.fps as f64)
    }

    pub fn frame_duration_ns(&self) -> u64 {
        1_000_000_000 / self.fps as u64
    }

    pub fn spec(&self) -> OutputSpec {
        OutputSpec {
            width: self.width,
            height: self.height,
            fps: self.fps,
            pixel_format: self.pixel_format.fourcc().to_string(),
            fit: self.fit.as_str().to_string(),
            mirror: self.mirror,
            rotate: self.rotate,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceConfig {
    pub path: Option<PathBuf>,
    #[serde(rename = "loop")]
    pub loop_video: bool,
    pub autostart: bool,
    pub hw_decode: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            path: None,
            loop_video: true,
            autostart: false,
            hw_decode: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub socket: Option<PathBuf>,
    pub log_level: String,
    /// Apply a matching `[[profiles]]` entry automatically when an application
    /// starts using the camera.
    pub auto_profiles: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket: None,
            log_level: "info".to_string(),
            auto_profiles: false,
        }
    }
}

/// Compatibility shims for applications that do not follow the V4L2
/// conventions to the letter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompatibilityConfig {
    /// Advertise `YUYV` while a Chromium/Electron application is running.
    ///
    /// Chromium-based applications (Discord, Chrome, Brave, Slack, ...)
    /// routinely fail to list a loopback device that only offers `NV12`, and
    /// show up as soon as the device offers `YUYV`. The daemon watches for those
    /// processes and switches the output to `YUYV` while any of them is
    /// running (deferred while an application is streaming, exactly like any
    /// other caps change), then returns to the configured pixel format.
    ///
    /// Turn it off with `vcam compat off` or `chromium_yuyv = false`.
    pub chromium_yuyv: bool,
}

impl Default for CompatibilityConfig {
    fn default() -> Self {
        Self {
            // On by default: the failure it prevents is silent and users
            // otherwise have to know that YUYV is the fix.
            chromium_yuyv: true,
        }
    }
}

/// Health limits for the pipeline watchdog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchdogConfig {
    pub enabled: bool,
    /// Restart the output pipeline above this resident set size.
    pub max_rss_mb: u64,
    /// Consider the output stalled after this many milliseconds without a
    /// successful frame push while playing.
    pub stall_timeout_ms: u64,
    /// Recover after this many consecutive failed frame pushes.
    pub max_push_failures: u32,
    /// Exit the daemon (letting systemd restart it) after this many failed
    /// recovery attempts.
    pub max_recoveries: u32,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rss_mb: 1024,
            stall_timeout_ms: 2000,
            max_push_failures: 30,
            max_recoveries: 5,
        }
    }
}

impl WatchdogConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_rss_mb < 64 {
            return Err("watchdog.max_rss_mb must be at least 64".into());
        }
        if self.stall_timeout_ms < 200 {
            return Err("watchdog.stall_timeout_ms must be at least 200".into());
        }
        if self.max_push_failures == 0 {
            return Err("watchdog.max_push_failures must be greater than zero".into());
        }
        if self.max_recoveries == 0 {
            return Err("watchdog.max_recoveries must be greater than zero".into());
        }
        Ok(())
    }
}

/// A partial output configuration, used by `[[profiles]]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputPatch {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub pixel_format: Option<PixelFormat>,
    pub fit: Option<FitMode>,
    pub mirror: Option<bool>,
    pub rotate: Option<u32>,
}

impl OutputPatch {
    /// Resolve the patch against a base configuration.
    pub fn apply(&self, base: &OutputConfig) -> OutputConfig {
        let mut output = base.clone();
        if let Some(width) = self.width {
            output.width = width;
        }
        if let Some(height) = self.height {
            output.height = height;
        }
        if let Some(fps) = self.fps {
            output.fps = fps;
        }
        if let Some(pixel_format) = self.pixel_format {
            output.pixel_format = pixel_format;
        }
        if let Some(fit) = self.fit {
            output.fit = fit;
        }
        if let Some(mirror) = self.mirror {
            output.mirror = mirror;
        }
        if let Some(rotate) = self.rotate {
            output.rotate = rotate;
        }
        output
    }

    /// Whether applying the patch changes the caps negotiated with the device
    /// (as opposed to decoder-only options such as fitting or rotation).
    pub fn touches_caps(&self) -> bool {
        self.width.is_some()
            || self.height.is_some()
            || self.fps.is_some()
            || self.pixel_format.is_some()
    }
}

/// An output profile selected when a matching application uses the camera.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    /// Case-insensitive substrings matched against the consumer executable.
    #[serde(rename = "match")]
    pub match_names: Vec<String>,
    #[serde(flatten)]
    pub patch: OutputPatch,
}

impl Profile {
    pub fn matches(&self, candidate: &str) -> bool {
        let candidate = candidate.to_ascii_lowercase();
        self.match_names.iter().any(|pattern| {
            let pattern = pattern.to_ascii_lowercase();
            !pattern.is_empty() && candidate.contains(&pattern)
        })
    }

    pub fn validate(&self, base: &OutputConfig) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("each [[profiles]] entry needs a name".into());
        }
        if self.match_names.is_empty() {
            return Err(format!(
                "profile '{}' needs at least one match pattern",
                self.name
            ));
        }
        self.patch
            .apply(base)
            .validate()
            .map_err(|error| format!("profile '{}': {error}", self.name))
    }
}

/// The first profile whose patterns match any of the given executables.
pub fn select_profile<'a>(profiles: &'a [Profile], executables: &[String]) -> Option<&'a Profile> {
    profiles.iter().find(|profile| {
        executables
            .iter()
            .any(|executable| profile.matches(executable))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.device_path(), PathBuf::from("/dev/video10"));
        assert_eq!(cfg.output.frame_size(), 1280 * 720 * 3 / 2);
        // The Chromium shim is on by default so that Discord/Chrome users do
        // not have to discover the YUYV requirement themselves.
        assert!(cfg.compatibility.chromium_yuyv);
    }

    #[test]
    fn serializes_the_compatibility_section() {
        let text = toml::to_string(&Config::default()).unwrap();
        assert!(text.contains("[compatibility]"));
        assert!(text.contains("chromium_yuyv = true"));
    }

    #[test]
    fn validates_even_dimensions() {
        let mut cfg = Config::default();
        cfg.output.width = 1279;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn parses_fit_and_pixel_format() {
        assert_eq!(FitMode::from_str("crop").unwrap(), FitMode::Fill);
        assert_eq!(FitMode::from_str("letterbox").unwrap(), FitMode::Letterbox);
        assert_eq!(PixelFormat::from_str("yuy2").unwrap(), PixelFormat::Yuyv);
        assert!(PixelFormat::from_str("rgb").is_err());
    }

    #[test]
    fn serializes_loop_as_loop() {
        let cfg = Config::default();
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("loop = true"));
        assert!(!text.contains("loop_video"));
    }

    #[test]
    fn selects_first_matching_profile() {
        let profiles = vec![
            Profile {
                name: "discord".to_string(),
                match_names: vec!["discord".to_string()],
                patch: OutputPatch {
                    pixel_format: Some(PixelFormat::Yuyv),
                    ..OutputPatch::default()
                },
            },
            Profile {
                name: "obs".to_string(),
                match_names: vec!["obs".to_string()],
                patch: OutputPatch::default(),
            },
        ];
        let selected = select_profile(&profiles, &["Discord".to_string()]).unwrap();
        assert_eq!(selected.name, "discord");
        assert!(select_profile(&profiles, &["firefox".to_string()]).is_none());
    }

    #[test]
    fn profile_patch_applies_and_keeps_base() {
        let base = OutputConfig {
            height: 480,
            ..OutputConfig::default()
        };
        let profile = Profile {
            name: "discord".to_string(),
            match_names: vec!["discord".to_string()],
            patch: OutputPatch {
                pixel_format: Some(PixelFormat::Yuyv),
                ..OutputPatch::default()
            },
        };
        let resolved = profile.patch.apply(&base);
        assert_eq!(resolved.pixel_format, PixelFormat::Yuyv);
        assert_eq!(resolved.height, 480);
        assert_eq!(resolved.width, base.width);
    }
}
