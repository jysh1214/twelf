use crate::backoff::BackOff;
use crate::lru::{ByteLru, ByteSized};
use crate::sftp_loader::canonical_key;
use eframe::egui;
use egui::load::{BytesPoll, ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use egui::{ColorImage, Context};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DECODED_CACHE_CAP: usize = 512 * 1024 * 1024;
const DECODE_RETRY_BACKOFF: Duration = Duration::from_secs(30);

impl ByteSized for Arc<ColorImage> {
    fn byte_size(&self) -> usize {
        self.size[0] * self.size[1] * 4
    }
}

struct LoaderState {
    cache: ByteLru<Arc<ColorImage>>,
    pending: HashSet<String>,
    failed: BackOff,
}

/// Decodes images off the UI thread into a bounded cache: every remote
/// (`sftp://`) one, and every local (`file://`) one but HEIC, which `HeicLoader`
/// reads straight from disk. Registered last so egui (which tries image loaders
/// most-recently-added-first) consults it before the `egui_extras` decoder —
/// which ignores EXIF orientation, so local portrait photos came out sideways
/// while the same files over SFTP would not have.
pub struct DecodedImageLoader {
    handle: tokio::runtime::Handle,
    state: Arc<Mutex<LoaderState>>,
}

impl DecodedImageLoader {
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        Self {
            handle,
            state: Arc::new(Mutex::new(LoaderState {
                cache: ByteLru::new(DECODED_CACHE_CAP),
                pending: HashSet::new(),
                failed: BackOff::new(DECODE_RETRY_BACKOFF),
            })),
        }
    }
}

impl ImageLoader for DecodedImageLoader {
    fn id(&self) -> &str {
        concat!(module_path!(), "::DecodedImageLoader")
    }

    fn load(&self, ctx: &Context, uri: &str, _size_hint: SizeHint) -> ImageLoadResult {
        if !handles(uri) {
            return Err(LoadError::NotSupported);
        }
        // One key per file, shared with the bytes loader. egui asks for a webp or
        // gif as `uri#0` while the prefetcher asks for the bare `uri`; keyed on
        // the raw string, a prefetched file was decoded again on selection and
        // held twice in the cache.
        let key = canonical_key(uri);
        let uri = key.as_str();
        {
            let mut state = self.state.lock().unwrap();
            if let Some(image) = state.cache.get(uri) {
                return Ok(ImagePoll::Ready { image });
            }
            if state.pending.contains(uri) {
                return Ok(ImagePoll::Pending { size: None });
            }
            if state.failed.is_backed_off(uri) {
                return Err(LoadError::Loading("previous decode failed".to_string()));
            }
        }
        let bytes = match ctx.try_load_bytes(uri) {
            Ok(BytesPoll::Ready { bytes, .. }) => bytes,
            Ok(BytesPoll::Pending { .. }) => return Ok(ImagePoll::Pending { size: None }),
            Err(e) => return Err(e),
        };
        self.state.lock().unwrap().pending.insert(uri.to_string());
        let state_clone = self.state.clone();
        let uri_owned = uri.to_string();
        let ctx_clone = ctx.clone();
        // Decode is CPU-bound; keep it off the async workers and the UI thread.
        self.handle.spawn_blocking(move || {
            let decoded = catching_panics(|| decode_image(&uri_owned, bytes.as_ref()));
            let mut state = state_clone.lock().unwrap();
            state.pending.remove(&uri_owned);
            match decoded {
                Ok(image) => {
                    state.failed.clear(&uri_owned);
                    state.cache.put(uri_owned, Arc::new(image));
                }
                Err(e) => {
                    crate::log!("decode failed for {uri_owned}: {e}");
                    state.failed.record(uri_owned);
                }
            }
            drop(state);
            ctx_clone.request_repaint();
        });
        Ok(ImagePoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        let key = canonical_key(uri);
        let mut state = self.state.lock().unwrap();
        state.cache.forget(&key);
        state.pending.remove(&key);
        state.failed.clear(&key);
    }

    fn forget_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.cache.forget_all();
        state.pending.clear();
        state.failed.clear_all();
    }

    fn byte_size(&self) -> usize {
        self.state.lock().unwrap().cache.byte_size()
    }
}

/// Run a decoder, turning a panic into an ordinary failure. Decoders meet files
/// from anywhere, and the image and HEIC crates have panicked on malformed ones
/// before. On the blocking pool tokio swallows the panic along with the rest of
/// the task — the clean-up included, so the URI stayed `pending` for good: a
/// spinner that never resolves, a prefetch slot never freed, and a repaint
/// requested every frame. On the UI thread it simply ends the app.
pub(crate) fn catching_panics<T>(decode: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(decode))
        .unwrap_or_else(|_| Err("the decoder panicked".to_string()))
}

/// Whether this loader decodes `uri`; see `DecodedImageLoader`.
fn handles(uri: &str) -> bool {
    uri.starts_with("sftp://") || (uri.starts_with("file://") && !crate::heic::is_heic(uri))
}

