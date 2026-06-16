use crate::sidebar;
use eframe::egui;
use futures::future::join_all;
use russh_sftp::client::SftpSession;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
}
