use crate::sidebar;
use eframe::egui;
use russh_sftp::client::SftpSession;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
            if is_dir || sidebar::is_image(&child_path) {
                Some(RemoteTreeNode::child(child_path, name, is_dir))
            } else {
                None
            }
        })
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(nodes)
}

pub fn render_remote_tree(
    ui: &mut egui::Ui,
    node: &mut RemoteTreeNode,
    is_root: bool,
    selected_remote: &mut Option<PathBuf>,
    sftp: &Arc<SftpSession>,
    tx: &Sender<ListingResult>,
    runtime: &tokio::runtime::Runtime,
    ctx: &egui::Context,
) {
    match &mut node.kind {
        RemoteNodeKind::File => {
            let is_selected = selected_remote.as_deref() == Some(node.path.as_path());
            if ui.selectable_label(is_selected, &node.name).clicked() {
                *selected_remote = Some(node.path.clone());
            }
        }
        RemoteNodeKind::Dir { children } => {
            let path = node.path.clone();
            egui::CollapsingHeader::new(&node.name)
                .id_salt(&node.path)
                .default_open(is_root)
                .show(ui, |ui| match children {
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
                                selected_remote,
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
        }
    }
}
