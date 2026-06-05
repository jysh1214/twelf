use eframe::egui::{self, ColorImage};
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageError};
use std::io::Cursor;
use std::time::{Duration, Instant};

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

/// Some encoders emit zero-delay frames; clamp them so playback neither
/// stalls on a single frame nor busy-spins the UI thread.
const MIN_FRAME_DELAY: Duration = Duration::from_millis(20);

/// A playing multi-frame WebP, advancing by wall-clock time and looping.
pub struct Animation {
    pub uri: String,
    frames: Vec<WebpFrame>,
    start: Instant,
    total: Duration,
    texture: Option<egui::TextureHandle>,
    shown: usize,
}

impl Animation {
    pub fn new(uri: String, mut frames: Vec<WebpFrame>) -> Self {
        for frame in &mut frames {
            frame.delay = frame.delay.max(MIN_FRAME_DELAY);
        }
        let total = frames.iter().map(|f| f.delay).sum();
        Self {
            uri,
            frames,
            start: Instant::now(),
            total,
            texture: None,
            shown: 0,
        }
    }

    /// Upload the frame for the current instant (reusing the texture unless the
    /// frame changed) and report how long until the next frame is due.
    pub fn frame(&mut self, ctx: &egui::Context) -> (egui::load::SizedTexture, Duration) {
        let (idx, remaining) = self.current_index();
        let options = egui::TextureOptions::LINEAR;
        match &mut self.texture {
            Some(handle) if self.shown == idx => {}
            Some(handle) => {
                handle.set(self.frames[idx].image.clone(), options);
                self.shown = idx;
            }
            None => {
                self.texture =
                    Some(ctx.load_texture(&self.uri, self.frames[idx].image.clone(), options));
                self.shown = idx;
            }
        }
        let handle = self.texture.as_ref().unwrap();
        (egui::load::SizedTexture::from_handle(handle), remaining)
    }

    fn current_index(&self) -> (usize, Duration) {
        let total = self.total.as_nanos();
        let elapsed = self.start.elapsed().as_nanos() % total;
        let mut acc = 0u128;
        for (i, frame) in self.frames.iter().enumerate() {
            let delay = frame.delay.as_nanos();
            if elapsed < acc + delay {
                return (i, Duration::from_nanos((acc + delay - elapsed) as u64));
            }
            acc += delay;
        }
        (self.frames.len() - 1, MIN_FRAME_DELAY)
    }
}
