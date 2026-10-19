//! Preview frames shared from the daemon to GUI clients.
//!
//! The GUI cannot reliably open a second reader on the loopback device: while
//! another application (Discord, Chrome, ...) is already streaming it,
//! v4l2loopback rejects `VIDIOC_REQBUFS` with `EBUSY`, so a reader that starts
//! after those applications never attaches. The daemon already owns the
//! device, so it renders a small RGB preview and publishes it atomically in the
//! runtime directory next to the control socket; the GUI only reads that file.
//! The preview therefore works no matter which applications are using the
//! camera.

use std::path::{Path, PathBuf};

use crate::config::{OutputConfig, PixelFormat};

/// Magic prefix identifying a preview file.
pub const PREVIEW_MAGIC: [u8; 8] = *b"VCAMPREV";
/// Layout of the header: magic (8 bytes) + width (u16 LE) + height (u16 LE).
pub const PREVIEW_HEADER_LEN: usize = 12;
/// Longest edge of the rendered preview.
pub const PREVIEW_MAX_WIDTH: u32 = 640;

/// Path of the preview file that lives next to the control socket.
pub fn preview_path(socket_path: &Path) -> PathBuf {
    socket_path.with_file_name("vcam-preview.raw")
}

/// Preview dimensions for an output configuration, preserving aspect ratio.
pub fn preview_size(output: &OutputConfig) -> (u32, u32) {
    let width = PREVIEW_MAX_WIDTH.min(output.width.max(1));
    let height = ((width as u64 * output.height as u64) / output.width.max(1) as u64).max(1) as u32;
    (width, height)
}

/// Downscale one output frame and encode it as a preview blob (header + RGB8).
pub fn render(frame: &[u8], output: &OutputConfig, width: u32, height: u32) -> Vec<u8> {
    let width = width.max(1);
    let height = height.max(1);
    let mut out = vec![0u8; PREVIEW_HEADER_LEN + width as usize * height as usize * 3];
    out[..8].copy_from_slice(&PREVIEW_MAGIC);
    out[8..10].copy_from_slice(&(width as u16).to_le_bytes());
    out[10..12].copy_from_slice(&(height as u16).to_le_bytes());

    let rgb = &mut out[PREVIEW_HEADER_LEN..];
    let src_w = output.width.max(1);
    let src_h = output.height.max(1);

    match output.pixel_format {
        PixelFormat::Yuyv => {
            let stride = src_w as usize * 2;
            for y in 0..height {
                let sy = (y as u64 * src_h as u64 / height as u64) as usize;
                let row = sy * stride;
                for x in 0..width {
                    let sx = (x as u64 * src_w as u64 / width as u64) as usize;
                    let pair = row + (sx / 2) * 4;
                    let luma = if sx % 2 == 0 { pair } else { pair + 2 };
                    let luma = frame.get(luma).copied().unwrap_or(16) as i32;
                    let u = frame.get(pair + 1).copied().unwrap_or(128) as i32;
                    let v = frame.get(pair + 3).copied().unwrap_or(128) as i32;
                    let (r, g, b) = yuv_to_rgb(luma, u, v);
                    let offset = (y as usize * width as usize + x as usize) * 3;
                    rgb[offset] = r;
                    rgb[offset + 1] = g;
                    rgb[offset + 2] = b;
                }
            }
        }
        PixelFormat::Nv12 => {
            let y_stride = src_w as usize;
            let uv_plane = y_stride * src_h as usize;
            for y in 0..height {
                let sy = (y as u64 * src_h as u64 / height as u64) as usize;
                for x in 0..width {
                    let sx = (x as u64 * src_w as u64 / width as u64) as usize;
                    let luma = frame.get(sy * y_stride + sx).copied().unwrap_or(16) as i32;
                    let uv = uv_plane + (sy / 2) * y_stride + (sx / 2) * 2;
                    let u = frame.get(uv).copied().unwrap_or(128) as i32;
                    let v = frame.get(uv + 1).copied().unwrap_or(128) as i32;
                    let (r, g, b) = yuv_to_rgb(luma, u, v);
                    let offset = (y as usize * width as usize + x as usize) * 3;
                    rgb[offset] = r;
                    rgb[offset + 1] = g;
                    rgb[offset + 2] = b;
                }
            }
        }
    }

    out
}

/// BT.601 studio-swing YUV to full-range RGB.
fn yuv_to_rgb(y: i32, u: i32, v: i32) -> (u8, u8, u8) {
    let c = (y - 16).max(0);
    let d = u - 128;
    let e = v - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    (clamp(r), clamp(g), clamp(b))
}

fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_size_preserves_aspect() {
        let wide = OutputConfig {
            width: 1280,
            height: 720,
            ..OutputConfig::default()
        };
        assert_eq!(preview_size(&wide), (640, 360));

        let small = OutputConfig {
            width: 640,
            height: 480,
            ..OutputConfig::default()
        };
        assert_eq!(preview_size(&small), (640, 480));

        let tiny = OutputConfig {
            width: 160,
            height: 120,
            ..OutputConfig::default()
        };
        assert_eq!(preview_size(&tiny), (160, 120));
    }

    #[test]
    fn render_yuyv_has_header_and_rgb() {
        let output = OutputConfig {
            width: 4,
            height: 2,
            pixel_format: PixelFormat::Yuyv,
            ..OutputConfig::default()
        };
        let frame = vec![0u8; output.frame_size()];
        let blob = render(&frame, &output, 2, 1);
        assert_eq!(&blob[..8], &PREVIEW_MAGIC);
        assert_eq!(u16::from_le_bytes([blob[8], blob[9]]), 2);
        assert_eq!(u16::from_le_bytes([blob[10], blob[11]]), 1);
        // 2x1 RGB8 pixels after the header.
        assert_eq!(blob.len(), PREVIEW_HEADER_LEN + 6);
    }

    #[test]
    fn render_nv12_has_header_and_rgb() {
        let output = OutputConfig {
            width: 4,
            height: 2,
            pixel_format: PixelFormat::Nv12,
            ..OutputConfig::default()
        };
        let frame = vec![0u8; output.frame_size()];
        let blob = render(&frame, &output, 2, 1);
        assert_eq!(&blob[..8], &PREVIEW_MAGIC);
        assert_eq!(blob.len(), PREVIEW_HEADER_LEN + 6);
    }
}
