use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Remembers recently-failed URIs and suppresses retries within a window.
pub struct BackOff {
    failed: HashMap<String, Instant>,
    window: Duration,
}

impl BackOff {
    pub fn new(window: Duration) -> Self {
        Self {
            failed: HashMap::new(),
            window,
        }
    }

    /// True if `uri` failed within the back-off window.
    pub fn is_backed_off(&self, uri: &str) -> bool {
        self.failed
            .get(uri)
            .is_some_and(|at| at.elapsed() < self.window)
    }

    pub fn record(&mut self, uri: String) {
        self.failed.insert(uri, Instant::now());
    }

    pub fn clear(&mut self, uri: &str) {
        self.failed.remove(uri);
    }

    pub fn clear_all(&mut self) {
        self.failed.clear();
    }
}
