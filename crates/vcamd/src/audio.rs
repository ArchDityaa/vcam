//! Virtual microphone companion.
//!
//! A PulseAudio/PipeWire null sink receives the source's audio track, and a
//! source remapped from that sink's monitor is what applications see as a
//! microphone. The bridge only manages the two session modules; the audio
//! itself is produced by the decoder pipeline.
//!
//! Every operation is best effort: a missing sound server must never take the
//! camera down, so failures are reported through `status().message` instead of
//! aborting the daemon.

use vcam_core::audio;
use vcam_core::{AudioConfig, AudioStatus};

/// Everything the decoder needs to feed the virtual microphone.
#[derive(Debug, Clone)]
pub struct AudioSpec {
    pub sink: String,
    pub sample_rate: u32,
    pub channels: u32,
}

pub struct AudioBridge {
    config: AudioConfig,
    sink_module: Option<u32>,
    source_module: Option<u32>,
    backend: String,
    available: bool,
    message: Option<String>,
}

impl AudioBridge {
    /// Probe the sound server and, when enabled, create the virtual microphone.
    pub fn new(config: &AudioConfig) -> Self {
        let mut bridge = Self {
            config: config.clone(),
            sink_module: None,
            source_module: None,
            backend: "unknown".to_string(),
            available: false,
            message: None,
        };
        bridge.probe();
        if config.enabled {
            bridge.enable();
        }
        bridge
    }

    fn probe(&mut self) {
        match audio::server_info() {
            Ok(info) => {
                self.backend = audio::parse_backend(&info);
                self.available = true;
                self.message = None;
            }
            Err(error) => {
                self.available = false;
                self.backend = "unknown".to_string();
                self.message = Some(error);
            }
        }
    }

    /// Create the sink and the remapped microphone. Existing modules with the
    /// configured names are adopted, so restarting the daemon (or recovering
    /// from a crash) never stacks `vcam_sink.2`, `vcam_sink.3`, ...
    pub fn enable(&mut self) {
        if !self.available {
            self.probe();
        }
        if !self.available {
            return;
        }
        match self.ensure_modules() {
            Ok(()) => {
                self.message = None;
                tracing::info!(
                    sink = %self.config.sink_name,
                    source = %self.config.source_name,
                    backend = %self.backend,
                    "virtual microphone ready"
                );
            }
            Err(error) => {
                tracing::warn!(%error, "failed to create the virtual microphone");
                self.message = Some(error);
            }
        }
    }

    fn ensure_modules(&mut self) -> Result<(), String> {
        let modules = audio::modules()?;
        self.sink_module = audio::find_module(
            &modules,
            "module-null-sink",
            "sink_name",
            &self.config.sink_name,
        )
        .map(|module| module.id);
        self.source_module = audio::find_module(
            &modules,
            "module-remap-source",
            "source_name",
            &self.config.source_name,
        )
        .map(|module| module.id);

        let loaded_sink = self.sink_module.is_none();
        if loaded_sink {
            let id = audio::load_module(
                "module-null-sink",
                &[
                    format!("sink_name={}", self.config.sink_name),
                    format!(
                        "sink_properties=device.description={}",
                        audio::escape_property_value(&self.config.label)
                    ),
                ],
            )?;
            self.sink_module = Some(id);
        }

        if self.source_module.is_none() {
            let result = audio::load_module(
                "module-remap-source",
                &[
                    format!("master={}.monitor", self.config.sink_name),
                    format!("source_name={}", self.config.source_name),
                    format!(
                        "source_properties=device.description={}",
                        audio::escape_property_value(&self.config.label)
                    ),
                ],
            );
            match result {
                Ok(id) => self.source_module = Some(id),
                Err(error) => {
                    // Do not leave an orphaned sink behind when the microphone
                    // itself could not be created.
                    if loaded_sink {
                        if let Some(id) = self.sink_module.take() {
                            let _ = audio::unload_module(id);
                        }
                    }
                    return Err(error);
                }
            }
        }

        Ok(())
    }

    /// Remove the microphone and the sink again. Safe to call repeatedly.
    pub fn disable(&mut self) {
        // The source must go first: unloading the sink first would leave the
        // remapped source pointing at a monitor that no longer exists.
        if let Some(id) = self.source_module.take() {
            if let Err(error) = audio::unload_module(id) {
                tracing::debug!(%error, module = id, "failed to unload audio source module");
            }
        }
        if let Some(id) = self.sink_module.take() {
            if let Err(error) = audio::unload_module(id) {
                tracing::debug!(%error, module = id, "failed to unload audio sink module");
            }
        }
    }

    /// Stop feeding audio after a pipeline failure, keeping the modules (and
    /// therefore the device list entry) intact where possible.
    pub fn fail(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    pub fn reconfigure(&mut self, config: &AudioConfig) {
        if &self.config == config {
            if config.enabled && !self.active() {
                self.enable();
            }
            return;
        }
        self.disable();
        self.config = config.clone();
        self.probe();
        if config.enabled {
            self.enable();
        }
    }

    /// The decoder configuration, or `None` when audio is not being produced.
    pub fn spec(&self) -> Option<AudioSpec> {
        self.source_module.map(|_| AudioSpec {
            sink: self.config.sink_name.clone(),
            sample_rate: self.config.sample_rate,
            channels: self.config.channels,
        })
    }

    pub fn active(&self) -> bool {
        self.source_module.is_some() && self.sink_module.is_some()
    }

    pub fn status(&self, enabled: bool, playing: bool) -> AudioStatus {
        AudioStatus {
            enabled,
            available: self.available,
            backend: self.backend.clone(),
            sink: self.config.sink_name.clone(),
            source: self.config.source_name.clone(),
            label: self.config.label.clone(),
            sample_rate: self.config.sample_rate,
            channels: self.config.channels,
            active: self.active(),
            playing: playing && self.source_module.is_some(),
            message: self.message.clone(),
        }
    }
}

impl Drop for AudioBridge {
    fn drop(&mut self) {
        self.disable();
    }
}

/// Whether a decoder error looks like it came from the audio branch rather
/// than from the video pipeline. `pulsesink` names the sink in most errors,
/// and the PulseAudio/PipeWire libraries add their own names to the rest.
pub fn is_audio_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("pulsesink")
        || error.contains("pulseaudio")
        || error.contains("pipewire")
        || (error.contains("audio") && error.contains("device"))
}
