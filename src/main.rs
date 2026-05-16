use eframe::egui;
use std::fs;
use std::path::{Path, PathBuf};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Twelf",
        options,
        Box::new(|cc| {
            // Must run before any `egui::Image` is rendered.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(TwelfApp::new()))
        }),
    )
}

struct TwelfApp {
    root_node: Option<TreeNode>,
    selected_image: Option<PathBuf>,
}

impl TwelfApp {
    fn new() -> Self {
        Self {
            root_node: None,
            selected_image: None,
        }
    }
}

struct TreeNode {
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
    fn root(path: PathBuf) -> Self {
        let name = path.display().to_string();
        Self {
            path,
            name,
            kind: NodeKind::Dir { children: None },
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

fn list_children(root: &Path) -> Vec<TreeNode> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut nodes: Vec<TreeNode> = entries
        .filter_map(Result::ok)
        .map(|e| TreeNode::child(e.path()))
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp")
    )
}

fn render_tree(
    ui: &mut egui::Ui,
    node: &mut TreeNode,
    is_root: bool,
    selected_image: &Option<PathBuf>,
    new_selection: &mut Option<Option<PathBuf>>,
) {
    match &mut node.kind {
        NodeKind::File => {
            let is_selected = selected_image.as_ref() == Some(&node.path);
            if ui.selectable_label(is_selected, &node.name).clicked() {
                let p = node.path.clone();
                *new_selection = Some(if is_image(&p) { Some(p) } else { None });
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

impl eframe::App for TwelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open Folder").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.root_node = Some(TreeNode::root(path));
                            self.selected_image = None;
                        }
                        ui.close();
                    }
                });
            });
        });
        egui::SidePanel::left("entries").show(ctx, |ui| {
            // Outer = a click happened; inner = the new selection (Some=show, None=clear).
            // Deferred to dodge the borrow on `&mut self.root_node`.
            let mut new_selection: Option<Option<PathBuf>> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(root_node) = &mut self.root_node {
                    render_tree(ui, root_node, true, &self.selected_image, &mut new_selection);
                }
            });
            if let Some(sel) = new_selection {
                self.selected_image = sel;
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(path) = &self.selected_image {
                ui.add(
                    egui::Image::new(format!("file://{}", path.display()))
                        .max_size(ui.available_size())
                        .maintain_aspect_ratio(true),
                );
            }
        });
    }
}
