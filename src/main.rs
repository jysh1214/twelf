mod config;
mod fonts;
mod heic;
mod nav;
mod remote;
mod sftp_loader;
mod sidebar;
mod ssh;

use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Twelf",
        options,
        Box::new(|cc| {
            // Must run before any `egui::Image` is rendered.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            cc.egui_ctx
                .add_image_loader(Arc::new(heic::HeicLoader::new()));
            fonts::apply_fonts(&cc.egui_ctx);
            let app = TwelfApp::new();
            cc.egui_ctx.add_bytes_loader(Arc::new(sftp_loader::SftpBytesLoader::new(
                app.session_holder.clone(),
                app.runtime.handle().clone(),
            )));
            Ok(Box::new(app))
        }),
    )
}

struct TwelfApp {
    root_node: Option<sidebar::TreeNode>,
    selected_image: Option<PathBuf>,
    nav: nav::Navigator,
    ssh: ssh::SshState,
    ssh_rx: Option<tokio::sync::mpsc::Receiver<ssh::ConnectResult>>,
    ssh_dialog: ssh::ConnectDialog,
    remote_root: Option<remote::RemoteTreeNode>,
    selected_remote: Option<PathBuf>,
    remote_listings_tx: tokio::sync::mpsc::Sender<remote::ListingResult>,
    remote_listings_rx: tokio::sync::mpsc::Receiver<remote::ListingResult>,
    session_holder: Arc<Mutex<Option<Arc<russh_sftp::client::SftpSession>>>>,
    runtime: tokio::runtime::Runtime,
}

impl TwelfApp {
    fn new() -> Self {
        let (remote_listings_tx, remote_listings_rx) = tokio::sync::mpsc::channel(64);
        Self {
            root_node: None,
            selected_image: None,
            nav: nav::Navigator::new(),
            ssh: ssh::SshState::Disconnected,
            ssh_rx: None,
            ssh_dialog: ssh::ConnectDialog::from_settings(config::load().ssh),
            remote_root: None,
            selected_remote: None,
            remote_listings_tx,
            remote_listings_rx,
            session_holder: Arc::new(Mutex::new(None)),
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime"),
        }
    }

    fn navigate_image(&mut self, delta: i32) {
        let remote_mode = matches!(self.ssh, ssh::SshState::Connected { .. })
            && self.remote_root.is_some();
        let (current, list) = if remote_mode {
            let Some(current) = self.selected_remote.clone() else { return };
            let Some(root) = self.remote_root.as_ref() else { return };
            (current, root.collect_images())
        } else {
            let Some(current) = self.selected_image.clone() else { return };
            let Some(root) = self.root_node.as_ref() else { return };
            (current, root.collect_images())
        };
        if let Some(new) = self.nav.navigate(&list, &current, delta) {
            if remote_mode {
                self.selected_remote = Some(new);
            } else {
                self.selected_image = Some(new);
            }
        }
    }
}

