use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Cap on one key's blob cache. The schema has always tracked
/// `last_accessed`, but nothing read it: every distinct remote image ever
/// viewed stayed on disk forever, and once the partition filled every
/// subsequent write failed silently.
const MAX_CACHE_BYTES: i64 = 4 * 1024 * 1024 * 1024;

/// Subdirectory of the cache root holding one directory per SSH key.
const KEYS_DIR: &str = "keys";

pub struct ImageCache {
    inner: Mutex<Option<Inner>>,
}

struct Inner {
    conn: Connection,
    /// This cache's own directory: `cache.db` and `blobs/`.
    dir: PathBuf,
    blobs_dir: PathBuf,
    max_bytes: i64,
    /// The cache root, when opened through `open_under`: every key's directory
    /// plus whatever the single-cache layout left behind. `clear` empties it.
    root: Option<PathBuf>,
}

/// Why a cache database did not open.
enum OpenError {
    /// The file is not a database under this key: corrupt, or another key's.
    NotADatabase(String),
    /// Locked by a second instance, an I/O error, no permission — nothing that
    /// says the contents are bad.
    Other(String),
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
                crate::log!("image cache disabled: {e}");
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
    fn initialize_under(&self, root: &Path, key: &[u8]) {
        let inner = Self::open_under(root, key).expect("open test cache");
        *self.inner.lock().unwrap() = Some(inner);
    }

    #[cfg(test)]
    fn initialize_at(&self, dir: &Path, key: &[u8]) {
        self.initialize_at_with_cap(dir, key, MAX_CACHE_BYTES);
    }

    #[cfg(test)]
    fn initialize_at_with_cap(&self, dir: &Path, key: &[u8], max_bytes: i64) {
        let key_hex = format!("{:x}", Sha256::digest(key));
        let mut inner = Self::open_at(dir, &key_hex).expect("open test cache");
        inner.max_bytes = max_bytes;
        *self.inner.lock().unwrap() = Some(inner);
    }

    fn try_open(ssh_key_path: &Path) -> Result<Inner, String> {
        let key_bytes = fs::read(ssh_key_path)
            .map_err(|e| format!("failed to read SSH key {}: {e}", ssh_key_path.display()))?;
        let mut root = dirs::cache_dir().ok_or_else(|| "no cache dir available".to_string())?;
        root.push("twelf");
        Self::open_under(&root, &key_bytes)
    }

    /// Open the cache belonging to `key_bytes` under `root`. Each key has a
    /// directory of its own: the database is encrypted with the key, so one
    /// shared database could only ever serve one key, and connecting with
    /// another used to read as corruption and delete the whole cache — every
    /// time the user switched between favorites with different keys.
    fn open_under(root: &Path, key_bytes: &[u8]) -> Result<Inner, String> {
        let key_hex = format!("{:x}", Sha256::digest(key_bytes));
        let dir = root.join(KEYS_DIR).join(key_dir_name(key_bytes));
        Self::adopt_legacy(root, &dir, &key_hex);
        let mut inner = Self::open_at(&dir, &key_hex)?;
        // As the single-cache layout had it; `open_at` covers `dir` itself.
        restrict(root, 0o700);
        inner.root = Some(root.to_path_buf());
        Ok(inner)
    }

    /// Move a cache from the single-cache layout (`cache.db` and `blobs/`
    /// directly under `root`) into `dir`, if this key is the one that opens it.
    /// Another key's is left where it is, for that key to claim. Best-effort: a
    /// cache that cannot be moved is just one that starts empty.
    fn adopt_legacy(root: &Path, dir: &Path, key_hex: &str) {
        let legacy_db = root.join("cache.db");
        if !legacy_db.exists() || dir.join("cache.db").exists() {
            return;
        }
        match Self::open_with_key(&legacy_db, key_hex) {
            Ok(conn) => drop(conn),
            Err(_) => return,
        }
        let moved = fs::create_dir_all(dir)
            .and_then(|()| fs::rename(&legacy_db, dir.join("cache.db")))
            .and_then(|()| fs::rename(root.join("blobs"), dir.join("blobs")));
        if let Err(e) = moved {
            crate::log!("could not adopt the cache in {}: {e}", root.display());
        }
    }

