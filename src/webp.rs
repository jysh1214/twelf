use eframe::egui::{self, ColorImage};
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, ImageDecoder, ImageError};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One decoded WebP frame and how long it stays on screen. Shared, so handing a
/// frame to the GPU does not copy its pixels first.
pub struct WebpFrame {
    pub image: Arc<ColorImage>,
    pub delay: Duration,
}

pub fn is_webp(uri: &str) -> bool {
    uri.to_ascii_lowercase().ends_with(".webp")
}

/// The most decoded frames of one animation may occupy — as much as the whole
/// decoded-image cache. Every frame is held as RGBA for as long as the file is
/// selected, and nothing bounded that: 600 frames at 1280x720 is 2.2 GB, and a
/// crafted file on the server could ask for a gigabyte per frame.
pub const MAX_ANIMATION_BYTES: usize = 512 * 1024 * 1024;

/// The frames of an animated WebP, in order.
pub struct DecodedAnimation {
    pub frames: Vec<WebpFrame>,
    /// The file has more frames than fit in the budget; these are its first.
    pub truncated: bool,
}

/// Decode an animated WebP, holding at most `budget` bytes of frames. `None`
/// for a still image, decided from the header alone: the still path decodes
/// those, and used to do so a second time after this had decoded them once only
/// to find a single frame. Also `None` once `cancel` is set, which is checked
/// between frames.
pub fn decode_animation(
    bytes: &[u8],
    budget: usize,
    cancel: &AtomicBool,
) -> Result<Option<DecodedAnimation>, ImageError> {
    let decoder = WebPDecoder::new(Cursor::new(bytes))?;
    if !decoder.has_animation() {
        return Ok(None);
    }
    // Every frame is the size of the canvas, which the header gives away. If
    // two of them cannot fit there is no animation to be had, and finding that
    // out by decoding would mean allocating the oversized frame first.
    let (width, height) = decoder.dimensions();
    let frame_bytes = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if frame_bytes.saturating_mul(2) > budget {
        return Ok(None);
    }
    let mut frames = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    // One frame at a time, converted and released before the next: collecting
    // them all first held every frame twice over at the peak.
    for frame in decoder.into_frames() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let frame = frame?;
        let delay: Duration = frame.delay().into();
        let rgba = frame.into_buffer();
        let (w, h) = rgba.dimensions();
        used = used.saturating_add(rgba.as_raw().len());
        if used > budget {
            truncated = true;
            break;
        }
        frames.push(WebpFrame {
            image: Arc::new(to_color_image(w, h, rgba.as_raw())),
            delay,
        });
    }
    // Flagged as animated but with a single frame (or none that fits): a still.
    if frames.len() <= 1 {
        return Ok(None);
    }
    Ok(Some(DecodedAnimation { frames, truncated }))
}

fn to_color_image(width: u32, height: u32, rgba: &[u8]) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba)
}

/// Where an animation's bytes come from.
pub enum Source {
    /// A local file, read by the decode task rather than by the UI thread.
    File(PathBuf),
    /// Bytes already in memory (the remote loader's).
    Bytes(egui::load::Bytes),
}

/// An animation being decoded on the blocking pool. Reading and decoding used
/// to run inside the UI's frame: arrowing onto a large still WebP froze the
/// window for a full decode, and an animation for all of its frames. Until this
/// resolves the panel shows the file's first frame through the still path.
/// Dropping it abandons the decode at the next frame.
pub struct PendingAnimation {
    rx: mpsc::Receiver<Option<DecodedAnimation>>,
    cancel: Arc<AtomicBool>,
}

impl PendingAnimation {
    pub fn spawn(source: Source, runtime: &tokio::runtime::Runtime, ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_task = cancel.clone();
        let ctx = ctx.clone();
        runtime.spawn_blocking(move || {
            // A decoder panic, a file that cannot be read, a malformed stream:
            // all of them just mean "show it as a still".
            let decoded = crate::decoded::catching_panics(|| {
                let bytes = match &source {
                    Source::File(path) => {
                        egui::load::Bytes::from(std::fs::read(path).map_err(|e| e.to_string())?)
                    }
                    Source::Bytes(bytes) => bytes.clone(),
                };
                decode_animation(bytes.as_ref(), MAX_ANIMATION_BYTES, &cancel_task)
                    .map_err(|e| e.to_string())
            });
            let _ = tx.send(decoded.unwrap_or(None));
            ctx.request_repaint();
        });
        Self { rx, cancel }
    }

    /// `Some` once the decode is over: the animation, or `None` for "play it as
    /// a still". `None` while it is still running.
    pub fn poll(&self) -> Option<Option<DecodedAnimation>> {
        match self.rx.try_recv() {
            Ok(decoded) => Some(decoded),
            Err(mpsc::TryRecvError::Empty) => None,
            // The task is gone without a word (runtime shutting down).
            Err(mpsc::TryRecvError::Disconnected) => Some(None),
        }
    }
}

