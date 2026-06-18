mod backoff;
mod cache;
mod config;
mod decoded;
mod fonts;
mod heic;
mod image_panel;
mod logging;
mod menu_bar;
mod nav;
mod remote;
mod sftp_loader;
mod sidebar;
mod ssh;
mod video;
mod webp;

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

/// How long the remote search query must be stable before launching a walk —
/// each remote read_dir is a network round-trip, so we don't walk per keystroke.
const REMOTE_SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// A delete the user has requested but not yet confirmed. Held while the confirm
/// modal is open; `is_remote` selects the local-fs vs SFTP backend.
struct PendingDelete {
    path: PathBuf,
    is_dir: bool,
    is_remote: bool,
}

struct TwelfApp {
    root_node: Option<sidebar::TreeNode>,
    selected_image: Option<PathBuf>,
    scroll_target: Option<PathBuf>,
    search_active: bool,
    search_query: String,
    search_cache: Option<(String, Vec<sidebar::SearchHit>)>,
    remote_search: Option<remote::RemoteSearchWalk>,
    remote_search_changed: Option<(String, std::time::Instant)>,
    zoom: f32,
    last_displayed: Option<PathBuf>,
    ssh: ssh::SshState,
    ssh_rx: Option<tokio::sync::mpsc::Receiver<ssh::ConnectResult>>,
    ssh_dialog: ssh::ConnectDialog,
    remote_root: Option<remote::RemoteTreeNode>,
    selected_remote: Option<PathBuf>,
    remote_download: Option<remote::RemoteDownload>,
    pending_delete: Option<PendingDelete>,
    remote_listings_tx: tokio::sync::mpsc::Sender<remote::ListingResult>,
    remote_listings_rx: tokio::sync::mpsc::Receiver<remote::ListingResult>,
    session_holder: Arc<Mutex<Option<Arc<russh_sftp::client::SftpSession>>>>,
    runtime: tokio::runtime::Runtime,
    cache: Arc<cache::ImageCache>,
    image_prefetch: VecDeque<String>,
    animation: Option<webp::Animation>,
    anim_pending: Option<String>,
    video: Option<video::VideoPlayer>,
}

impl TwelfApp {
    fn new() -> Self {
        let (remote_listings_tx, remote_listings_rx) = tokio::sync::mpsc::channel(64);
        Self {
            root_node: None,
            selected_image: None,
            scroll_target: None,
            search_active: false,
            search_query: String::new(),
            search_cache: None,
            remote_search: None,
            remote_search_changed: None,
            zoom: 1.0,
            last_displayed: None,
            ssh: ssh::SshState::Disconnected,
            ssh_rx: None,
            ssh_dialog: ssh::ConnectDialog::from_settings(config::load().ssh),
            remote_root: None,
            selected_remote: None,
            remote_download: None,
            pending_delete: None,
            remote_listings_tx,
            remote_listings_rx,
            session_holder: Arc::new(Mutex::new(None)),
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime"),
            cache: Arc::new(cache::ImageCache::new()),
            image_prefetch: VecDeque::new(),
            animation: None,
            anim_pending: None,
            video: None,
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

    /// Carry out a confirmed delete. The local and remote backends are wired in
    /// later subtasks; for now this only consumes the request.
    fn execute_delete(&mut self, pd: PendingDelete, ctx: &egui::Context) {
        let _ = (pd, ctx);
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
                    self.search_active = false;
                    self.search_query.clear();
                    self.search_cache = None;
                    self.remote_search = None;
                    self.remote_search_changed = None;
                    self.remote_download = None;
                    self.pending_delete = None;
                    *self.session_holder.lock().unwrap() = Some(session.clone());
                    self.cache.initialize(&ssh::expand_home(&info.key_path));
                    ctx.forget_all_images();
                    ssh::SshState::Connected { session, info }
                }
                Err(error) => {
                    // A failed reconnect with the search bar open would otherwise
                    // strand a walk/results against the old host.
                    self.search_active = false;
                    self.search_query.clear();
                    self.search_cache = None;
                    self.remote_search = None;
                    self.remote_search_changed = None;
                    self.remote_download = None;
                    self.pending_delete = None;
                    ssh::SshState::Failed { error }
                }
            };
            self.ssh_rx = None;
        }

        self.drain_image_prefetch(ctx);

