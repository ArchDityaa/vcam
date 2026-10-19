use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use vcam_core::ipc::{Command, Response};
use vcam_core::preview;
use vcam_core::util::expand_path;
use vcam_core::{
    consumers, select_profile, Config, Consumer, Event, FitMode, OutputConfig, PixelFormat,
    PlaybackState, Status,
};

use crate::decoder::{Decoder, DecoderEvent};
use crate::frame;
use crate::output::Output;
use crate::watchdog::{current_rss_bytes, Watchdog, WatchdogVerdict};

pub struct DaemonRequest {
    pub id: u64,
    pub command: Command,
    pub reply: oneshot::Sender<Response>,
}

pub struct ControllerHandle {
    pub command_tx: mpsc::UnboundedSender<DaemonRequest>,
    pub events: broadcast::Sender<Event>,
    pub socket_path: PathBuf,
    /// Notified when a client asks the daemon to shut down.
    pub shutdown_notify: Arc<Notify>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ControllerHandle {
    pub fn spawn(
        config: Config,
        config_path: PathBuf,
        device_override: Option<PathBuf>,
    ) -> Result<Self, String> {
        Controller::spawn(config, config_path, device_override)
    }

    pub fn shutdown(self) {
        self.signal_shutdown();
    }

    fn signal_shutdown(&self) {
        let (reply, _) = oneshot::channel();
        let _ = self.command_tx.send(DaemonRequest {
            id: 0,
            command: Command::Shutdown,
            reply,
        });
    }
}

impl Drop for ControllerHandle {
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A seek requested while playback is paused. The decoder is run briefly so
/// that a frame at the new position is actually produced, then paused again.
struct SeekPreview {
    target_ms: u64,
    deadline: Instant,
}

/// Fields requested by `Command::SetOutput`; `None` keeps the current value.
#[derive(Debug, Default, Clone)]
struct OutputUpdate {
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    pixel_format: Option<String>,
    fit: Option<String>,
    mirror: Option<bool>,
    rotate: Option<u32>,
}

struct Controller {
    config: Config,
    config_path: PathBuf,
    device: PathBuf,
    output: Output,
    decoder: Option<Decoder>,
    status: Status,
    last_frame: Vec<u8>,
    black_frame: Vec<u8>,
    command_rx: mpsc::UnboundedReceiver<DaemonRequest>,
    events: broadcast::Sender<Event>,
    shutdown: bool,
    output_error: Option<String>,
    seek_preview: Option<SeekPreview>,
    preview_path: PathBuf,
    preview_tick: u32,
    watchdog: Watchdog,
    consumers: Vec<Consumer>,
    next_consumer_poll: Instant,
    pending_output: Option<OutputConfig>,
    /// Output that applies when no profile is active.
    base_output: OutputConfig,
    /// Profile whose output change is deferred until the camera is free.
    pending_profile: Option<String>,
    auto_profile: bool,
    active_profile: Option<String>,
    shutdown_notify: Arc<Notify>,
}

impl Controller {
    pub fn spawn(
        config: Config,
        config_path: PathBuf,
        device_override: Option<PathBuf>,
    ) -> Result<ControllerHandle, String> {
        config.validate()?;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(64);
        let event_tx = events.clone();
        let socket_path = config.socket_path();
        let shutdown_notify = Arc::new(Notify::new());
        let shutdown_notify_thread = shutdown_notify.clone();

        let thread = std::thread::Builder::new()
            .name("vcamd-pipeline".to_string())
            .spawn(move || {
                match Controller::new(
                    config,
                    config_path,
                    device_override,
                    command_rx,
                    event_tx,
                    shutdown_notify_thread,
                ) {
                    Ok(mut controller) => {
                        let _ = ready_tx.send(Ok(()));
                        if let Err(error) = controller.run() {
                            tracing::error!(%error, "pipeline controller stopped");
                        }
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|error| format!("failed to spawn pipeline thread: {error}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(ControllerHandle {
                command_tx,
                events,
                socket_path,
                shutdown_notify,
                join: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => Err(format!("timed out waiting for the pipeline: {error}")),
        }
    }

    fn new(
        config: Config,
        config_path: PathBuf,
        device_override: Option<PathBuf>,
        command_rx: mpsc::UnboundedReceiver<DaemonRequest>,
        events: broadcast::Sender<Event>,
        shutdown_notify: Arc<Notify>,
    ) -> Result<Self, String> {
        let device = device_override.unwrap_or_else(|| config.device_path());
        if !device.exists() {
            return Err(format!(
                "virtual camera {} does not exist.\n\
                 Load the module first:\n\
                 sudo modprobe v4l2loopback video_nr={} card_label=\"{}\" exclusive_caps=1 max_buffers=4",
                device.display(),
                config.device.video_nr,
                config.device.label
            ));
        }

        let output = Output::new(&device, &config.output)?;
        let black_frame = frame::black_frame(&config.output);
        let status = Status {
            state: PlaybackState::Idle,
            source: None,
            loop_video: config.source.loop_video,
            position_ms: 0,
            duration_ms: 0,
            output: config.output.spec(),
            device: device.display().to_string(),
            label: config.device.label.clone(),
            message: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            consumers: Vec::new(),
            pending: None,
            profile: None,
        };

        let preview_path = preview::preview_path(&config.socket_path());
        let watchdog = Watchdog::new(&config.watchdog, Instant::now());
        let auto_profile = config.daemon.auto_profiles;
        let base_output = config.output.clone();
        let mut controller = Self {
            config,
            config_path,
            device,
            output,
            decoder: None,
            status,
            last_frame: black_frame.clone(),
            black_frame,
            command_rx,
            events,
            shutdown: false,
            output_error: None,
            seek_preview: None,
            preview_path,
            preview_tick: 0,
            watchdog,
            consumers: Vec::new(),
            next_consumer_poll: Instant::now(),
            pending_output: None,
            base_output,
            pending_profile: None,
            auto_profile,
            active_profile: None,
            shutdown_notify,
        };

        if controller.config.source.autostart {
            if let Some(path) = controller.config.source.path.clone() {
                let loop_video = controller.config.source.loop_video;
                if let Err(error) = controller.start_source(&path, loop_video) {
                    controller.status.state = PlaybackState::Error;
                    controller.status.message = Some(error);
                }
            }
        }

        Ok(controller)
    }

    fn run(&mut self) -> Result<(), String> {
        let mut next_frame = Instant::now();
        let mut last_status = Instant::now();
        let mut next_health = Instant::now();

        loop {
            if self.shutdown {
                break;
            }

            while let Ok(request) = self.command_rx.try_recv() {
                self.handle(request);
            }

            self.poll_decoder_events();

            let now = Instant::now();
            if now >= next_frame {
                self.tick_frame();
                next_frame += self.config.output.frame_duration();
                if next_frame <= now {
                    next_frame = now + self.config.output.frame_duration();
                }
            }

            // Reader detection is cheap enough to run once per second; it also
            // releases any output change that was deferred while an
            // application was using the camera.
            if now >= self.next_consumer_poll {
                self.poll_consumers();
                self.next_consumer_poll = now + Duration::from_secs(1);
            }

            if now >= next_health {
                self.check_health();
                next_health = now + Duration::from_millis(250);
            }

            if last_status.elapsed() >= Duration::from_millis(500) {
                self.refresh_position();
                self.emit_status();
                last_status = Instant::now();
            }

            std::thread::sleep(Duration::from_millis(2));
        }

        if let Some(decoder) = self.decoder.take() {
            decoder.stop();
        }
        self.output.stop();
        Ok(())
    }

    fn handle(&mut self, request: DaemonRequest) {
        let DaemonRequest { id, command, reply } = request;
        tracing::debug!(command = command.name(), "handling request");
        let response = match self.dispatch(command) {
            Ok(data) => Response::ok(id, data),
            Err(error) => Response::err(id, error),
        };
        let _ = reply.send(response);
    }

    fn dispatch(&mut self, command: Command) -> Result<Value, String> {
        match command {
            Command::Status => self.status_value(),
            Command::Start { source, loop_video } => {
                let source = source
                    .or_else(|| {
                        self.config
                            .source
                            .path
                            .as_ref()
                            .map(|path| path.display().to_string())
                    })
                    .ok_or_else(|| {
                        "no video configured; pass --source or set [source] path".to_string()
                    })?;
                let path = expand_path(&source);
                let loop_video = loop_video.unwrap_or(self.config.source.loop_video);
                if let Err(error) = self.start_source(&path, loop_video) {
                    self.status.message = Some(error.clone());
                    self.emit_status();
                    return Err(error);
                }
                self.status_value()
            }
            Command::Stop => {
                if let Some(decoder) = self.decoder.take() {
                    decoder.stop();
                }
                self.seek_preview = None;
                self.status.state = PlaybackState::Idle;
                self.status.source = None;
                self.status.position_ms = 0;
                self.status.duration_ms = 0;
                self.status.message = None;
                self.last_frame = self.black_frame.clone();
                self.emit_status();
                self.status_value()
            }
            Command::Pause => {
                if self.status.state != PlaybackState::Playing {
                    return Err("playback is not running".to_string());
                }
                let decoder = self
                    .decoder
                    .as_ref()
                    .ok_or_else(|| "no active source".to_string())?;
                decoder.pause()?;
                self.status.state = PlaybackState::Paused;
                self.refresh_position();
                self.emit_status();
                self.status_value()
            }
            Command::Resume => {
                if self.status.state != PlaybackState::Paused {
                    return Err("playback is not paused".to_string());
                }
                let decoder = self
                    .decoder
                    .as_ref()
                    .ok_or_else(|| "no active source".to_string())?;
                decoder.play()?;
                self.seek_preview = None;
                self.status.state = PlaybackState::Playing;
                self.emit_status();
                self.status_value()
            }
            Command::Seek { position_ms } => {
                let decoder = self
                    .decoder
                    .as_ref()
                    .ok_or_else(|| "no active source".to_string())?;
                decoder.seek(position_ms)?;
                self.status.position_ms = position_ms;
                if self.status.state == PlaybackState::Paused {
                    // A paused pipeline does not render the prerolled frame
                    // through appsink, so run it briefly to obtain a frame at
                    // the requested position, then pause again.
                    decoder.play()?;
                    self.seek_preview = Some(SeekPreview {
                        target_ms: position_ms,
                        deadline: Instant::now() + Duration::from_millis(800),
                    });
                }
                self.emit_status();
                self.status_value()
            }
            Command::SetSource { path, loop_video } => {
                let was_paused = self.status.state == PlaybackState::Paused;
                let path = expand_path(&path);
                let loop_video = loop_video.unwrap_or(self.config.source.loop_video);
                if let Err(error) = self.start_source(&path, loop_video) {
                    self.status.message = Some(error.clone());
                    self.emit_status();
                    return Err(error);
                }
                if was_paused {
                    if let Some(decoder) = &self.decoder {
                        decoder.pause()?;
                    }
                    self.status.state = PlaybackState::Paused;
                    self.emit_status();
                }
                self.status_value()
            }
            Command::SetLoop { enabled } => {
                self.config.source.loop_video = enabled;
                self.status.loop_video = enabled;
                if let Some(decoder) = self.decoder.as_mut() {
                    decoder.set_loop(enabled);
                }
                self.emit_status();
                self.status_value()
            }
            Command::SetOutput {
                width,
                height,
                fps,
                pixel_format,
                fit,
                mirror,
                rotate,
                force,
            } => {
                let output = self.merged_output(OutputUpdate {
                    width,
                    height,
                    fps,
                    pixel_format,
                    fit,
                    mirror,
                    rotate,
                })?;
                let target = output.clone();
                self.apply_output(output, force.unwrap_or(false))?;
                self.base_output = target;
                self.status_value()
            }
            Command::SetProfile { name, force } => {
                let profile = self
                    .config
                    .profiles
                    .iter()
                    .find(|profile| profile.name == name)
                    .cloned()
                    .ok_or_else(|| format!("no profile named '{name}'"))?;
                let target = profile.patch.apply(&self.base_output);
                // An explicit choice wins over automatic selection until auto
                // mode is turned back on.
                self.auto_profile = false;
                self.config.daemon.auto_profiles = false;
                self.pending_profile = None;
                if target != self.config.output {
                    self.apply_output(target.clone(), force.unwrap_or(false))?;
                }
                if self.config.output == target {
                    self.active_profile = Some(profile.name.clone());
                } else {
                    // Deferred until the camera becomes free.
                    self.active_profile = None;
                    self.pending_profile = Some(profile.name.clone());
                }
                self.status.profile = self.active_profile.clone();
                self.emit_status();
                self.status_value()
            }
            Command::SetProfileAuto { enabled } => {
                self.auto_profile = enabled;
                self.config.daemon.auto_profiles = enabled;
                if enabled {
                    self.reconcile_profile();
                }
                self.emit_status();
                self.status_value()
            }
            Command::Profiles => {
                let profiles: Vec<Value> = self
                    .config
                    .profiles
                    .iter()
                    .map(|profile| {
                        json!({
                            "name": profile.name,
                            "match": profile.match_names,
                            "output": profile.patch.apply(&self.base_output).spec(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "profiles": profiles,
                    "active": self.active_profile,
                    "auto": self.auto_profile,
                }))
            }
            Command::Doctor => {
                let report = vcam_doctor::run(&self.config, &self.device);
                serde_json::to_value(report).map_err(|error| error.to_string())
            }
            Command::Reload => self.reload(),
            Command::Shutdown => {
                self.shutdown = true;
                // Wake the async runtime so the whole process exits and the
                // socket is removed, not just the pipeline thread.
                self.shutdown_notify.notify_one();
                Ok(json!({ "shutdown": true }))
            }
        }
    }

    fn start_source(&mut self, path: &Path, loop_video: bool) -> Result<(), String> {
        let decoder = Decoder::new(
            &self.config.output,
            path,
            loop_video,
            self.config.source.hw_decode,
        )?;
        decoder.play()?;

        if let Some(previous) = self.decoder.take() {
            previous.stop();
        }
        self.decoder = Some(decoder);
        self.seek_preview = None;
        self.config.source.path = Some(path.to_path_buf());
        self.config.source.loop_video = loop_video;

        self.status.state = PlaybackState::Playing;
        self.status.source = Some(path.display().to_string());
        self.status.loop_video = loop_video;
        self.status.position_ms = 0;
        self.status.duration_ms = self
            .decoder
            .as_ref()
            .map(|decoder| decoder.duration_ms())
            .unwrap_or(0);
        self.status.message = None;
        self.output_error = None;
        self.emit_status();
        Ok(())
    }

    fn tick_frame(&mut self) {
        // Drain the decoder even while paused: a seek performed during pause
        // briefly runs the pipeline so a frame at the new position appears.
        let mut pulled_pts = None;
        if let Some(decoder) = &self.decoder {
            if let Some((frame, pts)) = decoder.pull_latest() {
                if frame.len() == self.black_frame.len() {
                    self.last_frame = frame;
                    if let Some(pts) = pts {
                        // The pipeline position query returns zero while
                        // paused on this decodebin3 pipeline, so derive the
                        // reported position from the frame actually shown.
                        pulled_pts = Some(pts.mseconds());
                        self.status.position_ms = pts.mseconds();
                    }
                }
            }
        }

        if let Some(preview) = self.seek_preview.take() {
            let reached = pulled_pts
                .map(|position| position.saturating_add(50) >= preview.target_ms)
                .unwrap_or(false);
            if reached || Instant::now() >= preview.deadline {
                if let Some(decoder) = &self.decoder {
                    if let Err(error) = decoder.pause() {
                        self.fail(error);
                        return;
                    }
                }
                self.emit_status();
            } else {
                self.seek_preview = Some(preview);
            }
        }

        match self.output.push(&self.last_frame) {
            Ok(()) => {
                self.output_error = None;
                self.watchdog.record_success(Instant::now());
            }
            Err(error) => {
                self.watchdog.record_failure();
                if self.output_error.as_deref() != Some(error.as_str()) {
                    tracing::warn!(%error, "failed to write frame to virtual camera");
                    self.output_error = Some(error);
                }
            }
        }

        // Publish a small preview for GUI clients at roughly 10 fps. The GUI
        // cannot open a second reader on the device once another application is
        // streaming it, so it renders from this file instead.
        let every = (self.config.output.fps / 10).max(1);
        self.preview_tick = self.preview_tick.wrapping_add(1);
        if self.preview_tick % every == 0 {
            self.write_preview();
        }
    }

    fn write_preview(&self) {
        let (width, height) = preview::preview_size(&self.config.output);
        let blob = preview::render(&self.last_frame, &self.config.output, width, height);
        // Write to a temporary file and rename so readers never see a partial
        // frame (rename is atomic on the same filesystem).
        let mut tmp = self.preview_path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        if std::fs::write(&tmp, &blob).is_ok() {
            let _ = std::fs::rename(&tmp, &self.preview_path);
        }
    }

    fn poll_consumers(&mut self) {
        let consumers = consumers::scan(&self.device, std::process::id());
        if consumers != self.consumers {
            self.consumers = consumers;
            self.status.consumers = self.consumers.clone();
            self.reconcile_profile();
            self.emit_status();
        }
        self.reconcile_pending();
    }

    /// Apply the first `[[profiles]]` entry matching a current consumer, or
    /// restore the base output when nothing matches. A change that would
    /// renegotiate the device caps is deferred until the camera is free.
    fn reconcile_profile(&mut self) {
        if !self.auto_profile || self.config.profiles.is_empty() {
            return;
        }
        let executables: Vec<String> = self
            .consumers
            .iter()
            .map(|consumer| consumer.label().to_string())
            .collect();
        let (name, target) = match select_profile(&self.config.profiles, &executables).cloned() {
            Some(profile) => {
                let target = profile.patch.apply(&self.base_output);
                (Some(profile.name), target)
            }
            None => (None, self.base_output.clone()),
        };

        if target != self.config.output {
            if let Err(error) = self.apply_output(target.clone(), false) {
                self.status.message = Some(error);
                self.emit_status();
                return;
            }
        }
        // Only report a profile as active once its output is actually applied.
        let applied = self.config.output == target;
        self.active_profile = if applied { name.clone() } else { None };
        self.pending_profile = if applied { None } else { name };
        self.status.profile = self.active_profile.clone();
    }

    /// Apply an output change that was deferred while applications were using
    /// the camera.
    fn reconcile_pending(&mut self) {
        if !self.consumers.is_empty() {
            return;
        }
        let Some(pending) = self.pending_output.take() else {
            return;
        };
        self.status.pending = None;
        if let Err(error) = self.apply_output(pending, true) {
            self.status.message = Some(error);
        }
        if let Some(name) = self.pending_profile.take() {
            self.active_profile = Some(name);
            self.status.profile = self.active_profile.clone();
        }
        self.emit_status();
    }

    fn consumer_summary(&self) -> String {
        let mut names: Vec<&str> = self
            .consumers
            .iter()
            .map(|consumer| consumer.label())
            .collect();
        names.sort_unstable();
        names.dedup();
        if names.is_empty() {
            "an application".to_string()
        } else {
            names.join(", ")
        }
    }

    fn check_health(&mut self) {
        if !self.config.watchdog.enabled {
            return;
        }
        if let Some(error) = self.output.poll_error() {
            tracing::warn!(%error, "output pipeline reported an error");
            self.recover_output(&error);
            return;
        }
        let now = Instant::now();
        let playing = self.status.state == PlaybackState::Playing;
        let level_max = self.config.output.frame_size() as u64 * 2;
        let level = self.output.level_bytes();
        let rss = current_rss_bytes();
        tracing::trace!(level, level_max, rss, playing, "watchdog sample");
        let verdict = self.watchdog.check(now, rss, playing, level, level_max);
        match verdict {
            WatchdogVerdict::Healthy => {}
            WatchdogVerdict::Waiting(reason) => {
                if self.status.message.as_deref() != Some(reason.as_str()) {
                    self.status.message = Some(reason);
                    self.emit_status();
                }
            }
            WatchdogVerdict::Recover(reason) => {
                tracing::warn!(%reason, "watchdog is rebuilding the output pipeline");
                self.recover_output(&reason);
            }
            WatchdogVerdict::Restart(reason) => {
                tracing::error!(%reason, "watchdog is restarting vcamd");
                std::process::exit(1);
            }
        }
    }

    fn recover_output(&mut self, reason: &str) {
        // The old pipeline must release the device before the new one opens it.
        self.output.stop();
        match Output::new(&self.device, &self.config.output) {
            Ok(output) => {
                self.output = output;
                self.watchdog.note_recovery_ok(Instant::now());
                tracing::info!("output pipeline recovered");
                self.status.message = Some(format!("recovering: {reason}"));
            }
            Err(error) => {
                self.watchdog.note_recovery_failed();
                tracing::warn!(%error, "output pipeline recovery failed");
                self.status.message = Some(format!("recovery failed: {error}"));
            }
        }
        self.emit_status();
    }

    fn poll_decoder_events(&mut self) {
        let mut events = Vec::new();
        if let Some(decoder) = &self.decoder {
            while let Some(event) = decoder.poll_event() {
                events.push(event);
            }
        }

        for event in events {
            match event {
                DecoderEvent::Eos => {
                    let loop_video = self
                        .decoder
                        .as_ref()
                        .map(|decoder| decoder.loop_video())
                        .unwrap_or(false);
                    if loop_video {
                        if let Some(decoder) = &self.decoder {
                            if let Err(error) = decoder.seek(0) {
                                self.fail(error);
                            } else if let Err(error) = decoder.play() {
                                self.fail(error);
                            }
                        }
                    } else {
                        if let Some(decoder) = self.decoder.take() {
                            decoder.stop();
                        }
                        self.status.state = PlaybackState::Idle;
                        self.status.source = None;
                        self.status.position_ms = 0;
                        self.status.duration_ms = 0;
                        self.status.message = Some("playback finished".to_string());
                        self.last_frame = self.black_frame.clone();
                        self.emit_status();
                    }
                }
                DecoderEvent::Error(error) => self.fail(error),
            }
        }
    }

    fn fail(&mut self, error: impl Into<String>) {
        if let Some(decoder) = self.decoder.take() {
            decoder.stop();
        }
        self.seek_preview = None;
        self.status.state = PlaybackState::Error;
        self.status.source = None;
        self.status.position_ms = 0;
        self.status.duration_ms = 0;
        self.status.message = Some(error.into());
        self.last_frame = self.black_frame.clone();
        self.emit_status();
    }

    fn merged_output(&self, update: OutputUpdate) -> Result<OutputConfig, String> {
        let mut output = self.config.output.clone();
        if let Some(width) = update.width {
            output.width = width;
        }
        if let Some(height) = update.height {
            output.height = height;
        }
        if let Some(fps) = update.fps {
            output.fps = fps;
        }
        if let Some(pixel_format) = update.pixel_format {
            output.pixel_format = PixelFormat::from_str(&pixel_format)?;
        }
        if let Some(fit) = update.fit {
            output.fit = FitMode::from_str(&fit)?;
        }
        if let Some(mirror) = update.mirror {
            output.mirror = mirror;
        }
        if let Some(rotate) = update.rotate {
            output.rotate = rotate;
        }
        output.validate()?;
        Ok(output)
    }

    fn apply_output(&mut self, new_output: OutputConfig, force: bool) -> Result<(), String> {
        if new_output == self.config.output {
            self.pending_output = None;
            self.status.pending = None;
            return Ok(());
        }

        // Changing the caps while applications are streaming the device makes
        // v4l2loopback wedge (new readers then fail with EBUSY). Defer the
        // change until the camera is free, unless the caller insists.
        if !force && caps_changed(&self.config.output, &new_output) && !self.consumers.is_empty() {
            let reason = format!(
                "waiting for {} to release the camera before changing the output format",
                self.consumer_summary()
            );
            self.pending_output = Some(new_output);
            self.status.pending = Some(reason.clone());
            self.status.message = Some(reason);
            self.emit_status();
            return Ok(());
        }

        self.pending_output = None;
        self.status.pending = None;

        let source = self.status.source.clone();
        let position = self.status.position_ms;
        let was_playing = self.status.state == PlaybackState::Playing;
        let was_paused = self.status.state == PlaybackState::Paused;

        // Options such as fitting, mirroring and rotation only affect how the
        // decoder produces frames; the caps negotiated with the device (and
        // therefore the output pipeline) stay untouched. Keeping the device
        // open avoids disturbing applications that are already streaming.
        let caps_same = !caps_changed(&self.config.output, &new_output);

        if let Some(decoder) = self.decoder.take() {
            decoder.stop();
        }
        self.seek_preview = None;

        if !caps_same {
            self.output.stop();

            let old_output = self.config.output.clone();
            match Output::new(&self.device, &new_output) {
                Ok(output) => {
                    self.output = output;
                }
                Err(error) => {
                    self.output = Output::new(&self.device, &old_output).map_err(|restore| {
                        format!(
                            "{error}; additionally failed to restore the previous output: {restore}"
                        )
                    })?;
                    return Err(error);
                }
            }

            self.black_frame = frame::black_frame(&new_output);
            self.last_frame = self.black_frame.clone();
        }

        self.config.output = new_output.clone();
        self.status.output = new_output.spec();
        self.status.position_ms = position;
        self.status.duration_ms = 0;

        if let Some(source) = source {
            let path = PathBuf::from(source);
            let loop_video = self.config.source.loop_video;
            match Decoder::new(&new_output, &path, loop_video, self.config.source.hw_decode) {
                Ok(decoder) => {
                    if was_paused {
                        decoder.pause()?;
                    } else if was_playing {
                        decoder.play()?;
                    }
                    let _ = decoder.seek(position);
                    self.decoder = Some(decoder);
                    self.status.state = if was_paused {
                        PlaybackState::Paused
                    } else {
                        PlaybackState::Playing
                    };
                    self.status.source = Some(path.display().to_string());
                    self.status.message = None;
                }
                Err(error) => {
                    self.status.state = PlaybackState::Error;
                    self.status.source = None;
                    self.status.message = Some(error);
                }
            }
        } else {
            self.status.state = PlaybackState::Idle;
        }

        self.emit_status();
        Ok(())
    }

    fn reload(&mut self) -> Result<Value, String> {
        let new_config = Config::load(&self.config_path).map_err(|error| error.to_string())?;
        new_config.validate()?;
        if new_config.device.device_path() != self.device {
            return Err("changing the device requires restarting vcamd".to_string());
        }

        self.config.source = new_config.source;
        self.config.daemon = new_config.daemon;
        self.config.watchdog = new_config.watchdog;
        self.config.profiles = new_config.profiles;
        self.base_output = new_config.output;
        self.auto_profile = self.config.daemon.auto_profiles;
        self.watchdog = Watchdog::new(&self.config.watchdog, Instant::now());

        // Re-resolve the output: a matching profile when auto mode is on,
        // otherwise the configured base output.
        if self.auto_profile && !self.config.profiles.is_empty() {
            self.reconcile_profile();
        } else {
            self.active_profile = None;
            self.status.profile = None;
            if self.base_output != self.config.output {
                self.apply_output(self.base_output.clone(), false)?;
            }
        }

        if self.status.state == PlaybackState::Idle && self.config.source.autostart {
            if let Some(path) = self.config.source.path.clone() {
                let loop_video = self.config.source.loop_video;
                self.start_source(&path, loop_video)?;
            }
        }

        self.status.loop_video = self.config.source.loop_video;
        self.emit_status();
        self.status_value()
    }

    fn refresh_position(&mut self) {
        if let Some(decoder) = &self.decoder {
            let duration = decoder.duration_ms();
            if duration > 0 {
                self.status.duration_ms = duration;
            }
        }
    }

    fn status_value(&self) -> Result<Value, String> {
        serde_json::to_value(&self.status).map_err(|error| error.to_string())
    }

    fn emit_status(&self) {
        let _ = self.events.send(Event::status(&self.status));
    }
}

/// Whether two outputs differ in the caps negotiated with the device. Fitting,
/// mirroring and rotation only affect decoding and can change without touching
/// the device caps.
fn caps_changed(a: &OutputConfig, b: &OutputConfig) -> bool {
    a.width != b.width || a.height != b.height || a.fps != b.fps || a.pixel_format != b.pixel_format
}
