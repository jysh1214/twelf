use crate::backoff::BackOff;
use crate::cache::ImageCache;
use crate::lru::{ByteLru, ByteSized};
use eframe::egui;
use egui::Context;
use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use russh_sftp::client::SftpSession;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// Cap on the raw remote bytes held in memory. egui never evicts a custom bytes
/// loader on its own, so before this every file fetched in a session stayed
/// resident — a "Load" over a folder of 800 6 MB JPEGs kept ~4.8 GB. Compressed
/// bytes are far smaller than the decoded images they feed, so this sits well
/// under the decoded cache's own cap.
const BYTES_CACHE_CAP: usize = 256 * 1024 * 1024;

impl ByteSized for Bytes {
    fn byte_size(&self) -> usize {
        self.len()
    }
}

struct LoaderState {
    cache: ByteLru<Bytes>,
    pending: HashSet<String>,
    failed: BackOff,
}

pub struct SftpBytesLoader {
    session: Arc<Mutex<Option<Arc<SftpSession>>>>,
    handle: tokio::runtime::Handle,
    state: Arc<Mutex<LoaderState>>,
    disk: Arc<ImageCache>,
}

impl SftpBytesLoader {
    pub fn new(
        session: Arc<Mutex<Option<Arc<SftpSession>>>>,
        handle: tokio::runtime::Handle,
        disk: Arc<ImageCache>,
    ) -> Self {
        Self {
            session,
            handle,
            state: Arc::new(Mutex::new(LoaderState {
                cache: ByteLru::new(BYTES_CACHE_CAP),
                pending: HashSet::new(),
                failed: BackOff::new(RETRY_BACKOFF),
            })),
            disk,
        }
    }
}

impl BytesLoader for SftpBytesLoader {
    fn id(&self) -> &str {
        concat!(module_path!(), "::SftpBytesLoader")
    }

    fn load(&self, ctx: &Context, uri: &str) -> BytesLoadResult {
        let Some(path) = remote_path(uri) else {
            return Err(LoadError::NotSupported);
        };
        let key = canonical_key(uri);
        {
            let mut state = self.state.lock().unwrap();
            if let Some(bytes) = state.cache.get(&key) {
                return Ok(BytesPoll::Ready {
                    size: None,
                    bytes,
                    mime: None,
                });
            }
            if state.pending.contains(&key) {
                return Ok(BytesPoll::Pending { size: None });
            }
            if state.failed.is_backed_off(&key) {
                return Err(LoadError::Loading("previous load failed".to_string()));
            }
        }
        let session_opt = self.session.lock().unwrap().clone();
        let Some(session) = session_opt else {
            return Err(LoadError::Loading("not connected".to_string()));
        };
        let path = path.to_string();
        self.state.lock().unwrap().pending.insert(key.clone());
        let state_clone = self.state.clone();
        let disk_clone = self.disk.clone();
        let key_owned = key;
        let ctx_clone = ctx.clone();
        self.handle.spawn(async move {
            // Fingerprint the remote file so a cached blob is reused only when its
            // size+mtime still match. A failed stat yields None/None, which degrades
            // to serving the cached blob (if any) and otherwise reading fresh.
            let meta = session.metadata(path.clone()).await.ok();
            let mtime = meta.as_ref().and_then(|m| m.mtime).map(|t| t as i64);
            let size = meta.as_ref().and_then(|m| m.size).map(|s| s as i64);
            // The disk cache is synchronous sqlite plus a whole-blob file read
            // or write. Running it inline parks a tokio worker that also drives
            // the SSH session, so it goes to the blocking pool — the same reason
            // `decoded` hands its decode to `spawn_blocking`.
            let hit = {
                let disk = disk_clone.clone();
                let key = key_owned.clone();
                tokio::task::spawn_blocking(move || disk.get(&key, mtime, size))
                    .await
                    .ok()
                    .flatten()
            };
            let bytes = match hit {
                Some(vec) => Some(vec),
                None => match session.read(path).await {
                    Ok(vec) => {
                        let disk = disk_clone.clone();
                        let key = key_owned.clone();
                        // `vec` is moved in and handed back, so storing it does
                        // not cost a second copy of the whole file.
                        tokio::task::spawn_blocking(move || {
                            disk.put(&key, &vec, mtime);
                            vec
                        })
                        .await
                        .ok()
                    }
                    Err(e) => {
                        crate::log!("failed to read {key_owned}: {e}");
                        None
                    }
                },
            };
            let mut state = state_clone.lock().unwrap();
            state.pending.remove(&key_owned);
            match bytes {
                Some(vec) => {
                    state.failed.clear(&key_owned);
                    state.cache.put(key_owned, vec.into());
                }
                None => {
                    state.failed.record(key_owned);
                }
            }
            drop(state);
            ctx_clone.request_repaint();
        });
        Ok(BytesPoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        let key = canonical_key(uri);
        let mut state = self.state.lock().unwrap();
        state.cache.forget(&key);
        state.failed.clear(&key);
    }

    fn forget_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.cache.forget_all();
        state.failed.clear_all();
    }