        let nav_delta = if ctx.wants_keyboard_input() {
            None
        } else {
            ctx.input(|i| {
                if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowUp) {
                    Some(-1_i32)
                } else if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowDown) {
                    Some(1)
                } else {
                    None
                }
            })
        };
        if let Some(delta) = nav_delta {
            self.navigate_image(delta);
        }

        // Space toggles play/pause for the active video. Consume it only when no
        // text field wants keyboard input, so it still types in the SSH dialog
        // and a focused on-screen button does not also toggle.
        let toggle_video = !ctx.wants_keyboard_input()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space));
        if toggle_video && let Some(player) = self.video.as_mut() {
            player.toggle_pause();
        }

        // Home snaps the side panel back to the root folder. Gated like Space so it
        // still moves the caret in the SSH dialog; clearing the arrow-nav scroll
        // target stops a pending row-scroll from fighting the reset this frame.
        let reset_scroll = !ctx.wants_keyboard_input()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Home));
        if reset_scroll {
            self.scroll_target = None;
        }

        // Ctrl+F opens the sidebar search and focuses its field. Ungated: the Ctrl
        // modifier can't be confused with typing, and gating would make it dead while
        // the search bar or SSH dialog is focused. Esc (only while searching, so it
        // doesn't swallow other Escapes) closes the bar and clears the query.
        let open_search = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F));
        if open_search {
            self.search_active = true;
        }
        if self.search_active
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.search_active = false;
            self.search_query.clear();
            self.search_cache = None;
            self.remote_search = None;
            self.remote_search_changed = None;
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
        // Delete confirmation. A right-click Delete in either tree parks its
        // target in `pending_delete`; nothing is removed until Confirm here.
        let mut confirm_delete = false;
        let mut cancel_delete = false;
        if let Some(pd) = &self.pending_delete {
            let name = pd
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| pd.path.display().to_string());
            let mut open = true;
            egui::Window::new("Delete")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    if pd.is_dir {
                        ui.label(format!("Delete folder \"{name}\" and everything inside it?"));
                    } else {
                        ui.label(format!("Delete \"{name}\"?"));
                    }
                    ui.label(egui::RichText::new("This cannot be undone.").italics());
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            cancel_delete = true;
                        }
                        if ui.button("Delete").clicked() {
                            confirm_delete = true;
                        }
                    });
                });
            // Window close button (X) counts as Cancel.
            if !open {
                cancel_delete = true;
            }
        }
        if cancel_delete {
            self.pending_delete = None;
        }
        if confirm_delete
            && let Some(pd) = self.pending_delete.take()
        {
            self.execute_delete(pd, ctx);
        }

        let sftp = match &self.ssh {
            ssh::SshState::Connected { session, .. } => Some(session.clone()),
            _ => None,
        };
        let remote_host = match &self.ssh {
            ssh::SshState::Connected { info, .. } => info.host.clone(),
            _ => String::new(),
        };
        // Set by the remote tree's Download context-menu action, consumed after the
        // panel so the blocking folder picker runs outside the tree render.
        let mut download_request: Option<PathBuf> = None;
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
                let area = egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
                if reset_scroll {
                    area.scroll_offset(egui::Vec2::ZERO)
                } else {
                    area
                }
            };
            if let (Some(sftp), Some(remote_root)) = (sftp, self.remote_root.as_mut()) {
                let mut new_remote_selection: Option<PathBuf> = None;
                let mut cancel_download = false;
                if let Some(dl) = self.remote_download.as_mut() {
                    dl.poll();
                    let mut text = if dl.is_finished() {
                        format!(
                            "Downloaded {} file(s), {} → {}",
                            dl.files(),
                            menu_bar::format_bytes(dl.bytes()),
                            dl.target().display()
                        )
                    } else {
                        let name = dl.target().file_name().unwrap_or_default().to_string_lossy();
                        format!(
                            "Downloading {name}: {} file(s), {}…",
                            dl.files(),
                            menu_bar::format_bytes(dl.bytes())
                        )
                    };
                    if dl.errors() > 0 {
                        text.push_str(&format!(" ({} failed)", dl.errors()));
                    }
                    let finished = dl.is_finished();
                    ui.horizontal(|ui| {
                        ui.label(text);
                        if !finished && ui.button("Cancel").clicked() {
                            cancel_download = true;
                        }
                    });
                    if !finished {
                        ctx.request_repaint();
                    }
                }
                if cancel_download {
                    self.remote_download = None;
                }
                if self.search_active {
                    sidebar::search_bar(ui, &mut self.search_query, open_search);
                }
                let searching = self.search_active && !self.search_query.trim().is_empty();
                if searching {
                    // Debounce: relaunch the recursive walk only once the query has been
                    // stable for REMOTE_SEARCH_DEBOUNCE. Replacing self.remote_search drops
                    // (and so cancels) any superseded walk.
                    let query = self.search_query.trim().to_string();
                    let same = matches!(&self.remote_search_changed, Some((q, _)) if *q == query);
                    if !same {
                        self.remote_search_changed =
                            Some((query.clone(), std::time::Instant::now()));
                    }
                    let stable = self
                        .remote_search_changed
                        .as_ref()
                        .map(|(_, since)| since.elapsed())
                        .unwrap_or(std::time::Duration::ZERO);
                    let needs_new =
                        self.remote_search.as_ref().map(|w| w.query()) != Some(query.as_str());
                    if needs_new {
                        if stable >= REMOTE_SEARCH_DEBOUNCE {
                            self.remote_search = Some(remote::spawn_remote_search(
                                sftp.clone(),
                                &self.runtime,
                                remote_root.path().to_path_buf(),
                                query,
                                ctx,
                            ));
                        } else {
                            ctx.request_repaint_after(REMOTE_SEARCH_DEBOUNCE - stable);
                        }
                    }
                } else {
                    self.remote_search = None;
                    self.remote_search_changed = None;
                }
                scroll().show(ui, |ui| {
                    if !searching {
                        remote::render_remote_tree(
                            ui,
                            remote_root,
                            true,
                            &remote_host,
                            &mut self.selected_remote,
                            &mut self.scroll_target,
                            &mut self.image_prefetch,
                            &mut download_request,
                            &sftp,
                            &self.remote_listings_tx,
                            &self.runtime,
                            ctx,
                        );
                        return;
                    }
                    let ready = self
                        .remote_search
                        .as_mut()
                        .map(|w| {
                            w.poll();
                            w.hits().is_some()
                        })
                        .unwrap_or(false);
                    if ready {
                        if let Some(hits) = self.remote_search.as_ref().and_then(|w| w.hits()) {
                            sidebar::render_search_results(
                                ui,
                                hits,
                                &self.selected_remote,
                                &mut self.scroll_target,
                                &mut new_remote_selection,
                            );
                        }
                    } else {
                        ui.label(egui::RichText::new("searching…").italics());
                        ctx.request_repaint();
                    }
                });
                if let Some(path) = new_remote_selection {
                    self.selected_remote = Some(path);
                }
            } else {
                // Captures the clicked image path — deferred to dodge the borrow
                // on `&mut self.root_node` taken by the renderers.
                let mut new_selection: Option<PathBuf> = None;
                if self.search_active {
                    sidebar::search_bar(ui, &mut self.search_query, open_search);
                }
                // Refresh the cached walk outside the scroll closure (it needs the root
                // path and query). Re-walk only when the trimmed query changes — egui
                // repaints ~60x/s, so an ungated walk would hit the disk every frame.
                let searching = self.search_active && !self.search_query.trim().is_empty();
                if searching && let Some(root) = self.root_node.as_ref() {
                    let query = self.search_query.trim();
                    if self.search_cache.as_ref().map(|(k, _)| k.as_str()) != Some(query) {
                        let hits = sidebar::search_tree(root.path(), query);
                        self.search_cache = Some((query.to_string(), hits));
                    }
                }
                scroll().show(ui, |ui| {
                    if searching {
                        if let Some((_, hits)) = &self.search_cache {
                            sidebar::render_search_results(
                                ui,
                                hits,
                                &self.selected_image,
                                &mut self.scroll_target,
                                &mut new_selection,
                            );
                        }
                    } else if let Some(root_node) = &mut self.root_node {
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
        // A folder's Download action was chosen: pick a local destination and spawn
        // the recursive copy. The picker runs here (not in the tree render) so it
        // blocks the frame only once, and the still-connected session is reused.
        if let Some(folder) = download_request {
            let session = match &self.ssh {
                ssh::SshState::Connected { session, .. } => Some(session.clone()),
                _ => None,
            };
            if let Some(session) = session
                && let Some(dest) = rfd::FileDialog::new().pick_folder()
            {
                self.remote_download = Some(remote::spawn_remote_download(
                    session,
                    &self.runtime,
                    folder,
                    dest,
                    ctx,
                ));
            }
        }
        image_panel::render(self, ctx);
    }
}
