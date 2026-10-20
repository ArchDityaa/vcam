use std::path::Path;
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use vcam_core::{FitMode, OutputConfig};

use crate::audio::AudioSpec;

#[derive(Debug, Clone)]
pub enum DecoderEvent {
    Eos,
    Error(String),
}

/// A decode pipeline that produces tightly packed frames in the configured
/// output format, exposed through an `appsink`.
pub struct Decoder {
    pipeline: gst::Pipeline,
    appsink: gst_app::AppSink,
    loop_video: bool,
    /// Audio branch, created on the first decoded audio pad.
    audio: Arc<Mutex<Option<AudioChain>>>,
}

/// The elements of the virtual microphone branch. Only the volume is needed
/// after construction, to mute the short playback used for paused seeks.
struct AudioChain {
    convert: gst::Element,
    volume: gst::Element,
}

impl Decoder {
    pub fn new(
        config: &OutputConfig,
        path: &Path,
        loop_video: bool,
        hw_decode: bool,
        audio: Option<&AudioSpec>,
    ) -> Result<Self, String> {
        if !path.is_file() {
            return Err(format!("video file does not exist: {}", path.display()));
        }

        let pipeline = gst::Pipeline::new();
        let filesrc = gst::ElementFactory::make("filesrc")
            .name("vcam-filesrc")
            .property("location", path.to_string_lossy().as_ref())
            .build()
            .map_err(|error| format!("failed to create filesrc: {error}"))?;
        let decodebin = gst::ElementFactory::make("decodebin3")
            .name("vcam-decodebin")
            .build()
            .map_err(|error| format!("failed to create decodebin3: {error}"))?;

        let convert = gst::ElementFactory::make("videoconvert")
            .name("vcam-convert")
            .build()
            .map_err(|error| format!("failed to create videoconvert: {error}"))?;

        let mut chain = vec![convert.clone()];

        if config.fit == FitMode::Fill {
            let crop = gst::ElementFactory::make("aspectratiocrop")
                .name("vcam-crop")
                .property(
                    "aspect-ratio",
                    gst::Fraction::new(config.width as i32, config.height as i32),
                )
                .build()
                .map_err(|error| format!("failed to create aspectratiocrop: {error}"))?;
            chain.push(crop);
        }

        let scale = gst::ElementFactory::make("videoscale")
            .name("vcam-scale")
            .property("add-borders", config.fit == FitMode::Letterbox)
            .build()
            .map_err(|error| format!("failed to create videoscale: {error}"))?;
        chain.push(scale);

        if config.rotate != 0 {
            let method = match config.rotate {
                90 => "clockwise",
                180 => "rotate-180",
                270 => "counterclockwise",
                _ => "none",
            };
            let flip = gst::ElementFactory::make("videoflip")
                .name("vcam-rotate")
                .property("method", method)
                .build()
                .map_err(|error| format!("failed to create videoflip: {error}"))?;
            chain.push(flip);
        }

        if config.mirror {
            let flip = gst::ElementFactory::make("videoflip")
                .name("vcam-mirror")
                .property("method", "horizontal-flip")
                .build()
                .map_err(|error| format!("failed to create videoflip: {error}"))?;
            chain.push(flip);
        }

        let rate = gst::ElementFactory::make("videorate")
            .name("vcam-rate")
            .build()
            .map_err(|error| format!("failed to create videorate: {error}"))?;
        chain.push(rate);

        let caps = gst::Caps::builder("video/x-raw")
            .field("format", config.pixel_format.gst_name())
            .field("width", config.width as i32)
            .field("height", config.height as i32)
            .field("framerate", gst::Fraction::new(config.fps as i32, 1))
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name("vcam-caps")
            .property("caps", &caps)
            .build()
            .map_err(|error| format!("failed to create capsfilter: {error}"))?;
        chain.push(capsfilter);

        // Keep the decoder paced by the pipeline clock. Without sync the
        // decoder runs as fast as the file allows while the controller only
        // drains the newest frame, which makes playback run several times too
        // fast. `try_pull_sample` stays non-blocking; samples simply become
        // available when their presentation time is due.
        let appsink_element = gst::ElementFactory::make("appsink")
            .name("vcam-appsink")
            .property("sync", true)
            .property("max-buffers", 2u32)
            .property("drop", true)
            .property("emit-signals", false)
            .build()
            .map_err(|error| format!("failed to create appsink: {error}"))?;
        let appsink = appsink_element
            .clone()
            .downcast::<gst_app::AppSink>()
            .map_err(|_| "element is not an appsink".to_string())?;
        chain.push(appsink_element);

        pipeline
            .add(&filesrc)
            .map_err(|error| format!("failed to add filesrc: {error}"))?;
        pipeline
            .add(&decodebin)
            .map_err(|error| format!("failed to add decodebin3: {error}"))?;
        for element in &chain {
            pipeline
                .add(element)
                .map_err(|error| format!("failed to add {}: {error}", element.name()))?;
        }

        filesrc
            .link(&decodebin)
            .map_err(|error| format!("failed to link filesrc to decodebin3: {error}"))?;
        for pair in chain.windows(2) {
            pair[0]
                .link(&pair[1])
                .map_err(|error| format!("failed to link decode chain: {error}"))?;
        }

        // decodebin3 exposes decoded pads dynamically. Video pads feed the
        // output chain; the first audio pad creates the microphone branch.
        //
        // The branch is built lazily on purpose: a sink that never receives a
        // buffer would also never receive EOS, so a video-only file would make
        // the pipeline wait forever and looping would stop working.
        let convert_weak = convert.downgrade();
        let pipeline_weak = pipeline.downgrade();
        let audio_state = Arc::new(Mutex::new(None::<AudioChain>));
        let audio_state_for_pads = audio_state.clone();
        let audio_spec = audio.cloned();
        decodebin.connect_pad_added(move |_decodebin, source_pad| {
            let caps = source_pad
                .current_caps()
                .unwrap_or_else(|| source_pad.query_caps(None));
            let name = caps
                .structure(0)
                .map(|structure| structure.name().to_string())
                .unwrap_or_default();
            if name.is_empty() || name.starts_with("video/") {
                // A pad without caps is treated as video, matching the previous
                // behaviour; decodebin3 normally supplies caps in `pad-added`.
                let Some(convert) = convert_weak.upgrade() else {
                    return;
                };
                link_video_pad(&convert, source_pad);
            } else if name.starts_with("audio/") {
                let Some(spec) = audio_spec.as_ref() else {
                    return;
                };
                let Some(pipeline) = pipeline_weak.upgrade() else {
                    return;
                };
                link_audio_pad(&pipeline, source_pad, spec, &audio_state_for_pads);
            }
        });

        if hw_decode {
            tracing::debug!("hardware decoding will be selected by decodebin3 when available");
        }

        Ok(Self {
            pipeline,
            appsink,
            loop_video,
            audio: audio_state,
        })
    }