    fn open_at(dir: &Path, key_hex: &str) -> Result<Inner, String> {
        fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
        let blobs_dir = dir.join("blobs");
        fs::create_dir_all(&blobs_dir)
            .map_err(|e| format!("failed to create {}: {e}", blobs_dir.display()))?;
        // The DB is encrypted but the blobs beside it are the image bytes in the
        // clear, and create_dir_all leaves 0755 under the usual umask.
        restrict(dir, 0o700);
        restrict(&blobs_dir, 0o700);
        let db_path = dir.join("cache.db");

        let conn = match Self::open_with_key(&db_path, key_hex) {
            Ok(conn) => conn,
            // In this key's own directory that can only be corruption, and the
            // blobs are indexed by nothing else: start the pair over.
            Err(OpenError::NotADatabase(_)) => {
                let _ = fs::remove_file(&db_path);
                if let Ok(iter) = fs::read_dir(&blobs_dir) {
                    for entry in iter.flatten() {
                        let _ = fs::remove_file(entry.path());
                    }
                }
                Self::open_with_key(&db_path, key_hex).map_err(|e| match e {
                    OpenError::NotADatabase(e) | OpenError::Other(e) => {
                        format!("failed to open encrypted cache after wipe: {e}")
                    }
                })?
            }
            // Any other failure used to wipe too — so a second running instance
            // holding the lock cost the first one its whole cache. Leave the
            // files alone and run without a disk cache this session.
            Err(OpenError::Other(e)) => return Err(e),
        };
        Ok(Inner {
            conn,
            dir: dir.to_path_buf(),
            blobs_dir,
            max_bytes: MAX_CACHE_BYTES,
            root: None,
        })
    }

