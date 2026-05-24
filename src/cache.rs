use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ImageCache {
    inner: Mutex<Option<Inner>>,
}

struct Inner {
    conn: Connection,
    blobs_dir: PathBuf,
}

impl ImageCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub fn initialize(&self, ssh_key_path: &Path) {
        match Self::try_open(ssh_key_path) {
            Ok(inner) => {
                if let Ok(mut guard) = self.inner.lock() {
                    *guard = Some(inner);
                }
            }
            Err(e) => {
                eprintln!("[twelf] image cache disabled: {e}");
                if let Ok(mut guard) = self.inner.lock() {
                    *guard = None;
                }
            }
        }
    }

    pub fn is_initialized(&self) -> bool {
        self.inner.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    #[cfg(test)]
    fn initialize_at(&self, dir: &Path, key: &[u8]) {
        let key_hex = format!("{:x}", Sha256::digest(key));
        let inner = Self::open_at(dir, &key_hex).expect("open test cache");
        *self.inner.lock().unwrap() = Some(inner);
    }

    fn try_open(ssh_key_path: &Path) -> Result<Inner, String> {
        let key_bytes = fs::read(ssh_key_path)
            .map_err(|e| format!("failed to read SSH key {}: {e}", ssh_key_path.display()))?;
        let key_hex = format!("{:x}", Sha256::digest(&key_bytes));
        let mut dir = dirs::cache_dir().ok_or_else(|| "no cache dir available".to_string())?;
        dir.push("twelf");
        Self::open_at(&dir, &key_hex)
    }

    fn open_at(dir: &Path, key_hex: &str) -> Result<Inner, String> {
        fs::create_dir_all(dir)
            .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
        let blobs_dir = dir.join("blobs");
        fs::create_dir_all(&blobs_dir)
            .map_err(|e| format!("failed to create {}: {e}", blobs_dir.display()))?;
        let db_path = dir.join("cache.db");

        match Self::open_with_key(&db_path, key_hex) {
            Ok(conn) => Ok(Inner { conn, blobs_dir }),
            Err(_) => {
                let _ = fs::remove_file(&db_path);
                if let Ok(iter) = fs::read_dir(&blobs_dir) {
                    for entry in iter.flatten() {
                        let _ = fs::remove_file(entry.path());
                    }
                }
                let conn = Self::open_with_key(&db_path, key_hex)
                    .map_err(|e| format!("failed to open encrypted cache after wipe: {e}"))?;
                Ok(Inner { conn, blobs_dir })
            }
        }
    }

    fn open_with_key(db_path: &Path, key_hex: &str) -> Result<Connection, String> {
        let conn = Connection::open(db_path)
            .map_err(|e| format!("failed to open {}: {e}", db_path.display()))?;
        conn.execute_batch(&format!("PRAGMA key = \"x'{key_hex}'\""))
            .map_err(|e| format!("failed to set key: {e}"))?;
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok::<(), rusqlite::Error>(()))
            .map_err(|e| format!("decryption check failed: {e}"))?;
        let entries_exists = conn.prepare("SELECT 1 FROM entries LIMIT 0").is_ok();
        let has_fingerprint = conn.prepare("SELECT mtime FROM entries LIMIT 0").is_ok();
        if entries_exists && !has_fingerprint {
            conn.execute("DROP TABLE entries", [])
                .map_err(|e| format!("failed to drop outdated table: {e}"))?;
        }
        conn.execute(
            "CREATE TABLE IF NOT EXISTS entries (
                uri TEXT PRIMARY KEY,
                byte_size INTEGER NOT NULL,
                mtime INTEGER,
                last_accessed INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|e| format!("failed to create table: {e}"))?;
        Ok(conn)
    }

    pub fn get(&self, uri: &str, mtime: Option<i64>, size: Option<i64>) -> Option<Vec<u8>> {
        let (rowid, blob_path) = {
            let guard = self.inner.lock().ok()?;
            let inner = guard.as_ref()?;
            let (id, stored_size, stored_mtime) = inner
                .conn
                .query_row(
                    "SELECT rowid, byte_size, mtime FROM entries WHERE uri = ?1",
                    params![uri],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    },
                )
                .optional()
                .ok()
                .flatten()?;
            if size.is_some_and(|s| s != stored_size) {
                return None;
            }
            if let (Some(m), Some(sm)) = (mtime, stored_mtime)
                && m != sm
            {
                return None;
            }
            (id, inner.blobs_dir.join(id.to_string()))
        };
        let bytes = fs::read(&blob_path).ok()?;
        let guard = self.inner.lock().ok()?;
        let inner = guard.as_ref()?;
        let current: Option<i64> = inner
            .conn
            .query_row(
                "SELECT rowid FROM entries WHERE uri = ?1",
                params![uri],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten();
        if current != Some(rowid) {
            return None;
        }
        let _ = inner.conn.execute(
            "UPDATE entries SET last_accessed = ?1 WHERE rowid = ?2",
            params![unix_now(), rowid],
        );
        Some(bytes)
    }

    pub fn put(&self, uri: &str, bytes: &[u8], mtime: Option<i64>) {
        let size = bytes.len() as i64;
        let now = unix_now();

        let (rowid, is_new, blobs_dir) = {
            let Ok(guard) = self.inner.lock() else { return };
            let Some(inner) = guard.as_ref() else { return };
            let existing = inner
                .conn
                .query_row(
                    "SELECT rowid FROM entries WHERE uri = ?1",
                    params![uri],
                    |row| row.get::<_, i64>(0),
                )
                .optional();
            let (id, is_new) = match existing {
                Ok(Some(id)) => (id, false),
                Ok(None) => {
                    if let Err(e) = inner.conn.execute(
                        "INSERT INTO entries (uri, byte_size, last_accessed) VALUES (?1, 0, ?2)",
                        params![uri, now],
                    ) {
                        eprintln!("[twelf] failed to insert placeholder for {uri}: {e}");
                        return;
                    }
                    (inner.conn.last_insert_rowid(), true)
                }
                Err(e) => {
                    eprintln!("[twelf] failed to look up cache entry for {uri}: {e}");
                    return;
                }
            };
            (id, is_new, inner.blobs_dir.clone())
        };

        let file_name = rowid.to_string();
        let final_path = blobs_dir.join(&file_name);
        let tmp_path = blobs_dir.join(format!("{file_name}.tmp"));
        let blob_ok = match fs::write(&tmp_path, bytes) {
            Ok(()) => match fs::rename(&tmp_path, &final_path) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("[twelf] failed to finalize {}: {e}", final_path.display());
                    let _ = fs::remove_file(&tmp_path);
                    false
                }
            },
            Err(e) => {
                eprintln!("[twelf] failed to write {}: {e}", tmp_path.display());
                false
            }
        };

        if let Ok(guard) = self.inner.lock()
            && let Some(inner) = guard.as_ref()
        {
            if blob_ok {
                let _ = inner.conn.execute(
                    "UPDATE entries SET byte_size = ?1, mtime = ?2, last_accessed = ?3 WHERE rowid = ?4",
                    params![size, mtime, now, rowid],
                );
            } else if is_new {
                let _ = inner
                    .conn
                    .execute("DELETE FROM entries WHERE rowid = ?1", params![rowid]);
            }
        }
    }

    pub fn clear(&self) {
        let blobs_dir = {
            let Ok(guard) = self.inner.lock() else { return };
            let Some(inner) = guard.as_ref() else { return };
            if let Err(e) = inner.conn.execute("DELETE FROM entries", []) {
                eprintln!("[twelf] failed to clear cache rows: {e}");
            }
            inner.blobs_dir.clone()
        };
        if let Ok(iter) = fs::read_dir(&blobs_dir) {
            for entry in iter.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    pub fn total_size_bytes(&self) -> u64 {
        let Ok(guard) = self.inner.lock() else { return 0 };
        let Some(inner) = guard.as_ref() else { return 0 };
        inner
            .conn
            .query_row(
                "SELECT COALESCE(SUM(byte_size), 0) FROM entries",
                [],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|n| n.max(0) as u64)
            .unwrap_or(0)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
