use crate::cache::ImageCache;
use eframe::egui;
use egui::Context;
use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use russh_sftp::client::SftpSession;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

struct LoaderState {
    cache: HashMap<String, Bytes>,
    pending: HashSet<String>,
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
        let Some(path) = uri.strip_prefix("sftp://") else {
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
        }
        if let Some(vec) = self.disk.get(uri) {
            let bytes: Bytes = vec.into();
            self.state
                .lock()
                .unwrap()
                .cache
                .insert(uri.to_string(), bytes.clone());
            return Ok(BytesPoll::Ready {
                size: None,
                bytes,
                mime: None,
            });
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
            let result = session.read(path).await;
            let mut state = state_clone.lock().unwrap();
            state.pending.remove(&uri_owned);
            if let Ok(vec) = result {
                disk_clone.put(&uri_owned, &vec, None);
                state.cache.insert(uri_owned, vec.into());
            }
            drop(state);
            ctx_clone.request_repaint();
        });
        Ok(BytesPoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        self.state.lock().unwrap().cache.remove(uri);
    }

    fn forget_all(&self) {
        self.state.lock().unwrap().cache.clear();
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
