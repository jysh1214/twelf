use eframe::egui;
use egui::load::{BytesPoll, ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use egui::{ColorImage, Context};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DECODED_CACHE_CAP: usize = 512 * 1024 * 1024;
const DECODE_RETRY_BACKOFF: Duration = Duration::from_secs(30);

struct Entry {
    image: Arc<ColorImage>,
    bytes: usize,
    last_used: u64,
}

/// Byte-capped LRU of decoded images, keyed by URI.
struct DecodedCache {
    map: HashMap<String, Entry>,
    total_bytes: usize,
    seq: u64,
    cap: usize,
}

impl DecodedCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            total_bytes: 0,
            seq: 0,
            cap: DECODED_CACHE_CAP,
        }
    }

    fn get(&mut self, uri: &str) -> Option<Arc<ColorImage>> {
        self.seq += 1;
        let now = self.seq;
        let entry = self.map.get_mut(uri)?;
        entry.last_used = now;
        Some(entry.image.clone())
    }

    fn put(&mut self, uri: String, image: Arc<ColorImage>) {
        let bytes = image.size[0] * image.size[1] * 4;
        if let Some(old) = self.map.remove(&uri) {
            self.total_bytes -= old.bytes;
        }
        // Evict least-recently-used until the newcomer fits (always keep the
        // newcomer, even if it alone exceeds the cap).
        while self.total_bytes + bytes > self.cap {
            let lru = self
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            match lru {
                Some(k) => {
                    if let Some(removed) = self.map.remove(&k) {
                        self.total_bytes -= removed.bytes;
                    }
                }
                None => break,
            }
        }
        self.seq += 1;
        let now = self.seq;
        self.total_bytes += bytes;
        self.map.insert(
            uri,
            Entry {
                image,
                bytes,
                last_used: now,
            },
        );
    }

    fn forget(&mut self, uri: &str) {
        if let Some(removed) = self.map.remove(uri) {
            self.total_bytes -= removed.bytes;
        }
    }

    fn forget_all(&mut self) {
        self.map.clear();
        self.total_bytes = 0;
    }

    fn byte_size(&self) -> usize {
        self.total_bytes
    }
}

struct LoaderState {
    cache: DecodedCache,
    pending: HashSet<String>,
    failed: HashMap<String, Instant>,
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
                cache: DecodedCache::new(),
                pending: HashSet::new(),
                failed: HashMap::new(),
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
            if let Some(failed_at) = state.failed.get(uri)
                && failed_at.elapsed() < DECODE_RETRY_BACKOFF
            {
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
                    state.failed.remove(&uri_owned);
                    state.cache.put(uri_owned, Arc::new(image));
                }
                Err(e) => {
                    eprintln!("[twelf] decode failed for {uri_owned}: {e}");
                    state.failed.insert(uri_owned, Instant::now());
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
        state.failed.remove(uri);
    }

    fn forget_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.cache.forget_all();
        state.pending.clear();
        state.failed.clear();
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

    fn img(pixels: usize) -> Arc<ColorImage> {
        Arc::new(ColorImage::from_rgba_unmultiplied(
            [pixels, 1],
            &vec![255u8; pixels * 4],
        ))
    }

    fn cache(cap: usize) -> DecodedCache {
        DecodedCache {
            map: HashMap::new(),
            total_bytes: 0,
            seq: 0,
            cap,
        }
    }

    #[test]
    fn evicts_least_recently_used_past_cap() {
        let mut c = cache(100); // fits two 40-byte entries
        c.put("a".into(), img(10));
        c.put("b".into(), img(10));
        c.put("c".into(), img(10)); // 120 > cap -> evict LRU (a)
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_some());
        assert!(c.get("c").is_some());
        assert!(c.byte_size() <= 100);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut c = cache(100);
        c.put("a".into(), img(10));
        c.put("b".into(), img(10));
        assert!(c.get("a").is_some()); // bump a; b becomes LRU
        c.put("c".into(), img(10)); // evict LRU (b)
        assert!(c.get("a").is_some());
        assert!(c.get("b").is_none());
        assert!(c.get("c").is_some());
    }

    #[test]
    fn forget_and_forget_all_drop_bytes() {
        let mut c = cache(1000);
        c.put("a".into(), img(10));
        c.put("b".into(), img(10));
        c.forget("a");
        assert!(c.get("a").is_none());
        assert_eq!(c.byte_size(), 40);
        c.forget_all();
        assert!(c.get("b").is_none());
        assert_eq!(c.byte_size(), 0);
    }
}
