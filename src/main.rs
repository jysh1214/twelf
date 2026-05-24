mod cache;
mod config;
mod decoded;
mod fonts;
mod heic;
mod image_panel;
mod menu_bar;
mod nav;
mod remote;
mod sftp_loader;
mod sidebar;
mod ssh;

use eframe::egui;
use std::collections::VecDeque;
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
                app.cache.clone(),
            )));
            // Registered after the others so egui's reverse-order lookup tries it
            // first for sftp:// images (decoded off-thread); it defers everything else.
            cc.egui_ctx.add_image_loader(Arc::new(decoded::DecodedImageLoader::new(
                app.runtime.handle().clone(),
            )));
            Ok(Box::new(app))
        }),
    )
}

struct TwelfApp {
    root_node: Option<sidebar::TreeNode>,
    selected_image: Option<PathBuf>,
    scroll_target: Option<PathBuf>,
    zoom: f32,
    last_displayed: Option<PathBuf>,
    ssh: ssh::SshState,
    ssh_rx: Option<tokio::sync::mpsc::Receiver<ssh::ConnectResult>>,
    ssh_dialog: ssh::ConnectDialog,
    remote_root: Option<remote::RemoteTreeNode>,
    selected_remote: Option<PathBuf>,
    remote_listings_tx: tokio::sync::mpsc::Sender<remote::ListingResult>,
    remote_listings_rx: tokio::sync::mpsc::Receiver<remote::ListingResult>,
    session_holder: Arc<Mutex<Option<Arc<russh_sftp::client::SftpSession>>>>,
    runtime: tokio::runtime::Runtime,
    cache: Arc<cache::ImageCache>,
    image_prefetch: VecDeque<String>,
}

impl TwelfApp {
    fn new() -> Self {
        let (remote_listings_tx, remote_listings_rx) = tokio::sync::mpsc::channel(64);
        Self {
            root_node: None,
            selected_image: None,
            scroll_target: None,
            zoom: 1.0,
            last_displayed: None,
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
            cache: Arc::new(cache::ImageCache::new()),
            image_prefetch: VecDeque::new(),
        }
    }

    /// Re-poll queued prefetch URIs each frame, driving the bytes→decode
    /// pipeline to completion for off-screen images. A one-shot poll wouldn't
    /// work: the byte fetch is still in flight on the first call.
    fn drain_image_prefetch(&mut self, ctx: &egui::Context) {
        if self.image_prefetch.is_empty() {
            return;
        }
        let mut remaining = VecDeque::with_capacity(self.image_prefetch.len());
        while let Some(uri) = self.image_prefetch.pop_front() {
            match ctx.try_load_image(&uri, egui::load::SizeHint::default()) {
                Ok(egui::load::ImagePoll::Pending { .. }) => remaining.push_back(uri),
                _ => {} // Ready (decoded + cached) or Err (give up) — drop it
            }
        }
        self.image_prefetch = remaining;
        if !self.image_prefetch.is_empty() {
            ctx.request_repaint();
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
        if let Some(new) = nav::navigate(&list, &current, delta) {
            self.scroll_target = Some(new.clone());
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
                    self.scroll_target = None;
                    *self.session_holder.lock().unwrap() = Some(session.clone());
                    self.cache.initialize(&ssh::expand_home(&info.key_path));
                    ctx.forget_all_images();
                    ssh::SshState::Connected { session, info }
                }
                Err(error) => ssh::SshState::Failed { error },
            };
            self.ssh_rx = None;
        }

        self.drain_image_prefetch(ctx);

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

        menu_bar::render(self, ctx);

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
        let remote_host = match &self.ssh {
            ssh::SshState::Connected { info, .. } => info.host.clone(),
            _ => String::new(),
        };
        let screen_w = ctx.content_rect().width();
        egui::SidePanel::left("entries")
            .min_width(screen_w * 0.10)
            .max_width(screen_w * 0.50)
            .show(ctx, |ui| {
            let panel_w = ui.available_width();
            ui.set_min_width(panel_w);
            ui.set_max_width(panel_w);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            let scroll = || {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            };
            if let (Some(sftp), Some(remote_root)) = (sftp, self.remote_root.as_mut()) {
                scroll().show(ui, |ui| {
                    remote::render_remote_tree(
                        ui,
                        remote_root,
                        true,
                        &remote_host,
                        &mut self.selected_remote,
                        &mut self.scroll_target,
                        &mut self.image_prefetch,
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
                scroll().show(ui, |ui| {
                    if let Some(root_node) = &mut self.root_node {
                        sidebar::render_tree(
                            ui,
                            root_node,
                            true,
                            &self.selected_image,
                            &mut self.scroll_target,
                            &mut new_selection,
                        );
                    }
                });
                if let Some(path) = new_selection {
                    self.selected_image = Some(path);
                }
            }
        });
        image_panel::render(self, ctx);
    }
}
