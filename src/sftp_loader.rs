use crate::backoff::BackOff;
use crate::cache::ImageCache;
use eframe::egui;
use egui::Context;
use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use russh_sftp::client::SftpSession;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RETRY_BACKOFF: Duration = Duration::from_secs(30);

struct LoaderState {
    cache: HashMap<String, Bytes>,
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
                cache: HashMap::new(),
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
        {
            let state = self.state.lock().unwrap();
            if let Some(bytes) = state.cache.get(uri).cloned() {
                return Ok(BytesPoll::Ready {
                    size: None,
                    bytes,
                    mime: None,
                });
            }
            if state.pending.contains(uri) {
                return Ok(BytesPoll::Pending { size: None });
            }
            if state.failed.is_backed_off(uri) {
                return Err(LoadError::Loading("previous load failed".to_string()));
            }
        }
        let session_opt = self.session.lock().unwrap().clone();
        let Some(session) = session_opt else {
            return Err(LoadError::Loading("not connected".to_string()));
        };
        let path = path.to_string();
        self.state.lock().unwrap().pending.insert(uri.to_string());
        let state_clone = self.state.clone();
        let disk_clone = self.disk.clone();
        let uri_owned = uri.to_string();
        let ctx_clone = ctx.clone();
        self.handle.spawn(async move {
            // Fingerprint the remote file so a cached blob is reused only when its
            // size+mtime still match. A failed stat yields None/None, which degrades
            // to serving the cached blob (if any) and otherwise reading fresh.
            let meta = session.metadata(path.clone()).await.ok();
            let mtime = meta.as_ref().and_then(|m| m.mtime).map(|t| t as i64);
            let size = meta.as_ref().and_then(|m| m.size).map(|s| s as i64);
            let bytes = match disk_clone.get(&uri_owned, mtime, size) {
                Some(vec) => Some(vec),
                None => match session.read(path).await {
                    Ok(vec) => {
                        disk_clone.put(&uri_owned, &vec, mtime);
                        Some(vec)
                    }
                    Err(e) => {
                        crate::log!("failed to read {uri_owned}: {e}");
                        None
                    }
                },
            };
            let mut state = state_clone.lock().unwrap();
            state.pending.remove(&uri_owned);
            match bytes {
                Some(vec) => {
                    state.failed.clear(&uri_owned);
                    state.cache.insert(uri_owned, vec.into());
                }
                None => {
                    state.failed.record(uri_owned);
                }
            }
            drop(state);
            ctx_clone.request_repaint();
        });
        Ok(BytesPoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        let mut state = self.state.lock().unwrap();
        state.cache.remove(uri);
        state.failed.clear(uri);
    }

    fn forget_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.cache.clear();
        state.failed.clear_all();
    }

    fn byte_size(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .cache
            .values()
            .map(|b| b.len())
            .sum()
    }

    fn has_pending(&self) -> bool {
        !self.state.lock().unwrap().pending.is_empty()
    }
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
    fn remote_path_strips_frame_fragment() {
        assert_eq!(remote_path("sftp://host/dir/a.webp#0"), Some("/dir/a.webp"));
        assert_eq!(remote_path("sftp://host/dir/a.jpg"), Some("/dir/a.jpg"));
        assert_eq!(remote_path("file:///dir/a.webp"), None);
    }
}
