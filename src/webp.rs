use eframe::egui::ColorImage;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageError};
use std::io::Cursor;
use std::time::Duration;

/// One decoded WebP frame and how long it stays on screen.
pub struct WebpFrame {
    pub image: ColorImage,
    pub delay: Duration,
}

pub fn is_webp(uri: &str) -> bool {
    uri.to_ascii_lowercase().ends_with(".webp")
}

/// Decode WebP bytes into an ordered frame sequence with per-frame delays.
/// A still WebP collapses to a single zero-delay frame, so callers can detect
/// animation by checking the frame count.
pub fn decode_frames(bytes: &[u8]) -> Result<Vec<WebpFrame>, ImageError> {
    let decoder = WebPDecoder::new(Cursor::new(bytes))?;
    if !decoder.has_animation() {
        let rgba = DynamicImage::from_decoder(decoder)?.to_rgba8();
        let (w, h) = rgba.dimensions();
        return Ok(vec![WebpFrame {
            image: to_color_image(w, h, rgba.as_raw()),
            delay: Duration::ZERO,
        }]);
    }
    decoder
        .into_frames()
        .collect_frames()?
        .into_iter()
        .map(|frame| {
            let delay: Duration = frame.delay().into();
            let rgba = frame.into_buffer();
            let (w, h) = rgba.dimensions();
            Ok(WebpFrame {
                image: to_color_image(w, h, rgba.as_raw()),
                delay,
            })
        })
        .collect()
}

fn to_color_image(width: u32, height: u32, rgba: &[u8]) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba)
}
