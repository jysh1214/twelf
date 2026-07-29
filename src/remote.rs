use crate::sidebar;
use eframe::egui;
use futures::future::join_all;
use russh_sftp::client::SftpSession;
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::Sender;

pub struct RemoteTreeNode {
    path: PathBuf,
    name: String,
    kind: RemoteNodeKind,
}

enum RemoteNodeKind {
    File,
    Dir { children: RemoteDirChildren },
}

enum RemoteDirChildren {
    Unloaded,
    Loading,
    Loaded(Vec<RemoteTreeNode>),
    Error(String),
}

pub type ListingResult = (PathBuf, Result<Vec<RemoteTreeNode>, String>);
/// One successfully re-listed directory from a poll cycle (errors are skipped).
pub type PollResult = (PathBuf, Vec<RemoteTreeNode>);

impl RemoteTreeNode {
    pub fn root(path: PathBuf) -> Self {
        let name = path.display().to_string();
        Self {
            path,
            name,
            kind: RemoteNodeKind::Dir {
                children: RemoteDirChildren::Unloaded,
            },
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn child(path: PathBuf, name: String, is_dir: bool) -> Self {
        let kind = if is_dir {
            RemoteNodeKind::Dir {
                children: RemoteDirChildren::Unloaded,
            }
        } else {
            RemoteNodeKind::File
        };
        Self { path, name, kind }
    }

    /// Walk the loaded subtree depth-first and collect every media file's full
    /// path (image or video). `Unloaded`, `Loading`, or `Error` folders
    /// contribute nothing.
    pub fn collect_images(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        self.collect_images_into(&mut out);
        out
    }

    fn collect_images_into(&self, out: &mut Vec<PathBuf>) {
        match &self.kind {
            RemoteNodeKind::File => out.push(self.path.clone()),
            RemoteNodeKind::Dir {
                children: RemoteDirChildren::Loaded(children),
            } => {
                for child in children {
                    child.collect_images_into(out);
                }
            }
            _ => {}
        }
    }

    pub fn apply_listing(
        &mut self,
        target: &Path,
        result: Result<Vec<RemoteTreeNode>, String>,
    ) -> bool {
        if self.path == target {
            if let RemoteNodeKind::Dir { children } = &mut self.kind {
                *children = match result {
                    Ok(c) => RemoteDirChildren::Loaded(c),
                    Err(e) => RemoteDirChildren::Error(e),
                };
                return true;
            }
            return false;
        }
        if !target.starts_with(&self.path) {
            return false;
        }
        // Recurse along the single child whose path prefixes `target`.
        if let RemoteNodeKind::Dir {
            children: RemoteDirChildren::Loaded(c),
        } = &mut self.kind
        {
            for child in c {
                if target.starts_with(&child.path) {
                    return child.apply_listing(target, result);
                }
            }
        }
        false
    }

    /// Mark the directory at `target` `Unloaded` so the next render re-lists it.
    /// Used to refresh a folder after one of its entries is deleted.
    pub fn reload(&mut self, target: &Path) -> bool {
        if self.path == target {
            if let RemoteNodeKind::Dir { children } = &mut self.kind {
                *children = RemoteDirChildren::Unloaded;
                return true;
            }
            return false;
        }
        if !target.starts_with(&self.path) {
            return false;
        }
        if let RemoteNodeKind::Dir {
            children: RemoteDirChildren::Loaded(c),
        } = &mut self.kind
        {
            for child in c {
                if target.starts_with(&child.path) && child.reload(target) {
                    return true;
                }
            }
        }
        false
    }

    fn is_dir(&self) -> bool {
        matches!(self.kind, RemoteNodeKind::Dir { .. })
    }

    /// Paths of every directory whose listing is currently `Loaded`, root
    /// included — the set a poll cycle re-lists. `Unloaded`/`Loading`/`Error`
    /// folders were never opened (or are in flight) and are skipped.
    pub fn loaded_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        self.loaded_dirs_into(&mut out);
        out
    }

    fn loaded_dirs_into(&self, out: &mut Vec<PathBuf>) {
        if let RemoteNodeKind::Dir {
            children: RemoteDirChildren::Loaded(children),
        } = &self.kind
        {
            out.push(self.path.clone());
            for child in children {
                child.loaded_dirs_into(out);
            }
        }
    }

    /// Merge a freshly polled listing for `target` into the tree without
    /// disturbing surviving entries: an existing child with the same path and
    /// dir-ness is kept (preserving its loaded subtree), new entries are
    /// inserted and vanished ones drop out, all in the new listing's order.
    /// A no-op unless `target` is currently `Loaded` — an in-flight Refresh
    /// or expansion owns the node then, and its own result is fresher.
    pub fn merge_listing(&mut self, target: &Path, new_children: Vec<RemoteTreeNode>) -> bool {
        if self.path == target {
            if let RemoteNodeKind::Dir {
                children: RemoteDirChildren::Loaded(children),
            } = &mut self.kind
            {
                let old = std::mem::take(children);
                *children = merge_children(old, new_children);
                return true;
            }
            return false;
        }
        if !target.starts_with(&self.path) {
            return false;
        }
        if let RemoteNodeKind::Dir {
            children: RemoteDirChildren::Loaded(c),
        } = &mut self.kind
        {
            for child in c {
                if target.starts_with(&child.path) {
                    return child.merge_listing(target, new_children);
                }
            }
        }
        false
    }
}

/// Membership and order come from the new listing; a surviving entry keeps its
/// old node (and so its state) unless its kind flipped between file and dir.
fn merge_children(old: Vec<RemoteTreeNode>, new: Vec<RemoteTreeNode>) -> Vec<RemoteTreeNode> {
    let mut old_by_path: HashMap<PathBuf, RemoteTreeNode> =
        old.into_iter().map(|n| (n.path.clone(), n)).collect();
    new.into_iter()
        .map(|n| match old_by_path.remove(&n.path) {
            Some(o) if o.is_dir() == n.is_dir() => o,
            _ => n,
        })
        .collect()
}

/// Join a server-supplied entry name onto `dir`, rejecting anything that is not
/// a single ordinary component. `PathBuf::push` normalises nothing: `../../x`
/// escapes the subtree and an absolute name replaces the path outright, so an
/// unchecked join lets a hostile or compromised server steer a download's writes
/// — or a recursive delete's removals — outside the directory being walked.
/// russh-sftp filters only the exact strings "." and "..", nothing containing a
/// separator.
fn child_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut components = Path::new(name).components();
    let Some(Component::Normal(single)) = components.next() else {
        return None;
    };
    if components.next().is_some() {
        return None;
    }
    Some(dir.join(single))
}