    pub fn loop_video(&self) -> bool {
        self.loop_video
    }

    /// Mute or unmute the microphone branch. Used while a paused seek runs the
    /// pipeline briefly to produce a frame, so no audio snippet leaks out.
    pub fn set_audio_muted(&self, muted: bool) {
        let Ok(state) = self.audio.lock() else {
            return;
        };
        if let Some(chain) = state.as_ref() {
            chain
                .volume
                .set_property("volume", if muted { 0.0 } else { 1.0 });
        }
    }

    /// Whether the source actually provided an audio pad.
    pub fn has_audio(&self) -> bool {
        self.audio
            .lock()
            .map(|state| state.is_some())
            .unwrap_or(false)
    }

    pub fn set_loop(&mut self, loop_video: bool) {
        self.loop_video = loop_video;
    }

    pub fn play(&self) -> Result<(), String> {
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| format!("failed to start playback: {error}"))?;
        let (state_change, _current, _pending) =
            self.pipeline.state(gst::ClockTime::from_seconds(5));
        if state_change.is_err() {
            return Err(self
                .take_bus_error()
                .unwrap_or_else(|| "playback pipeline failed to start".to_string()));
        }
        Ok(())
    }

    pub fn pause(&self) -> Result<(), String> {
        self.pipeline
            .set_state(gst::State::Paused)
            .map_err(|error| format!("failed to pause playback: {error}"))?;
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    pub fn seek(&self, position_ms: u64) -> Result<(), String> {
        // ACCURATE rather than KEY_UNIT: many camera/phone clips only have a
        // keyframe at the beginning, which would make a KEY_UNIT seek snap
        // back to zero. Accurate seeks decode from the preceding keyframe.
        self.pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::from_mseconds(position_ms),
            )
            .map_err(|error| format!("failed to seek: {error}"))
    }

    pub fn duration_ms(&self) -> u64 {
        self.pipeline
            .query_duration::<gst::ClockTime>()
            .map(|duration| duration.mseconds())
            .unwrap_or(0)
    }

    /// Drain the appsink and return only the newest frame together with its
    /// presentation timestamp. Dropping older frames keeps latency low when
    /// decoding runs slightly ahead of real time.
    pub fn pull_latest(&self) -> Option<(Vec<u8>, Option<gst::ClockTime>)> {
        let mut latest = None;
        while let Some(sample) = self.appsink.try_pull_sample(gst::ClockTime::ZERO) {
            if let Some(buffer) = sample.buffer() {
                let pts = buffer.pts();
                if let Ok(map) = buffer.map_readable() {
                    latest = Some((map.as_slice().to_vec(), pts));
                }
            }
        }
        latest
    }

    pub fn poll_event(&self) -> Option<DecoderEvent> {
        let bus = self.pipeline.bus()?;
        let message = bus.timed_pop_filtered(
            gst::ClockTime::ZERO,
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )?;
        match message.view() {
            gst::MessageView::Eos(..) => Some(DecoderEvent::Eos),
            gst::MessageView::Error(error) => Some(DecoderEvent::Error(format!(
                "{} ({})",
                error.error(),
                error
                    .debug()
                    .map(|debug| debug.to_string())
                    .unwrap_or_else(|| "no debug information".to_string())
            ))),
            _ => None,
        }
    }

    fn take_bus_error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        let message = bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])?;
        match message.view() {
            gst::MessageView::Error(error) => Some(format!(
                "{} ({})",
                error.error(),
                error
                    .debug()
                    .map(|debug| debug.to_string())
                    .unwrap_or_else(|| "no debug information".to_string())
            )),
            _ => None,
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn make_element(factory: &str, name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|error| format!("failed to create {factory}: {error}"))
}

