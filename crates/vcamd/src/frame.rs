use vcam_core::{OutputConfig, PixelFormat};

/// A black frame in the configured raw format.
///
/// NV12 uses limited-range black (Y=16, UV=128); YUY2 uses the same luma and
/// neutral chroma so it looks identical on every application.
pub fn black_frame(config: &OutputConfig) -> Vec<u8> {
    let width = config.width as usize;
    let height = config.height as usize;
    match config.pixel_format {
        PixelFormat::Nv12 => {
            let luma = width * height;
            let mut frame = vec![16u8; luma + luma / 2];
            for byte in &mut frame[luma..] {
                *byte = 128;
            }
            frame
        }
        PixelFormat::Yuyv => {
            let mut frame = vec![16u8; width * height * 2];
            for pixel in frame.chunks_exact_mut(4) {
                pixel[0] = 16;
                pixel[1] = 128;
                pixel[2] = 16;
                pixel[3] = 128;
            }
            frame
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_size_and_neutral_chroma() {
        let cfg = OutputConfig {
            width: 64,
            height: 48,
            ..OutputConfig::default()
        };
        let frame = black_frame(&cfg);
        assert_eq!(frame.len(), 64 * 48 * 3 / 2);
        assert!(frame[..64 * 48].iter().all(|&v| v == 16));
        assert!(frame[64 * 48..].iter().all(|&v| v == 128));
    }

    #[test]
    fn yuy2_size_and_pattern() {
        let cfg = OutputConfig {
            width: 64,
            height: 48,
            pixel_format: PixelFormat::Yuyv,
            ..OutputConfig::default()
        };
        let frame = black_frame(&cfg);
        assert_eq!(frame.len(), 64 * 48 * 2);
        assert_eq!(&frame[..4], &[16, 128, 16, 128]);
    }
}
