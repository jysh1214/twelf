use eframe::egui;
use egui::load::{BytesPoll, ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use crate::backoff::BackOff;
use crate::lru::{ByteLru, ByteSized};
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

/// Decodes remote (`sftp://`) images off the UI thread into a bounded cache.
/// Registered last so egui (which tries image loaders most-recently-added-first)
/// consults it before the synchronous `egui_extras` decoder.
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
        if !uri.starts_with("sftp://") {
            return Err(LoadError::NotSupported);
        }
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
            let decoded = decode_image(&uri_owned, bytes.as_ref());
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
        let mut state = self.state.lock().unwrap();
        state.cache.forget(uri);
        state.pending.remove(uri);
        state.failed.clear(uri);
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

fn decode_image(uri: &str, bytes: &[u8]) -> Result<ColorImage, String> {
    if crate::heic::is_heic(uri) {
        crate::heic::decode_bytes(bytes).map_err(|e| e.to_string())
    } else {
        let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
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
}
