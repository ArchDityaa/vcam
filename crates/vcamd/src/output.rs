use std::path::Path;

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use vcam_core::OutputConfig;

/// Persistent appsrc -> v4l2sink pipeline.
///
/// The daemon paces frames itself and pushes them into `appsrc`, which keeps
/// the loopback device open even while the source is being changed or paused.
pub struct Output {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    frame_size: usize,
    frame_ns: u64,
    pts: gst::ClockTime,
}

impl Output {
    pub fn new(device: &Path, config: &OutputConfig) -> Result<Self, String> {
        let frame_size = config.frame_size();
        let frame_ns = config.frame_duration_ns();
        let pipeline = gst::Pipeline::new();

        let appsrc_element = gst::ElementFactory::make("appsrc")
            .name("vcam-source")
            .property("is-live", true)
            .property("format", gst::Format::Time)
            .property("do-timestamp", false)
            // Never block the controller thread: if the device stops draining,
            // drop the newest frames instead of growing an unbounded queue.
            // The watchdog reacts to the stall separately.
            .property("block", false)
            .property_from_str("leaky-type", "downstream")
            .property("max-bytes", frame_size as u64 * 2)
            .build()
            .map_err(|error| format!("failed to create appsrc: {error}"))?;
        let appsrc = appsrc_element
            .clone()
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| "element is not an appsrc".to_string())?;

        // v4l2sink's device caps always constrain pixel-aspect-ratio,
        // interlace-mode and colorimetry. A caps structure that leaves those
        // fields open is broader than the sink caps, so accept-caps rejects
        // it and appsrc negotiation fails with not-negotiated. Spell out the
        // fields explicitly to keep the caps a strict subset.
        let colorimetry = if config.height >= 720 {
            "bt709"
        } else {
            "bt601"
        };
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", config.pixel_format.gst_name())
            .field("width", config.width as i32)
            .field("height", config.height as i32)
            .field("framerate", gst::Fraction::new(config.fps as i32, 1))
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .field("interlace-mode", "progressive")
            .field("colorimetry", colorimetry)
            .build();
        appsrc.set_property("caps", &caps);

        let device_string = device.to_string_lossy().to_string();
        let sink = gst::ElementFactory::make("v4l2sink")
            .name("vcam-sink")
            .property("device", &device_string)
            .property("sync", false)
            .property("async", false)
            .build()
            .map_err(|error| format!("failed to create v4l2sink: {error}"))?;

        let queue = gst::ElementFactory::make("queue")
            .name("vcam-queue")
            .property("max-size-buffers", 2u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            // Leak old frames rather than applying backpressure upstream.
            .property_from_str("leaky", "downstream")
            .build()
            .map_err(|error| format!("failed to create queue: {error}"))?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name("vcam-output-caps")
            .property("caps", &caps)
            .build()
            .map_err(|error| format!("failed to create output capsfilter: {error}"))?;

        pipeline
            .add(&appsrc)
            .map_err(|error| format!("failed to add appsrc: {error}"))?;
        pipeline
            .add(&capsfilter)
            .map_err(|error| format!("failed to add output capsfilter: {error}"))?;
        pipeline
            .add(&queue)
            .map_err(|error| format!("failed to add queue: {error}"))?;
        pipeline
            .add(&sink)
            .map_err(|error| format!("failed to add v4l2sink: {error}"))?;
        appsrc
            .link(&capsfilter)
            .map_err(|error| format!("failed to link appsrc to capsfilter: {error}"))?;
        capsfilter
            .link(&queue)
            .map_err(|error| format!("failed to link capsfilter to queue: {error}"))?;
        queue
            .link(&sink)
            .map_err(|error| format!("failed to link queue to v4l2sink: {error}"))?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| format!("failed to start virtual camera pipeline: {error}"))?;
        let (state_change, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(5));
        if state_change.is_err() {
            let detail = pipeline
                .bus()
                .and_then(|bus| {
                    bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])
                })
                .and_then(|message| match message.view() {
                    gst::MessageView::Error(error) => Some(format!(
                        "{} ({})",
                        error.error(),
                        error
                            .debug()
                            .map(|debug| debug.to_string())
                            .unwrap_or_else(|| "no debug information".to_string())
                    )),
                    _ => None,
                })
                .unwrap_or_else(|| "unknown pipeline error".to_string());
            let _ = pipeline.set_state(gst::State::Null);
            return Err(format!(
                "cannot open virtual camera {device_string}: {detail}. \
                 Is another producer already using the device?"
            ));
        }

        Ok(Self {
            pipeline,
            appsrc,
            frame_size,
            frame_ns,
            pts: gst::ClockTime::ZERO,
        })
    }

    pub fn push(&mut self, frame: &[u8]) -> Result<(), String> {
        if frame.len() != self.frame_size {
            return Err(format!(
                "frame size mismatch: expected {} bytes, got {}",
                self.frame_size,
                frame.len()
            ));
        }
        let mut buffer = gst::Buffer::from_slice(frame.to_vec());
        buffer
            .get_mut()
            .ok_or_else(|| "failed to obtain a writable frame buffer".to_string())?
            .set_pts(Some(self.pts));
        self.pts += gst::ClockTime::from_nseconds(self.frame_ns);
        self.appsrc
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|error| format!("failed to push frame to virtual camera: {error:?}"))
    }

    /// Bytes currently queued inside `appsrc`. A value pinned at the maximum
    /// while frames keep being pushed means the device stopped draining.
    pub fn level_bytes(&self) -> u64 {
        self.appsrc.current_level_bytes()
    }

    /// Drain the pipeline bus for a fatal error, e.g. the device disappearing.
    pub fn poll_error(&self) -> Option<String> {
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

    pub fn stop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop();
    }
}