fn link_video_pad(convert: &gst::Element, source_pad: &gst::Pad) {
    let Some(sink_pad) = convert.static_pad("sink") else {
        return;
    };
    if sink_pad.is_linked() {
        return;
    }
    if source_pad.link(&sink_pad).is_err() {
        tracing::warn!(
            pad = source_pad.name().as_str(),
            "failed to link decoded video pad"
        );
    }
}

fn link_audio_pad(
    pipeline: &gst::Pipeline,
    source_pad: &gst::Pad,
    spec: &AudioSpec,
    state: &Mutex<Option<AudioChain>>,
) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    if let Some(chain) = state.as_ref() {
        // decodebin3 can expose a new audio pad after a seek; attach it to the
        // branch that already exists instead of creating a second one.
        link_audio_source(&chain.convert, source_pad);
        return;
    }
    match build_audio_chain(pipeline, spec) {
        Ok(chain) => {
            link_audio_source(&chain.convert, source_pad);
            *state = Some(chain);
            tracing::info!(
                sink = %spec.sink,
                rate = spec.sample_rate,
                channels = spec.channels,
                "virtual microphone branch attached"
            );
        }
        Err(error) => {
            tracing::warn!(%error, "failed to build the audio branch; continuing without audio");
        }
    }
}

fn link_audio_source(convert: &gst::Element, source_pad: &gst::Pad) {
    let Some(sink_pad) = convert.static_pad("sink") else {
        return;
    };
    if sink_pad.is_linked() {
        return;
    }
    if source_pad.link(&sink_pad).is_err() {
        tracing::warn!(
            pad = source_pad.name().as_str(),
            "failed to link decoded audio pad"
        );
    }
}

fn build_audio_chain(pipeline: &gst::Pipeline, spec: &AudioSpec) -> Result<AudioChain, String> {
    let convert = make_element("audioconvert", "vcam-audio-convert")?;
    let resample = make_element("audioresample", "vcam-audio-resample")?;

    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", spec.sample_rate as i32)
        .field("channels", spec.channels as i32)
        .field("layout", "interleaved")
        .build();
    let capsfilter = make_element("capsfilter", "vcam-audio-caps")?;
    capsfilter.set_property("caps", &caps);

    let queue = make_element("queue", "vcam-audio-queue")?;
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 500_000_000u64);

    let volume = make_element("volume", "vcam-audio-volume")?;
    volume.set_property("volume", 1.0f64);

    let sink = make_element("pulsesink", "vcam-audio-sink")?;
    sink.set_property("device", spec.sink.as_str());
    sink.set_property("client-name", "vcam");
    sink.set_property("sync", true);
    // The camera pipeline keeps using the system clock; letting the audio sink
    // provide the pipeline clock would change how decoded frames are paced.
    sink.set_property("provide-clock", false);
    // Never make the whole pipeline wait for the audio device.
    sink.set_property("async", false);

    let chain = [&convert, &resample, &capsfilter, &queue, &volume, &sink];
    for element in chain {
        pipeline
            .add(element)
            .map_err(|error| format!("failed to add {}: {error}", element.name()))?;
    }
    for pair in chain.windows(2) {
        pair[0]
            .link(pair[1])
            .map_err(|error| format!("failed to link the audio chain: {error}"))?;
    }
    for element in chain {
        element
            .sync_state_with_parent()
            .map_err(|error| format!("failed to start {}: {error}", element.name()))?;
    }

    Ok(AudioChain { convert, volume })
}