    fn open_with_key(db_path: &Path, key_hex: &str) -> Result<Connection, OpenError> {
        let other = |what: &str, e: rusqlite::Error| OpenError::Other(format!("{what}: {e}"));
        let conn = Connection::open(db_path)
            .map_err(|e| other(&format!("failed to open {}", db_path.display()), e))?;
        conn.execute_batch(&format!("PRAGMA key = \"x'{key_hex}'\""))
            .map_err(|e| other("failed to set key", e))?;
        // The key is only tested by the first read.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| {
            Ok::<(), rusqlite::Error>(())
        })
        .map_err(|e| {
            if e.sqlite_error_code() == Some(rusqlite::ErrorCode::NotADatabase) {
                OpenError::NotADatabase(format!("decryption check failed: {e}"))
            } else {
                other("decryption check failed", e)
            }
        })?;
        let entries_exists = conn.prepare("SELECT 1 FROM entries LIMIT 0").is_ok();
        let has_fingerprint = conn.prepare("SELECT mtime FROM entries LIMIT 0").is_ok();
        if entries_exists && !has_fingerprint {
            conn.execute("DROP TABLE entries", [])
                .map_err(|e| other("failed to drop outdated table", e))?;
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
        .map_err(|e| other("failed to create table", e))?;
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
            if let Some(m) = mtime
                && stored_mtime != Some(m)
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
                        crate::log!("failed to insert placeholder for {uri}: {e}");
                        return;
                    }
                    (inner.conn.last_insert_rowid(), true)
                }
                Err(e) => {
                    crate::log!("failed to look up cache entry for {uri}: {e}");
                    return;
                }
            };
            (id, is_new, inner.blobs_dir.clone())
        };

        let file_name = rowid.to_string();
        let final_path = blobs_dir.join(&file_name);
        let tmp_path = blobs_dir.join(format!("{file_name}.tmp"));
        let blob_ok = match fs::write(&tmp_path, bytes) {
            Ok(()) => {
                // Owner-only before it is visible under its final name.
                restrict(&tmp_path, 0o600);
                match fs::rename(&tmp_path, &final_path) {
                    Ok(()) => true,
                    Err(e) => {
                        crate::log!("failed to finalize {}: {e}", final_path.display());
                        let _ = fs::remove_file(&tmp_path);
                        false
                    }
                }
            }
            Err(e) => {
                crate::log!("failed to write {}: {e}", tmp_path.display());
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
                Self::evict_over_cap(inner, rowid);
            } else if is_new {
                let _ = inner
                    .conn
                    .execute("DELETE FROM entries WHERE rowid = ?1", params![rowid]);
            }
        }
    }

    /// Drop least-recently-used entries, blob file and row together, until the
    /// total is back under the cap. `keep` is the row just written, which is
    /// exempt so a single oversized file still lands (it is also the newest, so
    /// it would sort last anyway — this only matters when it is the last row).
    /// Runs inside the caller's lock, keeping `get`'s rowid revalidation honest.
    fn evict_over_cap(inner: &Inner, keep: i64) {
        let Some(mut total) = sum_bytes(&inner.conn) else {
            return;
        };
        while total > inner.max_bytes {
            let victim = inner
                .conn
                .query_row(
                    "SELECT rowid, byte_size FROM entries WHERE rowid != ?1
                     ORDER BY last_accessed ASC, rowid ASC LIMIT 1",
                    params![keep],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .ok()
                .flatten();
            let Some((rowid, size)) = victim else { break };
            let _ = fs::remove_file(inner.blobs_dir.join(rowid.to_string()));
            if inner
                .conn
                .execute("DELETE FROM entries WHERE rowid = ?1", params![rowid])
                .is_err()
            {
                break;
            }
            total -= size;
        }
    }

    /// Empty the cache — all of it, not only the open key's share. Whoever
    /// clears a cache wants the disk space or the plaintext copies gone, and
    /// another key's directory can be reached no other way short of connecting
    /// with that key.
    pub fn clear(&self) {
        let (dir, blobs_dir, root) = {
            let Ok(guard) = self.inner.lock() else { return };
            let Some(inner) = guard.as_ref() else { return };
            if let Err(e) = inner.conn.execute("DELETE FROM entries", []) {
                crate::log!("failed to clear cache rows: {e}");
            }
            (
                inner.dir.clone(),
                inner.blobs_dir.clone(),
                inner.root.clone(),
            )
        };
        if let Ok(iter) = fs::read_dir(&blobs_dir) {
            for entry in iter.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let Some(root) = root else { return };
        // What the single-cache layout left behind, if no key ever claimed it.
        let _ = fs::remove_file(root.join("cache.db"));
        let _ = fs::remove_dir_all(root.join("blobs"));
        if let Ok(iter) = fs::read_dir(root.join(KEYS_DIR)) {
            for entry in iter.flatten() {
                if entry.path() != dir {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
    }

    pub fn total_size_bytes(&self) -> u64 {
        let Ok(guard) = self.inner.lock() else {
            return 0;
        };
        let Some(inner) = guard.as_ref() else {
            return 0;
        };
        sum_bytes(&inner.conn).map(|n| n.max(0) as u64).unwrap_or(0)
    }
}

/// Directory name for a key's cache. Hashed under its own label rather than
/// cut from the database key, so the name on disk gives away no part of it.
fn key_dir_name(key_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"twelf cache directory\0");
    hasher.update(key_bytes);
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// Tighten `path` to `mode`. Best-effort: a cache that cannot be locked down is
/// still a working cache, and the platforms without Unix modes have none to set.
#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
        crate::log!("failed to restrict {}: {e}", path.display());
    }
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

fn sum_bytes(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(byte_size), 0) FROM entries",
        [],
        |row| row.get::<_, i64>(0),
    )
    .ok()
}

/// Milliseconds, not seconds: `last_accessed` orders LRU eviction, and a whole
/// burst of images is viewed within one second while browsing — at second
/// granularity they all tie and the tie-break degrades to insertion order.
/// Older second-granularity rows simply sort as very old and are re-stamped on
/// their next hit, so no migration is needed.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh_cache() -> (ImageCache, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let cache = ImageCache::new();
        cache.initialize_at(dir.path(), b"test-key");
        (cache, dir)
    }

    fn under(root: &Path, key: &[u8]) -> ImageCache {
        let cache = ImageCache::new();
        cache.initialize_under(root, key);
        cache
    }

    #[test]
    fn switching_keys_keeps_each_keys_cache() {
        let root = tempdir().expect("tempdir");
        under(root.path(), b"key-a").put("sftp://nas/a.jpg", b"from a", Some(1));
        // Connecting with another key used to delete everything stored so far.
        under(root.path(), b"key-b").put("sftp://work/b.jpg", b"from b", Some(1));
        assert_eq!(
            under(root.path(), b"key-a").get("sftp://nas/a.jpg", Some(1), Some(6)),
            Some(b"from a".to_vec())
        );
        assert_eq!(
            under(root.path(), b"key-b").get("sftp://work/b.jpg", Some(1), Some(6)),
            Some(b"from b".to_vec())
        );
        // One key cannot read the other's index.
        assert_eq!(
            under(root.path(), b"key-b").get("sftp://nas/a.jpg", Some(1), Some(6)),
            None
        );
    }

    #[test]
    fn a_corrupt_database_is_rebuilt_but_an_unopenable_one_is_left_alone() {
        let key_hex = format!("{:x}", Sha256::digest(b"test-key"));

        // Garbage where the database should be: start over, blobs included.
        let corrupt = tempdir().expect("tempdir");
        fs::create_dir_all(corrupt.path().join("blobs")).unwrap();
        fs::write(corrupt.path().join("blobs").join("7"), b"orphan").unwrap();
        fs::write(corrupt.path().join("cache.db"), vec![0x5au8; 4096]).unwrap();
        assert!(ImageCache::open_at(corrupt.path(), &key_hex).is_ok());
        assert!(!corrupt.path().join("blobs").join("7").exists());

        // A database that cannot be opened at all says nothing about what is in
        // the cache: no wipe, just no disk cache this session.
        let blocked = tempdir().expect("tempdir");
        fs::create_dir_all(blocked.path().join("blobs")).unwrap();
        fs::write(blocked.path().join("blobs").join("7"), b"kept").unwrap();
        fs::create_dir(blocked.path().join("cache.db")).unwrap();
        assert!(ImageCache::open_at(blocked.path(), &key_hex).is_err());
        assert!(blocked.path().join("blobs").join("7").exists());
    }

    #[test]
    fn the_single_cache_layout_is_adopted_by_its_own_key_only() {
        let root = tempdir().expect("tempdir");
        // The old layout: one database and blob folder directly under the root.
        let legacy = ImageCache::new();
        legacy.initialize_at(root.path(), b"key-a");
        legacy.put("sftp://nas/a.jpg", b"hello", Some(1));
        drop(legacy);

        // Another key leaves it where it is…
        assert_eq!(
            under(root.path(), b"key-b").get("sftp://nas/a.jpg", Some(1), Some(5)),
            None
        );
        assert!(root.path().join("cache.db").exists());
        // …and the key that wrote it takes it along, contents intact.
        assert_eq!(
            under(root.path(), b"key-a").get("sftp://nas/a.jpg", Some(1), Some(5)),
            Some(b"hello".to_vec())
        );
        assert!(!root.path().join("cache.db").exists());
        assert!(!root.path().join("blobs").exists());
    }

    #[test]
    fn clear_empties_every_keys_cache() {
        let root = tempdir().expect("tempdir");
        let legacy = ImageCache::new();
        legacy.initialize_at(root.path(), b"key-gone");
        legacy.put("sftp://old/x.jpg", b"stale", Some(1));
        drop(legacy);
        under(root.path(), b"key-a").put("sftp://nas/a.jpg", b"from a", Some(1));

        let current = under(root.path(), b"key-b");
        current.put("sftp://work/b.jpg", b"from b", Some(1));
        current.clear();

        assert_eq!(current.total_size_bytes(), 0);
        assert_eq!(current.get("sftp://work/b.jpg", Some(1), Some(6)), None);
        assert_eq!(
            under(root.path(), b"key-a").get("sftp://nas/a.jpg", Some(1), Some(6)),
            None
        );
        assert!(!root.path().join("cache.db").exists());
        assert!(!root.path().join("blobs").exists());
        // The open cache keeps working after the clear.
        current.put("sftp://work/c.jpg", b"again", Some(1));
        assert_eq!(
            current.get("sftp://work/c.jpg", Some(1), Some(5)),
            Some(b"again".to_vec())
        );
    }

    #[test]
    fn matching_fingerprint_returns_bytes() {
        let (cache, _dir) = fresh_cache();
        cache.put("sftp://host/a.jpg", b"hello", Some(100));
        assert_eq!(
            cache.get("sftp://host/a.jpg", Some(100), Some(5)),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn changed_mtime_is_a_miss() {
        let (cache, _dir) = fresh_cache();
        cache.put("sftp://host/a.jpg", b"hello", Some(100));
        assert_eq!(cache.get("sftp://host/a.jpg", Some(200), Some(5)), None);
    }

    #[test]
    fn changed_size_is_a_miss() {
        let (cache, _dir) = fresh_cache();
        cache.put("sftp://host/a.jpg", b"hello", Some(100));
        assert_eq!(cache.get("sftp://host/a.jpg", Some(100), Some(4)), None);
    }

    #[test]
    fn absent_mtime_falls_back_to_size_match() {
        let (cache, _dir) = fresh_cache();
        cache.put("sftp://host/a.jpg", b"hello", Some(100));
        assert_eq!(
            cache.get("sftp://host/a.jpg", None, Some(5)),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn null_mtime_entry_is_revalidated() {
        let (cache, _dir) = fresh_cache();
        // Stored during a stat outage: no mtime recorded.
        cache.put("sftp://host/a.jpg", b"hello", None);
        // Once a real mtime is known, the unconfirmable entry must miss.
        assert_eq!(cache.get("sftp://host/a.jpg", Some(100), Some(5)), None);
        // A repair put with the real mtime restores the hit.
        cache.put("sftp://host/a.jpg", b"hello", Some(100));
        assert_eq!(
            cache.get("sftp://host/a.jpg", Some(100), Some(5)),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn eviction_drops_least_recently_used_past_the_cap() {
        let dir = tempdir().expect("tempdir");
        let cache = ImageCache::new();
        cache.initialize_at_with_cap(dir.path(), b"test-key", 20);

        // Separated so the recency stamps are distinct rather than tied — the
        // ordering under test is the point, not the clock's resolution.
        let tick = || std::thread::sleep(std::time::Duration::from_millis(2));
        cache.put("sftp://h/a.jpg", &[0u8; 8], Some(1));
        tick();
        cache.put("sftp://h/b.jpg", &[0u8; 8], Some(1));
        tick();
        // Touch a so b is the least recently used.
        assert!(cache.get("sftp://h/a.jpg", Some(1), Some(8)).is_some());
        tick();

        // 24 > 20, so one entry has to go, and it must be b.
        cache.put("sftp://h/c.jpg", &[0u8; 8], Some(1));
        assert!(cache.get("sftp://h/b.jpg", Some(1), Some(8)).is_none());
        assert!(cache.get("sftp://h/a.jpg", Some(1), Some(8)).is_some());
        assert!(cache.get("sftp://h/c.jpg", Some(1), Some(8)).is_some());
        assert!(cache.total_size_bytes() <= 20);

        // The blob file goes with the row, or the cap would only be nominal.
        let blobs = std::fs::read_dir(dir.path().join("blobs"))
            .expect("blobs dir")
            .filter_map(Result::ok)
            .count();
        assert_eq!(blobs, 2);
    }

    #[test]
    fn an_entry_larger_than_the_cap_is_still_stored() {
        let dir = tempdir().expect("tempdir");
        let cache = ImageCache::new();
        cache.initialize_at_with_cap(dir.path(), b"test-key", 10);
        cache.put("sftp://h/big.jpg", &[0u8; 64], Some(1));
        // Evicting everything else cannot get under the cap; the newcomer stays
        // rather than being deleted immediately after being written.
        assert_eq!(
            cache.get("sftp://h/big.jpg", Some(1), Some(64)),
            Some(vec![0u8; 64])
        );
    }

    #[test]
    fn larger_requested_size_is_a_miss() {
        let (cache, _dir) = fresh_cache();
        cache.put("sftp://host/a.jpg", b"hello", Some(100));
        // A stat reporting a size larger than the stored blob must also miss —
        // any size difference invalidates, not only a smaller one.
        assert_eq!(cache.get("sftp://host/a.jpg", Some(100), Some(6)), None);
    }

    #[test]
    fn a_cache_filled_exactly_to_the_cap_evicts_nothing() {
        let dir = tempdir().expect("tempdir");
        let cache = ImageCache::new();
        cache.initialize_at_with_cap(dir.path(), b"test-key", 16);
        cache.put("sftp://h/a.jpg", &[0u8; 8], Some(1));
        cache.put("sftp://h/b.jpg", &[0u8; 8], Some(1));
        // Sitting exactly on the cap is not being over it.
        assert!(cache.get("sftp://h/a.jpg", Some(1), Some(8)).is_some());
        assert!(cache.get("sftp://h/b.jpg", Some(1), Some(8)).is_some());
        assert_eq!(cache.total_size_bytes(), 16);
    }
}