async fn list_remote_children(
    sftp: &SftpSession,
    path: &Path,
) -> Result<Vec<RemoteTreeNode>, String> {
    let path_str = path.to_string_lossy().into_owned();
    let entries = sftp.read_dir(path_str).await.map_err(|e| e.to_string())?;
    let mut nodes: Vec<RemoteTreeNode> = entries
        .filter_map(|entry| {
            let name = entry.file_name();
            let is_dir = entry.metadata().is_dir();
            let Some(child_path) = child_path(path, &name) else {
                crate::log!("skipping entry with unsafe name {name:?} in {}", path.display());
                return None;
            };
            if is_dir
                || sidebar::is_image(&child_path)
                || crate::video::is_video(&child_path.to_string_lossy())
            {
                Some(RemoteTreeNode::child(child_path, name, is_dir))
            } else {
                None
            }
        })
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(nodes)
}

const REMOTE_SEARCH_MAX_DEPTH: usize = 64;
/// Max concurrent in-flight SFTP read_dirs during a search walk. russh-sftp
/// pipelines requests by id, so this overlaps their round-trips.
const REMOTE_SEARCH_CONCURRENCY: usize = 8;
/// Max concurrent in-flight SFTP ops (read_dir + file reads) during a download.
const REMOTE_DOWNLOAD_CONCURRENCY: usize = 8;

/// An in-flight recursive remote search. The walk runs off-thread and sends one
/// final pruned result back. The handle owns the result channel, so a superseded
/// walk's late send lands on a dropped receiver and is discarded; dropping the
/// handle also flips `cancel`, stopping the walk's read_dir loop early.
pub struct RemoteSearchWalk {
    query: String,
    cancel: Arc<AtomicBool>,
    rx: std::sync::mpsc::Receiver<Vec<sidebar::SearchHit>>,
    hits: Option<Vec<sidebar::SearchHit>>,
}

impl RemoteSearchWalk {
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Pull the completed result into the handle once it arrives (non-blocking).
    pub fn poll(&mut self) {
        if self.hits.is_none()
            && let Ok(hits) = self.rx.try_recv()
        {
            self.hits = Some(hits);
        }
    }

    pub fn hits(&self) -> Option<&[sidebar::SearchHit]> {
        self.hits.as_deref()
    }
}

