use eframe::egui;
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

fn list_children(root: &Path) -> Vec<TreeNode> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut nodes: Vec<TreeNode> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() || is_image(p))
        .map(TreeNode::child)
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes
}

pub fn collect_images(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_images_into(root, &mut out);
    out.sort();
    out
}

fn collect_images_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_images_into(&path, out);
        } else if is_image(&path) {
            out.push(path);
        }
    }
}

fn is_image(path: &Path) -> bool {
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
    new_selection: &mut Option<PathBuf>,
) {
    match &mut node.kind {
        NodeKind::File => {
            let is_selected = selected_image.as_ref() == Some(&node.path);
            if ui.selectable_label(is_selected, &node.name).clicked() {
                *new_selection = Some(node.path.clone());
            }
        }
        NodeKind::Dir { children } => {
            let path = node.path.clone();
            egui::CollapsingHeader::new(&node.name)
                .id_salt(&node.path)
                .default_open(is_root)
                .show(ui, |ui| {
                    if children.is_none() {
                        *children = Some(list_children(&path));
                    }
                    if let Some(children) = children {
                        for child in children {
                            render_tree(ui, child, false, selected_image, new_selection);
                        }
                    }
                });
        }
    }
}