fn decode_image(uri: &str, bytes: &[u8]) -> Result<ColorImage, String> {
    if crate::heic::is_heic(uri) {
        crate::heic::decode_bytes(bytes)
    } else {
        use image::ImageDecoder as _;
        // `image::load_from_memory` never looks at the EXIF orientation, so a
        // phone's portrait JPEG rendered on its side. libheif applies its own
        // transforms, which is why the HEIC of the same shot was upright.
        let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?
            .into_decoder()
            .map_err(|e| e.to_string())?;
        // Unreadable orientation metadata is no reason to refuse the picture.
        let orientation = decoder
            .orientation()
            .unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
        img.apply_orientation(orientation);
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        Ok(ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            rgba.as_raw(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eviction policy itself is covered in `lru`; this pins the size a
    /// decoded image is charged, which is what makes the 512 MB cap meaningful.
    #[test]
    fn a_decoded_image_is_charged_its_rgba_size() {
        let image: Arc<ColorImage> = Arc::new(ColorImage::from_rgba_unmultiplied(
            [10, 4],
            &vec![255u8; 10 * 4 * 4],
        ));
        assert_eq!(image.byte_size(), 10 * 4 * 4);
    }

    /// A 16x8 JPEG, left half red and right half blue, tagged with EXIF
    /// `orientation` (spliced in as an APP1 segment after the JFIF header).
    fn jpeg_with_orientation(orientation: u8) -> Vec<u8> {
        let mut pixels = image::RgbImage::new(16, 8);
        for (x, _, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = if x < 8 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            };
        }
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
            .encode_image(&pixels)
            .expect("encode");
        let mut app1 = vec![0xFF, 0xE1, 0x00, 0x22];
        app1.extend_from_slice(b"Exif\0\0MM\0*\0\0\0\x08");
        // One IFD entry: tag 0x0112 (Orientation), SHORT, count 1, the value.
        app1.extend_from_slice(&[0x00, 0x01, 0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01]);
        app1.extend_from_slice(&[0x00, orientation, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            &jpeg[2..4],
            &[0xFF, 0xE0],
            "encoder writes a JFIF header first"
        );
        let after_jfif = 4 + u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
        jpeg.splice(after_jfif..after_jfif, app1);
        jpeg
    }

    #[test]
    fn exif_orientation_is_applied() {
        let is_red = |c: egui::Color32| c.r() > 150 && c.b() < 100;
        let is_blue = |c: egui::Color32| c.b() > 150 && c.r() < 100;

        // Untagged (1 = upright): as stored, red on the left.
        let upright = decode_image("file:///a.jpg", &jpeg_with_orientation(1)).expect("decode");
        assert_eq!(upright.size, [16, 8]);
        assert!(is_red(upright[(2, 4)]) && is_blue(upright[(13, 4)]));

        // 6 = the camera was held rotated; showing it needs a quarter turn
        // clockwise, which puts the stored left edge on top.
        let portrait = decode_image("file:///a.jpg", &jpeg_with_orientation(6)).expect("decode");
        assert_eq!(portrait.size, [8, 16]);
        assert!(is_red(portrait[(4, 2)]) && is_blue(portrait[(4, 13)]));
    }

    #[test]
    fn a_decoder_panic_is_a_failed_decode() {
        let panicked: Result<ColorImage, String> = catching_panics(|| panic!("malformed chunk"));
        assert_eq!(panicked.unwrap_err(), "the decoder panicked");
        // Ordinary outcomes pass straight through.
        assert_eq!(catching_panics(|| Ok::<_, String>(7)), Ok(7));
        assert_eq!(
            catching_panics(|| Err::<u8, _>("truncated".to_string())),
            Err("truncated".to_string())
        );
    }

    #[test]
    fn local_images_are_decoded_here_except_heic() {
        assert!(handles("sftp://nas/photos/a.jpg"));
        assert!(handles("sftp://nas/photos/a.heic"));
        assert!(handles("file:///photos/a.jpg"));
        // Egui asks for an animated format by frame; still ours.
        assert!(handles("file:///photos/a.webp#0"));
        // `HeicLoader` reads these from disk itself.
        assert!(!handles("file:///photos/a.HEIC"));
        assert!(!handles("https://example.com/a.jpg"));
    }

    #[test]
    fn a_fragmented_uri_shares_the_entry_stored_bare() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let loader = DecodedImageLoader::new(rt.handle().clone());
        let image = Arc::new(ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 255]));
        // What a prefetch leaves behind: the decode, under the bare URI.
        loader
            .state
            .lock()
            .unwrap()
            .cache
            .put("sftp://host/a.webp".to_string(), image);
        let ctx = Context::default();
        // Selecting the file asks for frame 0; that must not decode it again.
        assert!(matches!(
            loader.load(&ctx, "sftp://host/a.webp#0", SizeHint::default()),
            Ok(ImagePoll::Ready { .. })
        ));
        // And forgetting either form drops the one entry.
        loader.forget("sftp://host/a.webp#0");
        assert!(
            loader
                .state
                .lock()
                .unwrap()
                .cache
                .get("sftp://host/a.webp")
                .is_none()
        );
    }
}