impl Drop for RemoteSearchWalk {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Spawn a recursive remote name-search under `root` on the runtime. Cancel by
/// dropping the returned handle (see `Drop`).
pub fn spawn_remote_search(
    sftp: Arc<SftpSession>,
    runtime: &tokio::runtime::Runtime,
    root: PathBuf,
    query: String,
    ctx: &egui::Context,
) -> RemoteSearchWalk {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel_task = cancel.clone();
    let query_lc = query.to_lowercase();
    let ctx = ctx.clone();
    runtime.spawn(async move {
        let sem = Semaphore::new(REMOTE_SEARCH_CONCURRENCY);
        let hits = search_remote_dir(&sftp, &root, &query_lc, &cancel_task, &sem, 0, false).await;
        let _ = tx.send(hits);
        ctx.request_repaint();
    });
    RemoteSearchWalk { query, cancel, rx, hits: None }
}

/// Recursively walk `dir` over SFTP, keeping entries whose name contains
/// `query_lc` plus the ancestor folders that lead to a match; a matched
/// folder keeps its full contents (`keep_all` below it), mirroring the local
/// walk. Empty on a read error (silent-skip, like the local walk), once
/// `cancel` is set, or past the depth cap (which bounds a symlink loop
/// without a per-dir round-trip).
async fn search_remote_dir(
    sftp: &SftpSession,
    dir: &Path,
    query_lc: &str,
    cancel: &AtomicBool,
    sem: &Semaphore,
    depth: usize,
    keep_all: bool,
) -> Vec<sidebar::SearchHit> {
    if depth > REMOTE_SEARCH_MAX_DEPTH || cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let nodes = {
        // Hold a permit only for the read_dir round-trip, never across recursion —
        // a parent waiting on its children would otherwise deadlock the permit pool.
        let _permit = sem.acquire().await.expect("search semaphore never closed");
        match list_remote_children(sftp, dir).await {
            Ok(nodes) => nodes,
            Err(_) => return Vec::new(),
        }
    };
    if cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    // Recurse into children concurrently and in order; the semaphore caps the
    // actual in-flight read_dirs, which russh-sftp pipelines over the one channel.
    let children = nodes.into_iter().map(|node| {
        Box::pin(async move {
            let RemoteTreeNode { path, name, kind } = node;
            let matches = name.to_lowercase().contains(query_lc);
            match kind {
                RemoteNodeKind::File => {
                    (matches || keep_all).then(|| sidebar::SearchHit::file(path, name))
                }
                RemoteNodeKind::Dir { .. } => {
                    let children = search_remote_dir(
                        sftp,
                        &path,
                        query_lc,
                        cancel,
                        sem,
                        depth + 1,
                        keep_all || matches,
                    )
                    .await;
                    sidebar::SearchHit::dir(path, name, matches, keep_all, children)
                }
            }
        })
    });
    join_all(children).await.into_iter().flatten().collect()
}

/// Max concurrent in-flight read_dirs during a poll cycle — gentler than the
/// search walk, this is background traffic.
const REMOTE_POLL_CONCURRENCY: usize = 4;

/// Spawn one poll cycle: re-list every directory in `dirs`, sending each
/// successful listing (errors are silently skipped — a transient failure must
/// not disturb the tree) for the UI loop to merge. `running` is flipped false
/// when the whole cycle is done, gating the next one.
pub fn spawn_remote_poll(
    sftp: Arc<SftpSession>,
    runtime: &tokio::runtime::Runtime,
    dirs: Vec<PathBuf>,
    tx: Sender<PollResult>,
    running: Arc<AtomicBool>,
    ctx: &egui::Context,
) {
    let ctx = ctx.clone();
    runtime.spawn(async move {
        let sem = Semaphore::new(REMOTE_POLL_CONCURRENCY);
        let listings = dirs.into_iter().map(|dir| {
            let sftp = &sftp;
            let sem = &sem;
            let tx = tx.clone();
            let ctx = ctx.clone();
            async move {
                // No recursion here, so holding the permit across the whole
                // read_dir round-trip cannot deadlock the pool.
                let _permit = sem.acquire().await.expect("poll semaphore never closed");
                if let Ok(nodes) = list_remote_children(sftp, &dir).await {
                    let _ = tx.send((dir, nodes)).await;
                    ctx.request_repaint();
                }
            }
        });
        join_all(listings).await;
        running.store(false, Ordering::Relaxed);
    });
}

/// Live counters for an in-flight download, shared with the walk task.
#[derive(Default)]
struct DownloadProgress {
    files: AtomicUsize,
    bytes: AtomicU64,
    errors: AtomicUsize,
    /// Files left untouched because a local copy already existed.
    skipped: AtomicUsize,
}

/// An in-flight recursive folder download. Like `RemoteSearchWalk`, the walk
/// runs off-thread and dropping the handle flips `cancel` to stop it. The
/// counters are read live each frame; `rx` fires once when the walk finishes.
pub struct RemoteDownload {
    target: PathBuf,
    cancel: Arc<AtomicBool>,
    progress: Arc<DownloadProgress>,
    rx: std::sync::mpsc::Receiver<()>,
    finished: bool,
}

impl RemoteDownload {
    /// Note completion once the walk signals it (non-blocking).
    pub fn poll(&mut self) {
        if !self.finished && self.rx.try_recv().is_ok() {
            self.finished = true;
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn files(&self) -> usize {
        self.progress.files.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.progress.bytes.load(Ordering::Relaxed)
    }

    pub fn errors(&self) -> usize {
        self.progress.errors.load(Ordering::Relaxed)
    }

    pub fn skipped(&self) -> usize {
        self.progress.skipped.load(Ordering::Relaxed)
    }

    /// Local folder the remote tree is copied into (`<dest>/<folder name>`).
    pub fn target(&self) -> &Path {
        &self.target
    }
}

impl Drop for RemoteDownload {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Spawn a recursive download of `root` into `dest` on the runtime. Every file
/// under `root` is copied to `<dest>/<root name>/…`, preserving structure.
/// Cancel by dropping the returned handle (see `Drop`).
pub fn spawn_remote_download(
    sftp: Arc<SftpSession>,
    runtime: &tokio::runtime::Runtime,
    root: PathBuf,
    dest: PathBuf,
    ctx: &egui::Context,
) -> RemoteDownload {
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(DownloadProgress::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel_task = cancel.clone();
    let progress_task = progress.clone();
    let ctx_task = ctx.clone();
    let dest_task = dest.clone();
    let target = match root.file_name() {
        Some(name) => dest.join(name),
        None => dest,
    };
    runtime.spawn(async move {
        let sem = Semaphore::new(REMOTE_DOWNLOAD_CONCURRENCY);
        download_remote_dir(
            &sftp,
            &root,
            &root,
            &dest_task,
            &cancel_task,
            &sem,
            &progress_task,
            0,
        )
        .await;
        let _ = tx.send(());
        ctx_task.request_repaint();
    });
    RemoteDownload { target, cancel, progress, rx, finished: false }
}

/// Recursively copy `dir` (under `root`) to the local destination. Unfiltered —
/// every file is fetched, unlike the media-only tree listing. Silent-skips a
/// read_dir error and stops on cancel or past the depth cap, like the search walk.
async fn download_remote_dir(
    sftp: &SftpSession,
    root: &Path,
    dir: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    sem: &Semaphore,
    progress: &DownloadProgress,
    depth: usize,
) {
    if depth > REMOTE_SEARCH_MAX_DEPTH || cancel.load(Ordering::Relaxed) {
        return;
    }
    let entries = {
        // Hold a permit only for the round-trip, never across recursion — see
        // search_remote_dir for the permit-pool deadlock this avoids.
        let _permit = sem.acquire().await.expect("download semaphore never closed");
        match sftp.read_dir(dir.to_string_lossy().into_owned()).await {
            Ok(entries) => entries,
            Err(_) => return,
        }
    };
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let children = entries.filter_map(|entry| {
        let is_dir = entry.metadata().is_dir();
        // A name that would escape `dir` is counted as a failure, not silently
        // dropped: the copy is then visibly incomplete rather than quietly so.
        let Some(child) = child_path(dir, &entry.file_name()) else {
            crate::log!("refusing entry with unsafe name {:?}", entry.file_name());
            progress.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        Some(Box::pin(async move {
            if is_dir {
                download_remote_dir(sftp, root, &child, dest, cancel, sem, progress, depth + 1)
                    .await;
            } else {
                let local = local_target(dest, root, &child);
                download_file(sftp, &child, &local, false, cancel, sem, progress).await;
            }
        }))
    });
    join_all(children).await;
}

/// Read size for the streaming copy. Large enough that a big file is not a
/// round-trip storm, small enough that eight concurrent transfers hold a
/// negligible amount of memory.
const DOWNLOAD_CHUNK: usize = 256 * 1024;

/// How one file's transfer ended.
enum FileOutcome {
    Written,
    /// A local copy existed and `overwrite` was not set.
    Skipped,
    /// The whole download was cancelled part-way.
    Cancelled,
}

/// Fetch one remote file into `local`. With `overwrite` false an existing local
/// file is left alone and counted as skipped: the folder walk picks its
/// destination with a directory picker, which carries no overwrite consent, so
/// truncating the user's own copies there is data loss. The single-file path
/// goes through a save dialog, which does ask. A read or write failure is
/// counted but does not abort the rest of the walk.
async fn download_file(
    sftp: &SftpSession,
    remote_file: &Path,
    local: &Path,
    overwrite: bool,
    cancel: &AtomicBool,
    sem: &Semaphore,
    progress: &DownloadProgress,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    // Checked before the transfer so a re-run costs nothing for what is already
    // on disk; checked again before the rename, once the bytes are here.
    if !may_write(local, overwrite) {
        progress.skipped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // The permit covers the whole transfer: the file is streamed, so it stays
    // in flight until its last chunk rather than for a single request.
    let _permit = sem.acquire().await.expect("download semaphore never closed");
    match stream_to_file(sftp, remote_file, local, overwrite, cancel, progress).await {
        Ok(FileOutcome::Written) => {
            progress.files.fetch_add(1, Ordering::Relaxed);
        }
        Ok(FileOutcome::Skipped) => {
            progress.skipped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(FileOutcome::Cancelled) => {}
        Err(_) => {
            progress.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Copy `remote_file` to `local` a chunk at a time through a `.part` sibling,
/// renamed into place only once the whole file has arrived.
///
/// Streaming is what keeps a multi-GB video from being materialised in memory:
/// `SftpSession::read` is open + read_to_end into one `Vec`, and the walk runs
/// several of those at once. The `.part` staging means an interrupted transfer
/// leaves nothing a later run could mistake for a complete copy, and `bytes`
/// advances per chunk instead of only when a whole file lands.
async fn stream_to_file(
    sftp: &SftpSession,
    remote_file: &Path,
    local: &Path,
    overwrite: bool,
    cancel: &AtomicBool,
    progress: &DownloadProgress,
) -> std::io::Result<FileOutcome> {
    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut remote = sftp
        .open(remote_file.to_string_lossy().into_owned())
        .await
        .map_err(std::io::Error::other)?;
    let part = part_path(local);
    let mut out = tokio::fs::File::create(&part).await?;
    let mut buf = vec![0u8; DOWNLOAD_CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(out);
            let _ = tokio::fs::remove_file(&part).await;
            return Ok(FileOutcome::Cancelled);
        }
        let read = remote.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        out.write_all(&buf[..read]).await?;
        progress.bytes.fetch_add(read as u64, Ordering::Relaxed);
    }
    out.flush().await?;
    drop(out);
    // Re-checked now the transfer is done: the destination may have appeared
    // while it ran, and the user's copy still wins.
    if !may_write(local, overwrite) {
        let _ = tokio::fs::remove_file(&part).await;
        return Ok(FileOutcome::Skipped);
    }
    tokio::fs::rename(&part, local).await?;
    Ok(FileOutcome::Written)
}

/// Whether a transfer may land on `local`. An existing file is the user's own
/// copy unless they authorised replacing it through a save dialog.
fn may_write(local: &Path, overwrite: bool) -> bool {
    overwrite || !local.exists()
}

/// Staging path a transfer writes to before being renamed into place.
fn part_path(local: &Path) -> PathBuf {
    let mut name = local.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// Spawn a download of the single remote file `remote` to the exact local path
/// `target` (chosen in a save dialog; the recursive folder variant is
/// `spawn_remote_download`). Cancel by dropping the returned handle.
pub fn spawn_remote_file_download(
    sftp: Arc<SftpSession>,
    runtime: &tokio::runtime::Runtime,
    remote: PathBuf,
    target: PathBuf,
    ctx: &egui::Context,
) -> RemoteDownload {
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(DownloadProgress::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel_task = cancel.clone();
    let progress_task = progress.clone();
    let ctx_task = ctx.clone();
    let target_task = target.clone();
    runtime.spawn(async move {
        let sem = Semaphore::new(1);
        // The save dialog already asked about an existing file.
        download_file(&sftp, &remote, &target_task, true, &cancel_task, &sem, &progress_task).await;
        let _ = tx.send(());
        ctx_task.request_repaint();
    });
    RemoteDownload { target, cancel, progress, rx, finished: false }
}

/// Local path a remote file lands at: `<dest>/<root name>/<path relative to root>`.
fn local_target(dest: &Path, root: &Path, remote_file: &Path) -> PathBuf {
    let mut out = dest.to_path_buf();
    if let Some(name) = root.file_name() {
        out.push(name);
    }
    if let Ok(rel) = remote_file.strip_prefix(root) {
        out.push(rel);
    }
    out
}


/// Max concurrent in-flight SFTP ops while enumerating a delete target.
const REMOTE_DELETE_CONCURRENCY: usize = 8;

/// An in-flight recursive delete. Like `RemoteDownload`, the walk runs off-thread
/// and dropping the handle flips `cancel`; `rx` fires once when it finishes and
/// `failed` counts entries that could not be removed.
pub struct RemoteDelete {
    target: PathBuf,
    cancel: Arc<AtomicBool>,
    failed: Arc<AtomicUsize>,
    rx: std::sync::mpsc::Receiver<()>,
    finished: bool,
}

impl RemoteDelete {
    pub fn poll(&mut self) {
        if !self.finished && self.rx.try_recv().is_ok() {
            self.finished = true;
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn failed(&self) -> usize {
        self.failed.load(Ordering::Relaxed)
    }

    /// The path being deleted — for the status label and the post-delete refresh.
    pub fn target(&self) -> &Path {
        &self.target
    }
}

impl Drop for RemoteDelete {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Spawn a recursive delete of `target` on the runtime. A directory is enumerated
/// in full and then removed deepest-first, so each dir is empty when removed
/// (SFTP `remove_dir` only deletes empty dirs). Cancel by dropping the handle.
pub fn spawn_remote_delete(
    sftp: Arc<SftpSession>,
    runtime: &tokio::runtime::Runtime,
    target: PathBuf,
    is_dir: bool,
    ctx: &egui::Context,
) -> RemoteDelete {
    let cancel = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel_task = cancel.clone();
    let failed_task = failed.clone();
    let ctx_task = ctx.clone();
    let target_task = target.clone();
    runtime.spawn(async move {
        let sem = Semaphore::new(REMOTE_DELETE_CONCURRENCY);
        if is_dir {
            let mut entries =
                collect_remote_paths(&sftp, &target_task, &cancel_task, &sem, 0).await;
            entries.push((target_task.clone(), true));
            for (path, path_is_dir) in deletion_order(entries) {
                if cancel_task.load(Ordering::Relaxed) {
                    break;
                }
                let path_str = path.to_string_lossy().into_owned();
                let res = if path_is_dir {
                    sftp.remove_dir(path_str).await
                } else {
                    sftp.remove_file(path_str).await
                };
                if res.is_err() {
                    failed_task.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else if sftp
            .remove_file(target_task.to_string_lossy().into_owned())
            .await
            .is_err()
        {
            failed_task.fetch_add(1, Ordering::Relaxed);
        }
        let _ = tx.send(());
        ctx_task.request_repaint();
    });
    RemoteDelete { target, cancel, failed, rx, finished: false }
}

/// Recursively list every path under `dir` (files and subdirectories, excluding
/// `dir` itself) as `(path, is_dir)`. Concurrent read_dirs bounded by `sem`, the
/// permit released before recursing — the same deadlock-avoidance as the search
/// walk. A read error under one branch silently contributes nothing.
async fn collect_remote_paths(
    sftp: &SftpSession,
    dir: &Path,
    cancel: &AtomicBool,
    sem: &Semaphore,
    depth: usize,
) -> Vec<(PathBuf, bool)> {
    if depth > REMOTE_SEARCH_MAX_DEPTH || cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let entries = {
        let _permit = sem.acquire().await.expect("delete semaphore never closed");
        match sftp.read_dir(dir.to_string_lossy().into_owned()).await {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        }
    };
    let children = entries.filter_map(|entry| {
        let is_dir = entry.metadata().is_dir();
        // Skipping leaves the entry in place, so the enclosing remove_dir fails
        // and the delete is reported as partial rather than reaching outside.
        let Some(child) = child_path(dir, &entry.file_name()) else {
            crate::log!("refusing to delete entry with unsafe name {:?}", entry.file_name());
            return None;
        };
        Some(Box::pin(async move {
            let mut out = Vec::new();
            if is_dir {
                out.extend(collect_remote_paths(sftp, &child, cancel, sem, depth + 1).await);
            }
            out.push((child, is_dir));
            out
        }))
    });
    join_all(children).await.into_iter().flatten().collect()
}

/// Order paths so each precedes its ancestors: deepest (most components) first.
/// Deleting in this order keeps every directory empty when it is removed.
fn deletion_order(mut entries: Vec<(PathBuf, bool)>) -> Vec<(PathBuf, bool)> {
    entries.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    entries
}

/// An in-flight one-shot remote rename. Far simpler than `RemoteDelete` — a
/// single `SftpSession::rename` round-trip, no walk and no cancel — so it just
/// carries the target path and a channel that fires once with Ok or the error.
pub struct RemoteRename {
    target: PathBuf,
    /// The path `target` becomes, captured at spawn. The completion handler must
    /// use this rather than reading back whatever dialog is open by then — they
    /// are not necessarily about the same file.
    renamed: PathBuf,
    rx: std::sync::mpsc::Receiver<Result<(), String>>,
    result: Option<Result<(), String>>,
}

impl RemoteRename {
    /// Pull the completed result into the handle once it arrives (non-blocking).
    pub fn poll(&mut self) {
        if self.result.is_none()
            && let Ok(res) = self.rx.try_recv()
        {
            self.result = Some(res);
        }
    }

    pub fn is_finished(&self) -> bool {
        self.result.is_some()
    }

    /// The completed result, if it has arrived.
    pub fn result(&self) -> Option<&Result<(), String>> {
        self.result.as_ref()
    }

    /// The path being renamed — for the status label and the parent refresh.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// The path it becomes, for following the selection once it succeeds.
    pub fn renamed(&self) -> &Path {
        &self.renamed
    }
}

/// Spawn a single SFTP rename of `old` to `new` on the runtime. The result (Ok or
/// the server's error string) is sent once and read from the handle via `poll`.
pub fn spawn_remote_rename(
    sftp: Arc<SftpSession>,
    runtime: &tokio::runtime::Runtime,
    old: PathBuf,
    new: PathBuf,
    ctx: &egui::Context,
) -> RemoteRename {
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx_task = ctx.clone();
    let old_str = old.to_string_lossy().into_owned();
    let new_str = new.to_string_lossy().into_owned();
    runtime.spawn(async move {
        let res = sftp.rename(old_str, new_str).await.map_err(|e| e.to_string());
        let _ = tx.send(res);
        ctx_task.request_repaint();
    });
    RemoteRename { target: old, renamed: new, rx, result: None }
}

/// URIs the Load action prefetches: every image under the loaded children, as
/// `sftp://{host}{path}`. Videos (and anything else non-image) are skipped —
/// the image pipeline would download them whole only to fail decoding.
fn image_prefetch_uris(children: &[RemoteTreeNode], host: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for child in children {
        child.collect_images_into(&mut paths);
    }
    paths
        .into_iter()
        .filter(|path| sidebar::is_image(path))
        .map(|path| format!("sftp://{host}{}", path.display()))
        .collect()
}

pub fn render_remote_tree(
    ui: &mut egui::Ui,
    node: &mut RemoteTreeNode,
    is_root: bool,
    host: &str,
    selected_remote: &mut Option<PathBuf>,
    scroll_target: &mut Option<PathBuf>,
    prefetch: &mut VecDeque<String>,
    download_request: &mut Option<(PathBuf, bool)>,
    delete_request: &mut Option<(PathBuf, bool)>,
    rename_request: &mut Option<(PathBuf, bool)>,
    refresh_request: &mut Option<PathBuf>,
    favorite_request: &mut Option<PathBuf>,
    sftp: &Arc<SftpSession>,
    tx: &Sender<ListingResult>,
    runtime: &tokio::runtime::Runtime,
    ctx: &egui::Context,
) {
    match &mut node.kind {
        RemoteNodeKind::File => {
            let is_selected = selected_remote.as_deref() == Some(node.path.as_path());
            let response = ui.selectable_label(is_selected, &node.name);
            if scroll_target.as_deref() == Some(node.path.as_path()) {
                sidebar::scroll_row_into_view(ui, &response);
                *scroll_target = None;
            }
            if response.clicked() {
                *selected_remote = Some(node.path.clone());
            }
            response.context_menu(|ui| {
                if ui.button("Download").clicked() {
                    *download_request = Some((node.path.clone(), false));
                    ui.close();
                }
                if ui.button("Rename").clicked() {
                    *rename_request = Some((node.path.clone(), false));
                    ui.close();
                }
                if ui.button("Delete").clicked() {
                    *delete_request = Some((node.path.clone(), false));
                    ui.close();
                }
            });
        }
        RemoteNodeKind::Dir { children } => {
            let path = node.path.clone();
            let force_open = scroll_target
                .as_deref()
                .is_some_and(|t| t.starts_with(&node.path));
            let mut header = egui::CollapsingHeader::new(&node.name)
                .id_salt(&node.path)
                .default_open(is_root);
            if force_open {
                header = header.open(Some(true));
            }
            let collapsing = header.show(ui, |ui| match children {
                RemoteDirChildren::Unloaded => {
                    *children = RemoteDirChildren::Loading;
                    let sftp_clone = sftp.clone();
                    let tx_clone = tx.clone();
                    let ctx_clone = ctx.clone();
                    let path_for_task = path.clone();
                    runtime.spawn(async move {
                        let result = list_remote_children(&sftp_clone, &path_for_task).await;
                        let _ = tx_clone.send((path_for_task, result)).await;
                        ctx_clone.request_repaint();
                    });
                    ui.label(egui::RichText::new("loading…").italics());
                }
                RemoteDirChildren::Loading => {
                    ui.label(egui::RichText::new("loading…").italics());
                }
                RemoteDirChildren::Loaded(c) => {
                    for child in c {
                        render_remote_tree(
                            ui,
                            child,
                            false,
                            host,
                            selected_remote,
                            scroll_target,
                            prefetch,
                            download_request,
                            delete_request,
                            rename_request,
                            refresh_request,
                            favorite_request,
                            sftp,
                            tx,
                            runtime,
                            ctx,
                        );
                    }
                }
                RemoteDirChildren::Error(msg) => {
                    ui.colored_label(egui::Color32::RED, msg.as_str());
                }
                });
            let children_ref: &RemoteDirChildren = &*children;
            collapsing.header_response.context_menu(|ui| {
                // Saves this folder as the root of a future connection — deep
                // paths are usually found by browsing, not remembered.
                if ui.button("Add to Favorites").clicked() {
                    *favorite_request = Some(path.clone());
                    ui.close();
                }
                // SFTP has no change notifications, so a re-list is on demand.
                if ui.button("Refresh").clicked() {
                    *refresh_request = Some(path.clone());
                    ui.close();
                }
                if ui.button("Load").clicked() {
                    if let RemoteDirChildren::Loaded(c) = children_ref {
                        prefetch.extend(image_prefetch_uris(c, host));
                    }
                    ui.close();
                }
                if ui.button("Download").clicked() {
                    *download_request = Some((path.clone(), true));
                    ui.close();
                }
                if !is_root && ui.button("Rename").clicked() {
                    *rename_request = Some((path.clone(), true));
                    ui.close();
                }
                if !is_root && ui.button("Delete").clicked() {
                    *delete_request = Some((path.clone(), true));
                    ui.close();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_prefetch_skips_videos() {
        let children = vec![
            RemoteTreeNode::child(PathBuf::from("/photos/a.jpg"), "a.jpg".to_string(), false),
            RemoteTreeNode::child(PathBuf::from("/photos/b.mkv"), "b.mkv".to_string(), false),
            RemoteTreeNode::child(PathBuf::from("/photos/sub"), "sub".to_string(), true),
        ];
        let uris = image_prefetch_uris(&children, "nas");
        assert_eq!(uris, vec!["sftp://nas/photos/a.jpg".to_string()]);
    }

    #[test]
    fn child_path_rejects_names_that_escape_the_directory() {
        let dir = Path::new("/photos/trip");
        // Ordinary names — including ones with spaces and dots — join normally.
        assert_eq!(child_path(dir, "a.jpg"), Some(PathBuf::from("/photos/trip/a.jpg")));
        assert_eq!(child_path(dir, "my photo.jpg"), Some(PathBuf::from("/photos/trip/my photo.jpg")));
        assert_eq!(child_path(dir, ".hidden"), Some(PathBuf::from("/photos/trip/.hidden")));
        // Traversal, absolute, and nested names would all escape `dir` via push.
        assert_eq!(child_path(dir, ".."), None);
        assert_eq!(child_path(dir, "../../.config/autostart/evil.desktop"), None);
        assert_eq!(child_path(dir, "/etc/cron.d/evil"), None);
        assert_eq!(child_path(dir, "sub/a.jpg"), None);
        // Degenerate names contribute no component at all.
        assert_eq!(child_path(dir, "."), None);
        assert_eq!(child_path(dir, ""), None);
    }

    #[test]
    fn local_target_recreates_folder_and_structure() {
        let dest = PathBuf::from("/home/me/dl");
        let root = PathBuf::from("/photos/trip");
        assert_eq!(
            local_target(&dest, &root, &PathBuf::from("/photos/trip/a.jpg")),
            PathBuf::from("/home/me/dl/trip/a.jpg")
        );
        assert_eq!(
            local_target(&dest, &root, &PathBuf::from("/photos/trip/sub/b.png")),
            PathBuf::from("/home/me/dl/trip/sub/b.png")
        );
    }

    #[test]
    fn may_write_protects_an_existing_local_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("a.jpg");
        // Nothing there yet: the transfer proceeds either way.
        assert!(may_write(&target, false));
        assert!(may_write(&target, true));
        // The user's own edited copy survives a re-run of the folder walk…
        std::fs::write(&target, b"mine").unwrap();
        assert!(!may_write(&target, false));
        // …but the save dialog's explicit confirmation replaces it.
        assert!(may_write(&target, true));
    }

    #[test]
    fn part_path_stages_beside_the_target() {
        // A sibling, so the rename into place stays on one filesystem.
        assert_eq!(
            part_path(Path::new("/dl/trip/a b.jpg")),
            PathBuf::from("/dl/trip/a b.jpg.part")
        );
    }

    #[test]
    fn local_target_keeps_spaces_in_names() {
        assert_eq!(
            local_target(
                &PathBuf::from("/dl"),
                &PathBuf::from("/photos/my trip"),
                &PathBuf::from("/photos/my trip/a b.jpg"),
            ),
            PathBuf::from("/dl/my trip/a b.jpg")
        );
    }

    fn node_name(path: &str) -> String {
        Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    }

    fn rfile(path: &str) -> RemoteTreeNode {
        RemoteTreeNode::child(PathBuf::from(path), node_name(path), false)
    }

    fn runloaded(path: &str) -> RemoteTreeNode {
        RemoteTreeNode::child(PathBuf::from(path), node_name(path), true)
    }

    fn rloaded(path: &str, children: Vec<RemoteTreeNode>) -> RemoteTreeNode {
        RemoteTreeNode {
            path: PathBuf::from(path),
            name: node_name(path),
            kind: RemoteNodeKind::Dir {
                children: RemoteDirChildren::Loaded(children),
            },
        }
    }

    fn loaded_child_paths(node: &RemoteTreeNode) -> Vec<String> {
        match &node.kind {
            RemoteNodeKind::Dir {
                children: RemoteDirChildren::Loaded(c),
            } => c.iter().map(|n| n.path.display().to_string()).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn loaded_dirs_collects_only_loaded_dirs() {
        let root = rloaded(
            "/r",
            vec![
                rfile("/r/a.jpg"),
                runloaded("/r/closed"),
                rloaded("/r/open", vec![rfile("/r/open/b.jpg")]),
            ],
        );
        assert_eq!(
            root.loaded_dirs(),
            vec![PathBuf::from("/r"), PathBuf::from("/r/open")]
        );
    }

    #[test]
    fn merge_listing_preserves_surviving_subtrees() {
        let mut root = rloaded(
            "/r",
            vec![
                rloaded("/r/sub", vec![rfile("/r/sub/x.jpg")]),
                rfile("/r/old.jpg"),
            ],
        );
        // The poll's fresh nodes are Unloaded; old.jpg vanished, new.jpg appeared.
        assert!(root.merge_listing(
            Path::new("/r"),
            vec![rfile("/r/new.jpg"), runloaded("/r/sub")],
        ));
        assert_eq!(loaded_child_paths(&root), vec!["/r/new.jpg", "/r/sub"]);
        // The surviving folder kept its loaded subtree instead of the fresh
        // Unloaded node — nothing collapses or re-fetches.
        let RemoteNodeKind::Dir {
            children: RemoteDirChildren::Loaded(c),
        } = &root.kind
        else {
            unreachable!()
        };
        assert_eq!(loaded_child_paths(&c[1]), vec!["/r/sub/x.jpg"]);
    }

    #[test]
    fn merge_listing_reaches_nested_and_replaces_on_kind_change() {
        let mut root = rloaded("/r", vec![rloaded("/r/sub", vec![runloaded("/r/sub/x")])]);
        // Nested target; "/r/sub/x" flipped from dir to file, so the new node wins.
        assert!(root.merge_listing(Path::new("/r/sub"), vec![rfile("/r/sub/x")]));
        let RemoteNodeKind::Dir {
            children: RemoteDirChildren::Loaded(c),
        } = &root.kind
        else {
            unreachable!()
        };
        let RemoteNodeKind::Dir {
            children: RemoteDirChildren::Loaded(sub),
        } = &c[0].kind
        else {
            unreachable!()
        };
        assert!(!sub[0].is_dir());
    }

    #[test]
    fn merge_listing_noops_unless_loaded() {
        // An Unloaded folder (never opened, or a Refresh in flight owns it).
        let mut unloaded = runloaded("/r");
        assert!(!unloaded.merge_listing(Path::new("/r"), vec![rfile("/r/a.jpg")]));
        // An absent target under a loaded root.
        let mut root = rloaded("/r", vec![]);
        assert!(!root.merge_listing(Path::new("/r/gone"), vec![]));
    }

    #[test]
    fn deletion_order_is_deepest_first() {
        let entries = vec![
            (PathBuf::from("/trip"), true),
            (PathBuf::from("/trip/a.jpg"), false),
            (PathBuf::from("/trip/sub"), true),
            (PathBuf::from("/trip/sub/b.png"), false),
        ];
        let order: Vec<PathBuf> = deletion_order(entries).into_iter().map(|(p, _)| p).collect();
        let pos = |s: &str| order.iter().position(|p| p == Path::new(s)).unwrap();
        // Every entry is removed before its parent directory…
        assert!(pos("/trip/sub/b.png") < pos("/trip/sub"));
        assert!(pos("/trip/sub") < pos("/trip"));
        assert!(pos("/trip/a.jpg") < pos("/trip"));
        // …and the target directory itself is removed last.
        assert_eq!(order.last().unwrap(), Path::new("/trip"));
    }

    #[test]
    fn apply_listing_fills_the_directory_it_was_requested_for() {
        // A subfolder the user just expanded, its listing still in flight.
        let mut root = rloaded("/r", vec![runloaded("/r/sub")]);
        assert!(root.apply_listing(Path::new("/r/sub"), Ok(vec![rfile("/r/sub/a.jpg")])));
        // The listing lands one level down; the root keeps its own children.
        assert_eq!(loaded_child_paths(&root), vec!["/r/sub"]);
        let RemoteNodeKind::Dir { children: RemoteDirChildren::Loaded(c) } = &root.kind
        else {
            unreachable!()
        };
        assert_eq!(loaded_child_paths(&c[0]), vec!["/r/sub/a.jpg"]);
    }

    #[test]
    fn reload_unloads_the_directory_it_was_requested_for() {
        let mut root = rloaded("/r", vec![rloaded("/r/sub", vec![rfile("/r/sub/a.jpg")])]);
        assert!(root.reload(Path::new("/r/sub")));
        // Only the named subfolder is dropped; the root keeps its listing.
        assert_eq!(loaded_child_paths(&root), vec!["/r/sub"]);
        let RemoteNodeKind::Dir { children: RemoteDirChildren::Loaded(c) } = &root.kind
        else {
            unreachable!()
        };
        assert!(matches!(
            c[0].kind,
            RemoteNodeKind::Dir { children: RemoteDirChildren::Unloaded }
        ));
    }
}
