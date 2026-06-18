use crate::sidebar;
use eframe::egui;
use futures::future::join_all;
use russh_sftp::client::SftpSession;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
            let mut child_path = path.to_path_buf();
            child_path.push(&name);
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
        let hits = search_remote_dir(&sftp, &root, &query_lc, &cancel_task, &sem, 0).await;
        let _ = tx.send(hits);
        ctx.request_repaint();
    });
    RemoteSearchWalk { query, cancel, rx, hits: None }
}

/// Recursively walk `dir` over SFTP, keeping entries whose name contains
/// `query_lc` plus the ancestor folders that lead to a match. Empty on a read
/// error (silent-skip, like the local walk), once `cancel` is set, or past the
/// depth cap (which bounds a symlink loop without a per-dir round-trip).
async fn search_remote_dir(
    sftp: &SftpSession,
    dir: &Path,
    query_lc: &str,
    cancel: &AtomicBool,
    sem: &Semaphore,
    depth: usize,
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
                RemoteNodeKind::File => matches.then(|| sidebar::SearchHit::file(path, name)),
                RemoteNodeKind::Dir { .. } => {
                    let children =
                        search_remote_dir(sftp, &path, query_lc, cancel, sem, depth + 1).await;
                    sidebar::SearchHit::dir(path, name, matches, children)
                }
            }
        })
    });
    join_all(children).await.into_iter().flatten().collect()
}

/// Live counters for an in-flight download, shared with the walk task.
#[derive(Default)]
struct DownloadProgress {
    files: AtomicUsize,
    bytes: AtomicU64,
    errors: AtomicUsize,
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
    let children = entries.map(|entry| {
        let is_dir = entry.metadata().is_dir();
        let mut child = dir.to_path_buf();
        child.push(entry.file_name());
        Box::pin(async move {
            if is_dir {
                download_remote_dir(sftp, root, &child, dest, cancel, sem, progress, depth + 1)
                    .await;
            } else {
                download_file(sftp, root, &child, dest, cancel, sem, progress).await;
            }
        })
    });
    join_all(children).await;
}

/// Fetch one remote file and write it under the destination. A read or write
/// failure is counted but does not abort the rest of the walk.
async fn download_file(
    sftp: &SftpSession,
    root: &Path,
    remote_file: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    sem: &Semaphore,
    progress: &DownloadProgress,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let bytes = {
        let _permit = sem.acquire().await.expect("download semaphore never closed");
        match sftp.read(remote_file.to_string_lossy().into_owned()).await {
            Ok(bytes) => bytes,
            Err(_) => {
                progress.errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    };
    if write_file(&local_target(dest, root, remote_file), &bytes).is_err() {
        progress.errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    progress.files.fetch_add(1, Ordering::Relaxed);
    progress.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
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

fn write_file(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(target, bytes)
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
    let children = entries.map(|entry| {
        let is_dir = entry.metadata().is_dir();
        let mut child = dir.to_path_buf();
        child.push(entry.file_name());
        Box::pin(async move {
            let mut out = Vec::new();
            if is_dir {
                out.extend(collect_remote_paths(sftp, &child, cancel, sem, depth + 1).await);
            }
            out.push((child, is_dir));
            out
        })
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
    RemoteRename { target: old, rx, result: None }
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
    download_request: &mut Option<PathBuf>,
    delete_request: &mut Option<(PathBuf, bool)>,
    rename_request: &mut Option<(PathBuf, bool)>,
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
                response.scroll_to_me(Some(egui::Align::Center));
                *scroll_target = None;
            }
            if response.clicked() {
                *selected_remote = Some(node.path.clone());
            }
            response.context_menu(|ui| {
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
                if ui.button("Load").clicked() {
                    if let RemoteDirChildren::Loaded(c) = children_ref {
                        prefetch.extend(image_prefetch_uris(c, host));
                    }
                    ui.close();
                }
                if ui.button("Download").clicked() {
                    *download_request = Some(path.clone());
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
}