    fn byte_size(&self) -> usize {
        self.state.lock().unwrap().cache.byte_size()
    }

    fn has_pending(&self) -> bool {
        !self.state.lock().unwrap().pending.is_empty()
    }
}

/// The single key one remote file is cached under, in memory and on disk.
///
/// egui rewrites an animated-format URI to `uri#<frame>` before asking for it,
/// and a webp goes down that path even when it holds one frame — so the same
/// file arrives here as both `…/a.webp` and `…/a.webp#0`. Keying on the raw URI
/// fetched, transferred and stored it twice; the network read already strips the
/// fragment, so the cache has to agree with it.
fn canonical_key(uri: &str) -> String {
    egui::decode_animated_image_uri(uri)
        .map_or(uri, |(base, _)| base)
        .to_string()
}

/// Recover the remote path from an `sftp://{host}{absolute_path}` URI, dropping
/// the `#frame` fragment egui appends to webp/gif URIs (which is never part of
/// the filesystem path). Returns `None` for non-sftp URIs.
fn remote_path(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("sftp://")?;
    let slash = rest.find('/')?;
    let path = &rest[slash..];
    Some(egui::decode_animated_image_uri(path).map_or(path, |(p, _)| p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    fn make_loader() -> (SftpBytesLoader, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let loader = SftpBytesLoader::new(
            Arc::new(Mutex::new(None)),
            rt.handle().clone(),
            Arc::new(ImageCache::new()),
        );
        (loader, rt)
    }

    #[test]
    fn backed_off_uri_errors_without_spawning() {
        let (loader, _rt) = make_loader();
        let uri = "sftp://host/a.jpg";
        loader.state.lock().unwrap().failed.record(uri.to_string());
        let ctx = egui::Context::default();
        assert!(matches!(
            loader.load(&ctx, uri),
            Err(LoadError::Loading(m)) if m == "previous load failed"
        ));
        assert!(loader.state.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn forget_clears_failed_entry() {
        let (loader, _rt) = make_loader();
        let uri = "sftp://host/a.jpg";
        loader.state.lock().unwrap().failed.record(uri.to_string());
        loader.forget(uri);
        assert!(!loader.state.lock().unwrap().failed.is_backed_off(uri));
    }

    #[test]
    fn forget_all_clears_failed() {
        let (loader, _rt) = make_loader();
        let uri = "sftp://host/a.jpg";
        loader.state.lock().unwrap().failed.record(uri.to_string());
        loader.forget_all();
        assert!(!loader.state.lock().unwrap().failed.is_backed_off(uri));
    }

    #[test]
    fn canonical_key_collapses_the_frame_fragment() {
        // Both forms egui asks for must land on one cache entry.
        assert_eq!(canonical_key("sftp://host/a.webp#0"), "sftp://host/a.webp");
        assert_eq!(canonical_key("sftp://host/a.webp"), "sftp://host/a.webp");
        // A plain URI is untouched, fragment or not.
        assert_eq!(canonical_key("sftp://host/a.jpg"), "sftp://host/a.jpg");
    }

    #[test]
    fn a_fragmented_uri_hits_the_entry_stored_bare() {
        let (loader, _rt) = make_loader();
        loader
            .state
            .lock()
            .unwrap()
            .cache
            .put("sftp://host/a.webp".to_string(), Bytes::from(vec![1u8, 2, 3]));
        let ctx = egui::Context::default();
        // Without the shared key this would miss and fetch the file again.
        assert!(matches!(
            loader.load(&ctx, "sftp://host/a.webp#0"),
            Ok(BytesPoll::Ready { .. })
        ));
    }

    #[test]
    fn remote_path_strips_frame_fragment() {
        assert_eq!(remote_path("sftp://host/dir/a.webp#0"), Some("/dir/a.webp"));
        assert_eq!(remote_path("sftp://host/dir/a.jpg"), Some("/dir/a.jpg"));
        assert_eq!(remote_path("file:///dir/a.webp"), None);
    }
}
