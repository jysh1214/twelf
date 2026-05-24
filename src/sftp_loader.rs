use crate::cache::ImageCache;
use eframe::egui;
use egui::Context;
use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use russh_sftp::client::SftpSession;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RETRY_BACKOFF: Duration = Duration::from_secs(30);

struct LoaderState {
    cache: HashMap<String, Bytes>,
    pending: HashSet<String>,
    failed: HashMap<String, Instant>,
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
                failed: HashMap::new(),
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
        let Some(rest) = uri.strip_prefix("sftp://") else {
            return Err(LoadError::NotSupported);
        };
        // URIs are `sftp://{host}{absolute_path}`; recover the path after the host.
        let Some(slash) = rest.find('/') else {
            return Err(LoadError::NotSupported);
        };
        let path = &rest[slash..];
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
            if let Some(failed_at) = state.failed.get(uri)
                && failed_at.elapsed() < RETRY_BACKOFF
            {
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
                    Err(_) => None,
                },
            };
            let mut state = state_clone.lock().unwrap();
            state.pending.remove(&uri_owned);
            match bytes {
                Some(vec) => {
                    state.failed.remove(&uri_owned);
                    state.cache.insert(uri_owned, vec.into());
                }
                None => {
                    state.failed.insert(uri_owned, Instant::now());
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
        state.failed.remove(uri);
    }

    fn forget_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.cache.clear();
        state.failed.clear();
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