impl eframe::App for TwelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok((path, result)) = self.remote_listings_rx.try_recv() {
            if let Some(root) = self.remote_root.as_mut() {
                root.apply_listing(&path, result);
            }
        }

        if let Some(rx) = self.ssh_rx.as_mut()
            && let Ok(result) = rx.try_recv()
        {
            self.ssh = match result {
                Ok((session, info)) => {
                    self.remote_root = Some(remote::RemoteTreeNode::root(PathBuf::from(&info.root)));
                    self.selected_remote = None;
                    *self.session_holder.lock().unwrap() = Some(session.clone());
                    ssh::SshState::Connected { session, info }
                }
                Err(error) => ssh::SshState::Failed { error },
            };
            self.ssh_rx = None;
        }

        let nav_delta = ctx.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowUp) {
                Some(-1_i32)
            } else if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowDown) {
                Some(1)
            } else {
                None
            }
        });
        if let Some(delta) = nav_delta {
            self.navigate_image(delta);
        }

        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open Folder").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.root_node = Some(sidebar::TreeNode::root(path));
                            self.selected_image = None;
                            self.remote_root = None;
                            self.selected_remote = None;
                            *self.session_holder.lock().unwrap() = None;
                        }
                        ui.close();
                    }
                    if ui.button("Connect SSH").clicked() {
                        self.ssh_dialog.open = true;
                        ui.close();
                    }
                });
                let status = match &self.ssh {
                    ssh::SshState::Disconnected => String::new(),
                    ssh::SshState::Connecting => "Connecting…".to_string(),
                    ssh::SshState::Connected { info, .. } => {
                        format!("Connected: {}@{}:{}", info.user, info.host, info.port)
                    }
                    ssh::SshState::Failed { error } => format!("SSH error: {error}"),
                };
                if !status.is_empty() {
                    ui.label(status);
                }
            });
        });

        let mut connect_clicked = false;
        let mut dialog_open = self.ssh_dialog.open;
        egui::Window::new("Connect SSH")
            .open(&mut dialog_open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("ssh_dialog_grid")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("HostName:");
                        ui.text_edit_singleline(&mut self.ssh_dialog.host);
                        ui.end_row();
                        ui.label("Port:");
                        ui.text_edit_singleline(&mut self.ssh_dialog.port);
                        ui.end_row();
                        ui.label("User:");
                        ui.text_edit_singleline(&mut self.ssh_dialog.user);
                        ui.end_row();
                        ui.label("SSH Key:");
                        ui.text_edit_singleline(&mut self.ssh_dialog.key_path);
                        ui.end_row();
                        ui.label("Root path:");
                        ui.text_edit_singleline(&mut self.ssh_dialog.root);
                        ui.end_row();
                    });
                if ui.button("Connect").clicked() {
                    connect_clicked = true;
                }
            });
        self.ssh_dialog.open = dialog_open;
        if connect_clicked {
            config::save(&config::Config {
                ssh: self.ssh_dialog.to_settings(),
            });
            let req = ssh::ConnectRequest {
                host: self.ssh_dialog.host.clone(),
                port: self.ssh_dialog.port.parse().unwrap_or(22),
                user: self.ssh_dialog.user.clone(),
                key_path: self.ssh_dialog.key_path.clone(),
                root: self.ssh_dialog.root.clone(),
            };
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            self.ssh = ssh::SshState::Connecting;
            self.ssh_rx = Some(rx);
            self.ssh_dialog.open = false;
            let ctx_clone = ctx.clone();
            self.runtime.spawn(async move {
                let result = ssh::connect(req).await;
                let _ = tx.send(result).await;
                ctx_clone.request_repaint();
            });
        }
        let sftp = match &self.ssh {
            ssh::SshState::Connected { session, .. } => Some(session.clone()),
            _ => None,
        };
        egui::SidePanel::left("entries").show(ctx, |ui| {
            ui.set_min_width(ui.available_width());
            if let (Some(sftp), Some(remote_root)) = (sftp, self.remote_root.as_mut()) {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    remote::render_remote_tree(
                        ui,
                        remote_root,
                        true,
                        &mut self.selected_remote,
                        &sftp,
                        &self.remote_listings_tx,
                        &self.runtime,
                        ctx,
                    );
                });
            } else {
                // Captures the clicked image path — deferred to dodge the borrow
                // on `&mut self.root_node` taken by `render_tree`.
                let mut new_selection: Option<PathBuf> = None;
                let scroll_to_selected = self.nav.take_scroll_flag();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if let Some(root_node) = &mut self.root_node {
                        sidebar::render_tree(
                            ui,
                            root_node,
                            true,
                            &self.selected_image,
                            scroll_to_selected,
                            &mut new_selection,
                        );
                    }
                });
                if let Some(path) = new_selection {
                    self.selected_image = Some(path);
                }
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            let uri = if let Some(path) = &self.selected_remote {
                Some(format!("sftp://{}", path.display()))
            } else {
                self.selected_image
                    .as_ref()
                    .map(|path| format!("file://{}", path.display()))
            };
            if let Some(uri) = uri {
                ui.centered_and_justified(|ui| {
                    ui.add(
                        egui::Image::new(uri)
                            .max_size(ui.available_size())
                            .maintain_aspect_ratio(true),
                    );
                });
            }
        });
    }
}