impl Drop for PendingAnimation {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
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
                // An `Arc` clone: the pixels are not copied on the way.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A lossless still WebP of one colour, as the `image` crate encodes it.
    fn still_webp(width: u32, height: u32, shade: u8) -> Vec<u8> {
        let pixels = image::RgbImage::from_pixel(width, height, image::Rgb([shade, 0, 0]));
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .encode(
                pixels.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .expect("encode");
        out
    }

    fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn u24(value: u32) -> [u8; 3] {
        let bytes = value.to_le_bytes();
        [bytes[0], bytes[1], bytes[2]]
    }

    /// An animated WebP of `count` full-canvas frames, assembled by hand — the
    /// `image` crate cannot encode one — from stills it can: a VP8X header with
    /// the animation flag, an ANIM chunk, and one ANMF per frame wrapping that
    /// still's own VP8L chunk.
    fn animated_webp(count: usize, width: u32, height: u32, delay_ms: u32) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        let mut vp8x = vec![0x02, 0, 0, 0];
        vp8x.extend_from_slice(&u24(width - 1));
        vp8x.extend_from_slice(&u24(height - 1));
        body.extend(chunk(b"VP8X", &vp8x));
        // Background colour, then loop count 0 (forever).
        body.extend(chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]));
        for i in 0..count {
            let still = still_webp(width, height, (i * 40) as u8);
            // Everything after "RIFF", the size and "WEBP" is the VP8L chunk.
            let bitstream = &still[12..];
            let mut anmf = Vec::new();
            anmf.extend_from_slice(&u24(0));
            anmf.extend_from_slice(&u24(0));
            anmf.extend_from_slice(&u24(width - 1));
            anmf.extend_from_slice(&u24(height - 1));
            anmf.extend_from_slice(&u24(delay_ms));
            anmf.push(0);
            anmf.extend_from_slice(bitstream);
            body.extend(chunk(b"ANMF", &anmf));
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    fn live() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn a_still_webp_is_left_to_the_still_path() {
        let still = still_webp(8, 8, 200);
        let decoded = decode_animation(&still, MAX_ANIMATION_BYTES, &live()).expect("decode");
        assert!(decoded.is_none());
    }

    #[test]
    fn an_animation_decodes_to_its_frames_and_delays() {
        let bytes = animated_webp(3, 8, 4, 70);
        let decoded = decode_animation(&bytes, MAX_ANIMATION_BYTES, &live())
            .expect("decode")
            .expect("animated");
        assert_eq!(decoded.frames.len(), 3);
        assert!(!decoded.truncated);
        assert_eq!(decoded.frames[0].image.size, [8, 4]);
        assert_eq!(decoded.frames[1].delay, Duration::from_millis(70));
    }

    #[test]
    fn frames_past_the_memory_budget_are_not_decoded() {
        // Each 8x4 frame is 128 bytes of RGBA; room for two and a half.
        let bytes = animated_webp(5, 8, 4, 70);
        let decoded = decode_animation(&bytes, 320, &live())
            .expect("decode")
            .expect("animated");
        assert_eq!(decoded.frames.len(), 2);
        assert!(decoded.truncated);
        // A budget that cannot hold even two frames leaves a still — decided
        // from the header, before any frame is decoded.
        assert!(
            decode_animation(&bytes, 200, &live())
                .expect("decode")
                .is_none()
        );
    }

    #[test]
    fn an_abandoned_decode_stops_and_yields_nothing() {
        let bytes = animated_webp(5, 8, 4, 70);
        let cancelled = AtomicBool::new(true);
        assert!(
            decode_animation(&bytes, MAX_ANIMATION_BYTES, &cancelled)
                .expect("decode")
                .is_none()
        );
    }

    #[test]
    fn a_pending_animation_resolves_off_the_calling_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("anim.webp");
        std::fs::write(&path, animated_webp(3, 8, 4, 70)).unwrap();
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let pending = PendingAnimation::spawn(Source::File(path), &rt, &egui::Context::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        let decoded = loop {
            if let Some(decoded) = pending.poll() {
                break decoded;
            }
            assert!(Instant::now() < deadline, "no result within 5s");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(decoded.expect("animated").frames.len(), 3);

        // A file that is not there is simply not an animation.
        let missing = PendingAnimation::spawn(
            Source::File(dir.path().join("gone.webp")),
            &rt,
            &egui::Context::default(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let decoded = loop {
            if let Some(decoded) = missing.poll() {
                break decoded;
            }
            assert!(Instant::now() < deadline, "no result within 5s");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(decoded.is_none());
    }

    #[test]
    fn is_webp_accepts_uppercase_extensions() {
        assert!(is_webp("file:///a/anim.webp"));
        assert!(is_webp("sftp://nas/a/ANIM.WEBP"));
        assert!(is_webp("file:///a/Anim.WebP"));
        assert!(!is_webp("file:///a/anim.gif"));
    }
}
