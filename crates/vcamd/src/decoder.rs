use std::path::Path;

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use vcam_core::{FitMode, OutputConfig};

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
}

impl Decoder {
    pub fn new(
        config: &OutputConfig,
        path: &Path,
        loop_video: bool,
        hw_decode: bool,
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

        // decodebin3 exposes decoded pads dynamically. Connect the first video
        // pad to videoconvert and ignore audio/subtitle pads.
        let convert_weak = convert.downgrade();
        decodebin.connect_pad_added(move |_decodebin, source_pad| {
            let Some(convert) = convert_weak.upgrade() else {
                return;
            };
            let caps = source_pad
                .current_caps()
                .unwrap_or_else(|| source_pad.query_caps(None));
            let is_video = caps
                .structure(0)
                .map(|structure| structure.name().as_str().starts_with("video/"))
                .unwrap_or(true);
            if !is_video {
                return;
            }
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
        });

        if hw_decode {
            tracing::debug!("hardware decoding will be selected by decodebin3 when available");
        }

        Ok(Self {
            pipeline,
            appsink,
            loop_video,
        })
    }

    pub fn loop_video(&self) -> bool {
        self.loop_video
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
