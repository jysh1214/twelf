use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ImageCache {
    inner: Option<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    blobs_dir: PathBuf,
}

impl ImageCache {
    pub fn new() -> Self {
        match Self::try_init() {
            Ok(inner) => Self { inner: Some(inner) },
            Err(e) => {
                eprintln!("[twelf] image cache disabled: {e}");
                Self { inner: None }
            }
        }
    }

    fn try_init() -> Result<Inner, String> {
        let mut dir = dirs::cache_dir().ok_or_else(|| "no cache dir available".to_string())?;
        dir.push("twelf");
        fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
        let blobs_dir = dir.join("blobs");
        fs::create_dir_all(&blobs_dir)
            .map_err(|e| format!("failed to create {}: {e}", blobs_dir.display()))?;
        let db_path = dir.join("cache.db");
        let conn = Connection::open(&db_path)
            .map_err(|e| format!("failed to open {}: {e}", db_path.display()))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS entries (
                uri TEXT PRIMARY KEY,
                blob_file TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|e| format!("failed to create table: {e}"))?;
        Ok(Inner {
            conn: Mutex::new(conn),
            blobs_dir,
        })
    }

    pub fn get(&self, uri: &str) -> Option<Vec<u8>> {
        let inner = self.inner.as_ref()?;
        let blob_file: String = {
            let conn = inner.conn.lock().ok()?;
            conn.query_row(
                "SELECT blob_file FROM entries WHERE uri = ?1",
                params![uri],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()?
        };
        let bytes = fs::read(inner.blobs_dir.join(&blob_file)).ok()?;
        if let Ok(conn) = inner.conn.lock() {
            let _ = conn.execute(
                "UPDATE entries SET last_accessed = ?1 WHERE uri = ?2",
                params![unix_now(), uri],
            );
        }
        Some(bytes)
    }

    pub fn put(&self, uri: &str, bytes: &[u8]) {
        let Some(inner) = self.inner.as_ref() else { return };
        let file_name = cache_key(uri);
        let final_path = inner.blobs_dir.join(&file_name);
        let tmp_path = inner.blobs_dir.join(format!("{file_name}.tmp"));
        if let Err(e) = fs::write(&tmp_path, bytes) {
            eprintln!("[twelf] failed to write {}: {e}", tmp_path.display());
            return;
        }
        if let Err(e) = fs::rename(&tmp_path, &final_path) {
            eprintln!("[twelf] failed to finalize {}: {e}", final_path.display());
            let _ = fs::remove_file(&tmp_path);
            return;
        }
        if let Ok(conn) = inner.conn.lock()
            && let Err(e) = conn.execute(
                "INSERT OR REPLACE INTO entries (uri, blob_file, byte_size, last_accessed) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![uri, file_name, bytes.len() as i64, unix_now()],
            )
        {
            eprintln!("[twelf] failed to record cache entry for {uri}: {e}");
        }
    }

    pub fn clear(&self) {
        let Some(inner) = self.inner.as_ref() else { return };
        if let Ok(conn) = inner.conn.lock()
            && let Err(e) = conn.execute("DELETE FROM entries", [])
        {
            eprintln!("[twelf] failed to clear cache rows: {e}");
        }
        if let Ok(iter) = fs::read_dir(&inner.blobs_dir) {
            for entry in iter.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    pub fn total_size_bytes(&self) -> u64 {
        let Some(inner) = self.inner.as_ref() else { return 0 };
        let Ok(conn) = inner.conn.lock() else { return 0 };
        conn.query_row(
            "SELECT COALESCE(SUM(byte_size), 0) FROM entries",
            [],
            |row| row.get::<_, i64>(0),
        )
        .ok()
        .map(|n| n.max(0) as u64)
        .unwrap_or(0)
    }
}

fn cache_key(uri: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in uri.as_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
