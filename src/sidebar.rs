use eframe::egui;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub struct TreeNode {
    path: PathBuf,
    name: String,
    kind: NodeKind,
}

enum NodeKind {
    File,
    Dir {
        children: Option<Vec<TreeNode>>,
    },
}

pub struct SearchHit {
    path: PathBuf,
    name: String,
    kind: SearchKind,
}

enum SearchKind {
    File,
    Dir { children: Vec<SearchHit> },
}

impl TreeNode {
    pub fn root(path: PathBuf) -> Self {
        let name = path.display().to_string();
        Self {
            path,
            name,
            kind: NodeKind::Dir { children: None },
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Walk the loaded subtree depth-first and collect every media file's full
    /// path (image or video). Folders whose children are `None` (not yet
    /// expanded) contribute nothing.
    pub fn collect_images(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        self.collect_images_into(&mut out);
        out
    }

    fn collect_images_into(&self, out: &mut Vec<PathBuf>) {
        match &self.kind {
            NodeKind::File => out.push(self.path.clone()),
            NodeKind::Dir { children: Some(children) } => {
                for child in children {
                    child.collect_images_into(out);
                }
            }
            NodeKind::Dir { children: None } => {}
        }
    }

    fn child(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let kind = if path.is_dir() {
            NodeKind::Dir { children: None }
        } else {
            NodeKind::File
        };
        Self { path, name, kind }
    }
}

fn is_visible(path: &Path) -> bool {
    path.is_dir() || is_image(path) || crate::video::is_video(&path.to_string_lossy())
}

fn list_children(root: &Path) -> Vec<TreeNode> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut nodes: Vec<TreeNode> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_visible(p))
        .map(TreeNode::child)
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes
}

/// Recursively walk the filesystem under `root`, keeping only entries whose name
/// contains `query` (case-insensitive) plus the ancestor folders that lead to a
/// match. Unlike the live `TreeNode`, the result is fully materialized, so it can
/// never lazy-load an unfiltered directory when rendered.
pub fn search_tree(root: &Path, query: &str) -> Vec<SearchHit> {
    let query_lc = query.to_lowercase();
    let mut visited = HashSet::new();
    search_dir(root, &query_lc, &mut visited)
}

fn search_dir(dir: &Path, query_lc: &str, visited: &mut HashSet<PathBuf>) -> Vec<SearchHit> {
    // Skip a directory already entered, so a symlink pointing back at an ancestor
    // can't make the walk loop forever (`is_dir()` follows symlinks).
    if let Ok(canonical) = dir.canonicalize() {
        if !visited.insert(canonical) {
            return Vec::new();
        }
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_visible(p))
        .collect();
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    let mut hits = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let matches = name.to_lowercase().contains(query_lc);
        if path.is_dir() {
            let children = search_dir(&path, query_lc, visited);
            if matches || !children.is_empty() {
                hits.push(SearchHit { path, name, kind: SearchKind::Dir { children } });
            }
        } else if matches {
            hits.push(SearchHit { path, name, kind: SearchKind::File });
        }
    }
    hits
}

pub fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif")
    )
}

pub fn render_tree(
    ui: &mut egui::Ui,
    node: &mut TreeNode,
    is_root: bool,
    selected_image: &Option<PathBuf>,
    scroll_target: &mut Option<PathBuf>,
    new_selection: &mut Option<PathBuf>,
) {
    match &mut node.kind {
        NodeKind::File => {
            let is_selected = selected_image.as_ref() == Some(&node.path);
            let response = ui.selectable_label(is_selected, &node.name);
            if scroll_target.as_deref() == Some(node.path.as_path()) {
                response.scroll_to_me(Some(egui::Align::Center));
                *scroll_target = None;
            }
            if response.clicked() {
                *new_selection = Some(node.path.clone());
            }
        }
        NodeKind::Dir { children } => {
            let path = node.path.clone();
            // Force this ancestor folder open so the selected row gets rendered.
            // `.open(Some(true))` toggles the underlying CollapsingState if needed
            // and requests a repaint, so the change persists across frames.
            let force_open = scroll_target
                .as_deref()
                .is_some_and(|t| t.starts_with(&node.path));
            let mut header = egui::CollapsingHeader::new(&node.name)
                .id_salt(&node.path)
                .default_open(is_root);
            if force_open {
                header = header.open(Some(true));
            }
            header.show(ui, |ui| {
                if children.is_none() {
                    *children = Some(list_children(&path));
                }
                if let Some(children) = children {
                    for child in children {
                        render_tree(
                            ui,
                            child,
                            false,
                            selected_image,
                            scroll_target,
                            new_selection,
                        );
                    }
                }
            });
        }
    }
}
