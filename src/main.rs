mod backoff;
mod cache;
mod config;
mod decoded;
mod fonts;
mod heic;
mod image_panel;
mod logging;
mod lru;
mod menu_bar;
mod nav;
mod remote;
mod sftp_loader;
mod sidebar;
mod ssh;
mod status_bar;
mod watcher;
mod video;
mod webp;

use eframe::egui;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// How often the expanded remote folders are re-listed so external changes
/// show up without a manual Refresh. SFTP has no change notifications, so
/// polling is the only automatic option; 30 s keeps the background traffic
/// negligible next to image loads.
const REMOTE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Minimum gap between watcher-driven re-walks of an open local search. The walk
/// is synchronous over the whole tree, so honouring every filesystem event would
/// hit the disk on every frame for as long as a folder is being written into.
const LOCAL_SEARCH_REWALK: std::time::Duration = std::time::Duration::from_millis(500);

/// How many prefetch fetches may be in flight at once. `SftpBytesLoader` spawns
/// a task per URI with no bound of its own, so draining the whole queue turned a
/// "Load" over a folder of 800 images into 800 concurrent whole-file reads down
/// the single SSH channel — with the image the user actually clicked queued
/// behind all of them. Every other remote walk is bounded the same way.
const PREFETCH_IN_FLIGHT: usize = 8;

/// A delete the user has requested but not yet confirmed. Held while the confirm
/// modal is open; `is_remote` selects the local-fs vs SFTP backend, and `error`
/// keeps a failed attempt on screen instead of closing the dialog on silence.
struct PendingDelete {
    path: PathBuf,
    is_dir: bool,
    is_remote: bool,
    error: Option<String>,
}

/// A rename in progress: the target, the backend, the editable new-name buffer,
/// a one-shot focus flag, and any error to show in the dialog.
struct PendingRename {
    path: PathBuf,
    is_dir: bool,
    is_remote: bool,
    name: String,
    needs_focus: bool,
    error: Option<String>,
}

struct TwelfApp {
    root_node: Option<sidebar::TreeNode>,
    fs_watcher: Option<watcher::FsWatcher>,
    selected_image: Option<PathBuf>,
    scroll_target: Option<PathBuf>,
    search_active: bool,
    search_query: String,
    search_cache: Option<(String, Vec<sidebar::SearchHit>)>,
    /// A watcher batch changed the local tree, so an open search's results are
    /// stale. Re-walking is debounced by `LOCAL_SEARCH_REWALK`.
    search_dirty: bool,
    last_search_walk: Option<std::time::Instant>,
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
    remote_delete: Option<remote::RemoteDelete>,
    pending_delete: Option<PendingDelete>,
    pending_rename: Option<PendingRename>,
    remote_rename: Option<remote::RemoteRename>,
    remote_listings_tx: tokio::sync::mpsc::Sender<remote::ListingResult>,
    remote_listings_rx: tokio::sync::mpsc::Receiver<remote::ListingResult>,
    remote_poll_tx: tokio::sync::mpsc::Sender<remote::PollResult>,
    remote_poll_rx: tokio::sync::mpsc::Receiver<remote::PollResult>,
    /// True while a poll cycle's task is still re-listing; the next cycle
    /// waits for it. Replaced (not just cleared) on reconnect so a stale
    /// cycle's completion can't unblock polling against the new session.
    remote_poll_running: Arc<AtomicBool>,
    last_remote_poll: Option<std::time::Instant>,
    session_holder: Arc<Mutex<Option<Arc<russh_sftp::client::SftpSession>>>>,
    runtime: tokio::runtime::Runtime,
    cache: Arc<cache::ImageCache>,
    /// Outcome of the last operation that finished with nothing else to show it
    /// (a partially-failed delete, a rejected request). Rendered in the status
    /// bar until the user dismisses it — `log!` is a no-op in release builds, so
    /// this is the only channel these failures have.
    status_message: Option<String>,
    image_prefetch: VecDeque<String>,
    /// Prefetch URIs whose fetch has been started and is still resolving,
    /// capped at `PREFETCH_IN_FLIGHT`.
    prefetch_in_flight: Vec<String>,
    /// URIs of the most recently displayed images, oldest first. Bounded, and
    /// anything falling out is forgotten — see `image_panel::retain_displayed`.
    displayed_uris: VecDeque<String>,
    animation: Option<webp::Animation>,
    anim_pending: Option<String>,
    video: Option<video::VideoPlayer>,
}

impl TwelfApp {
    fn new() -> Self {
        let (remote_listings_tx, remote_listings_rx) = tokio::sync::mpsc::channel(64);
        let (remote_poll_tx, remote_poll_rx) = tokio::sync::mpsc::channel(64);
        Self {
            root_node: None,
            fs_watcher: None,
            selected_image: None,
            scroll_target: None,
            search_active: false,
            search_query: String::new(),
            search_cache: None,
            search_dirty: false,
            last_search_walk: None,
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
            remote_delete: None,
            pending_delete: None,
            pending_rename: None,
            remote_rename: None,
            remote_listings_tx,
            remote_listings_rx,
            remote_poll_tx,
            remote_poll_rx,
            remote_poll_running: Arc::new(AtomicBool::new(false)),
            last_remote_poll: None,
            session_holder: Arc::new(Mutex::new(None)),
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime"),
            cache: Arc::new(cache::ImageCache::new()),
            status_message: None,
            image_prefetch: VecDeque::new(),
            prefetch_in_flight: Vec::new(),
            displayed_uris: VecDeque::new(),
            animation: None,
            anim_pending: None,
            video: None,
        }
    }

    /// Drive the bytes→decode pipeline for off-screen images, at most
    /// `PREFETCH_IN_FLIGHT` at a time. Started URIs must be re-polled every
    /// frame — a one-shot poll never resolves, since the byte fetch is still in
    /// flight on the first call — and only once one settles is the next one
    /// started, which is what bounds the fan-out.
    fn drain_image_prefetch(&mut self, ctx: &egui::Context) {
        if self.image_prefetch.is_empty() && self.prefetch_in_flight.is_empty() {
            return;
        }
        // Ready (decoded + cached) and Err (give up) both drop out.
        self.prefetch_in_flight.retain(|uri| {
            matches!(
                ctx.try_load_image(uri, egui::load::SizeHint::default()),
                Ok(egui::load::ImagePoll::Pending { .. })
            )
        });
        while self.prefetch_in_flight.len() < PREFETCH_IN_FLIGHT {
            let Some(uri) = self.image_prefetch.pop_front() else { break };
            if let Ok(egui::load::ImagePoll::Pending { .. }) =
                ctx.try_load_image(&uri, egui::load::SizeHint::default())
            {
                self.prefetch_in_flight.push(uri);
            }
        }
        if !self.image_prefetch.is_empty() || !self.prefetch_in_flight.is_empty() {
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

    /// Carry out a confirmed delete. Local deletes run here synchronously; a
    /// remote delete is spawned and resolved by the update loop. A failure keeps
    /// its message in `pending_delete` so the dialog stays open, mirroring
    /// `execute_rename` — the previous silent return left the row on screen with
    /// no clue why.
    fn execute_delete(&mut self, ctx: &egui::Context) {
        let Some(pd) = self.pending_delete.as_ref() else { return };
        let path = pd.path.clone();
        let is_dir = pd.is_dir;
        let is_remote = pd.is_remote;

        if is_remote {
            // One walk at a time. Assigning over a live handle drops it, and
            // `RemoteDelete::drop` cancels the walk it owns: since removal is
            // deepest-first, the abandoned target would be left with its files
            // gone and its directory skeleton standing, reported to no one.
            if self.remote_delete.is_some() {
                self.fail_delete("Another delete is still running");
                return;
            }
            let session = match &self.ssh {
                ssh::SshState::Connected { session, .. } => Some(session.clone()),
                _ => None,
            };
            let Some(session) = session else {
                self.fail_delete("Not connected");
                return;
            };
            self.remote_delete = Some(remote::spawn_remote_delete(
                session,
                &self.runtime,
                path.clone(),
                is_dir,
                ctx,
            ));
            self.pending_delete = None;
            self.clear_after_delete(&path, ctx);
            return;
        }
        let result = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = result {
            self.fail_delete(&e.to_string());
            return;
        }
        if let Some(root) = self.root_node.as_mut() {
            root.remove_path(&path);
        }
        self.pending_delete = None;
        self.clear_after_delete(&path, ctx);
    }

    /// Drop every cached image. Also clears the displayed-URI window, whose
    /// entries have just been forgotten wholesale — leaving them would waste
    /// the window on URIs that no longer hold anything.
    fn forget_all_images(&mut self, ctx: &egui::Context) {
        self.displayed_uris.clear();
        ctx.forget_all_images();
    }

    /// Hold the Delete dialog open with `msg` shown in it.
    fn fail_delete(&mut self, msg: &str) {
        crate::log!("delete failed: {msg}");
        if let Some(pd) = self.pending_delete.as_mut() {
            pd.error = Some(msg.to_string());
        }
    }

    /// After a delete, drop any selection that pointed at (or under) `deleted`
    /// and close the search so a stale result row can't linger.
    fn clear_after_delete(&mut self, deleted: &Path, ctx: &egui::Context) {
        let mut cleared = false;
        if self.selected_image.as_deref().is_some_and(|p| p.starts_with(deleted)) {
            self.selected_image = None;
            cleared = true;
        }
        if self.selected_remote.as_deref().is_some_and(|p| p.starts_with(deleted)) {
            self.selected_remote = None;
            cleared = true;
        }
        if cleared {
            self.forget_all_images(ctx);
        }
        self.search_active = false;
        self.search_query.clear();
        self.search_cache = None;
        self.remote_search = None;
        self.remote_search_changed = None;
    }

    /// Carry out a confirmed rename. Local renames run here synchronously; the
    /// remote backend is wired in a later subtask. On failure the message is kept
    /// in `pending_rename` so the dialog stays open for correction.
    fn execute_rename(&mut self, ctx: &egui::Context) {
        let Some(pr) = self.pending_rename.as_ref() else { return };
        let old = pr.path.clone();
        let is_remote = pr.is_remote;
        let new_name = pr.name.trim().to_string();
        let Some(parent) = old.parent().map(Path::to_path_buf) else {
            self.pending_rename = None;
            return;
        };
        let new = parent.join(&new_name);

        if is_remote {
            // Already in flight — ignore a double submit.
            if self.remote_rename.is_some() {
                return;
            }
            if let ssh::SshState::Connected { session, .. } = &self.ssh {
                self.remote_rename = Some(remote::spawn_remote_rename(
                    session.clone(),
                    &self.runtime,
                    old,
                    new,
                    ctx,
                ));
            }
            // Keep `pending_rename` open; the poll loop resolves success/error.
            return;
        }
        if new.exists() {
            if let Some(pr) = self.pending_rename.as_mut() {
                pr.error = Some("A file or folder with that name already exists".to_string());
            }
            return;
        }
        if let Err(e) = std::fs::rename(&old, &new) {
            if let Some(pr) = self.pending_rename.as_mut() {
                pr.error = Some(e.to_string());
            }
            return;
        }
        if let Some(root) = self.root_node.as_mut() {
            root.reload(&parent);
        }
        self.apply_rename_side_effects(&old, &new, ctx);
        self.pending_rename = None;
    }

    /// After a successful rename `old`→`new`: follow the selection to the new
    /// path (including a selected descendant of a renamed folder) and close the
    /// search so a stale row can't linger.
    fn apply_rename_side_effects(&mut self, old: &Path, new: &Path, ctx: &egui::Context) {
        let img = self.selected_image.as_deref().and_then(|s| rebase_path(s, old, new));
        let rem = self.selected_remote.as_deref().and_then(|s| rebase_path(s, old, new));
        let mut moved = false;
        if let Some(p) = img {
            self.selected_image = Some(p);
            moved = true;
        }
        if let Some(p) = rem {
            self.selected_remote = Some(p);
            moved = true;
        }
        if moved {
            self.forget_all_images(ctx);
        }
        self.search_active = false;
        self.search_query.clear();
        self.search_cache = None;
        self.remote_search = None;
        self.remote_search_changed = None;
    }
}

/// Whether `name` is an acceptable new name for an item currently called
/// `current`: non-empty after trimming, actually changed, and a single path
/// component (no separator).
fn valid_rename(name: &str, current: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed != current
        && !trimmed.contains('/')
        && !trimmed.contains('\\')
}

/// If `selected` is `old` (or lives under it, for a renamed folder), return the
/// path with the `old` prefix swapped for `new`; otherwise `None`.
fn rebase_path(selected: &Path, old: &Path, new: &Path) -> Option<PathBuf> {
    if selected == old {
        return Some(new.to_path_buf());
    }
    selected.strip_prefix(old).ok().map(|rest| new.join(rest))
}

impl eframe::App for TwelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok((path, result)) = self.remote_listings_rx.try_recv() {
            if let Some(root) = self.remote_root.as_mut() {
                root.apply_listing(&path, result);
            }
        }

        // Apply external filesystem changes under the local root: re-list each
        // touched directory and drop stale search results so the tree (and an
        // open search) reflect the disk immediately.
        if let Some(fs_watcher) = &self.fs_watcher {
            let dirs = fs_watcher.changed_dirs();
            if !dirs.is_empty() {
                if let Some(root) = self.root_node.as_mut() {
                    for dir in &dirs {
                        root.reload(dir);
                    }
                }
                // Only flag the search stale; re-walking here would run the
                // whole-tree walk once per frame while the folder churns.
                self.search_dirty = true;
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
                    self.pending_rename = None;
                    self.remote_delete = None;
                    self.remote_rename = None;
                    // Poll state restarts against the new session; drain any
                    // stale cycle's listings so they can't merge into the new
                    // tree, and give the (possibly still-running) old cycle
                    // its own flag to finish with.
                    self.last_remote_poll = None;
                    self.remote_poll_running = Arc::new(AtomicBool::new(false));
                    while self.remote_poll_rx.try_recv().is_ok() {}
                    *self.session_holder.lock().unwrap() = Some(session.clone());
                    self.cache.initialize(&ssh::expand_home(&info.key_path));
                    self.forget_all_images(ctx);
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
                    self.pending_rename = None;
                    self.remote_delete = None;
                    self.remote_rename = None;
                    // Tear the remote side down as thoroughly as the Ok arm and
                    // Open Folder do. Keeping `selected_remote` here wedged the
                    // UI: the image panel gives it precedence over any local
                    // selection, so clicking a local row did nothing visible,
                    // while the stale `session_holder` kept the loader serving
                    // the old host's bytes under a host-less sftp:/// URI.
                    self.remote_root = None;
                    self.selected_remote = None;
                    self.scroll_target = None;
                    self.last_remote_poll = None;
                    self.remote_poll_running = Arc::new(AtomicBool::new(false));
                    *self.session_holder.lock().unwrap() = None;
                    self.forget_all_images(ctx);
                    ssh::SshState::Failed { error }
                }
            };
            self.ssh_rx = None;
        }

        // Periodic remote refresh: merge finished poll listings into the tree
        // (kept nodes keep their loaded subtrees, so nothing flickers or
        // collapses), then start the next cycle over the currently expanded
        // folders once the interval elapsed. A cycle is skipped while the
        // previous one still runs, while a search walk has the connection
        // busy, or while a delete/rename is rewriting the tree (a listing
        // read just before the op could resurrect its target).
        if let ssh::SshState::Connected { session, .. } = &self.ssh
            && self.remote_root.is_some()
        {
            let quiet = self.remote_delete.is_none() && self.remote_rename.is_none();
            while let Ok((path, nodes)) = self.remote_poll_rx.try_recv() {
                if quiet && let Some(root) = self.remote_root.as_mut() {
                    root.merge_listing(&path, nodes);
                }
            }
            let due = self
                .last_remote_poll
                .is_none_or(|t| t.elapsed() >= REMOTE_POLL_INTERVAL);
            if due {
                if quiet
                    && self.remote_search.is_none()
                    && !self.remote_poll_running.load(Ordering::Relaxed)
                {
                    let dirs = self
                        .remote_root
                        .as_ref()
                        .map(|r| r.loaded_dirs())
                        .unwrap_or_default();
                    if !dirs.is_empty() {
                        self.remote_poll_running.store(true, Ordering::Relaxed);
                        remote::spawn_remote_poll(
                            session.clone(),
                            &self.runtime,
                            dirs,
                            self.remote_poll_tx.clone(),
                            self.remote_poll_running.clone(),
                            ctx,
                        );
                    }
                }
                self.last_remote_poll = Some(std::time::Instant::now());
            }
            // Wake up for the next cycle even when the app sits idle.
            let remaining = self
                .last_remote_poll
                .map(|t| REMOTE_POLL_INTERVAL.saturating_sub(t.elapsed()))
                .unwrap_or(REMOTE_POLL_INTERVAL);
            ctx.request_repaint_after(remaining);
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
        // Added before the side panel so the bar spans the full window width.
        status_bar::render(self, ctx);

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
                    if let Some(err) = &pd.error {
                        ui.colored_label(egui::Color32::RED, err.as_str());
                    }
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
        } else if confirm_delete {
            self.execute_delete(ctx);
        }

        // Rename dialog. A right-click Rename in either tree parks its target in
        // `pending_rename`; the entered name is applied only on Rename / Enter.
        let mut do_rename = false;
        let mut cancel_rename = false;
        let renaming = self.remote_rename.is_some();
        if let Some(pr) = self.pending_rename.as_mut() {
            let current = pr
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| pr.path.display().to_string());
            let mut open = true;
            egui::Window::new("Rename")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(if pr.is_dir {
                        format!("Rename folder \"{current}\" to:")
                    } else {
                        format!("Rename \"{current}\" to:")
                    });
                    // A remote rename is in flight — show progress, no re-submit.
                    if renaming {
                        ui.label(egui::RichText::new("Renaming…").italics());
                        return;
                    }
                    let edit = ui.text_edit_singleline(&mut pr.name);
                    if edit.changed() {
                        pr.error = None;
                    }
                    if pr.needs_focus {
                        edit.request_focus();
                        pr.needs_focus = false;
                    }
                    let valid = valid_rename(&pr.name, &current);
                    if valid
                        && edit.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    {
                        do_rename = true;
                    }
                    if let Some(err) = &pr.error {
                        ui.colored_label(egui::Color32::RED, err.as_str());
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            cancel_rename = true;
                        }
                        if ui.add_enabled(valid, egui::Button::new("Rename")).clicked() {
                            do_rename = true;
                        }
                    });
                });
            // Window close button (X) counts as Cancel.
            if !open {
                cancel_rename = true;
            }
        }
        if cancel_rename {
            self.pending_rename = None;
        } else if do_rename {
            self.execute_rename(ctx);
        }

        let sftp = match &self.ssh {
            ssh::SshState::Connected { session, .. } => Some(session.clone()),
            _ => None,
        };
        let remote_host = match &self.ssh {
            ssh::SshState::Connected { info, .. } => info.host.clone(),
            _ => String::new(),
        };
        // Set by the remote tree's Download context-menu action (path, is_dir),
        // consumed after the panel so the blocking picker runs outside the tree
        // render.
        let mut download_request: Option<(PathBuf, bool)> = None;
        // Set by a Delete context-menu action in either tree (path, is_dir);
        // consumed after the panel into `pending_delete`.
        let mut delete_request: Option<(PathBuf, bool)> = None;
        // Set by a Rename context-menu action in either tree (path, is_dir);
        // consumed after the panel into `pending_rename`.
        let mut rename_request: Option<(PathBuf, bool)> = None;
        // Set by the remote tree's Refresh context-menu action; consumed after
        // the panel into a reload of that folder's cached listing.
        let mut refresh_request: Option<PathBuf> = None;
        // Set when a finished remote delete reports failures (name, count);
        // consumed after the panel into `status_message`.
        let mut delete_failed: Option<(String, usize)> = None;
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
                if let Some(del) = self.remote_delete.as_mut() {
                    del.poll();
                }
                if self.remote_delete.as_ref().is_some_and(|d| d.is_finished()) {
                    if let Some(del) = self.remote_delete.take() {
                        let target = del.target().to_path_buf();
                        let failed = del.failed();
                        if let Some(parent) = target.parent() {
                            remote_root.reload(parent);
                        }
                        if failed > 0 {
                            // Applied after the panel: `remote_root` holds a
                            // borrow of self for this whole block.
                            delete_failed = Some((
                                target.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                                failed,
                            ));
                        }
                    }
                } else if let Some(del) = self.remote_delete.as_ref() {
                    let name = del.target().file_name().unwrap_or_default().to_string_lossy();
                    ui.label(egui::RichText::new(format!("Deleting {name}…")).italics());
                    ctx.request_repaint();
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
                            &mut delete_request,
                            &mut rename_request,
                            &mut refresh_request,
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
                                Some(&mut download_request),
                                &mut delete_request,
                                &mut rename_request,
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
                    let query_changed =
                        self.search_cache.as_ref().map(|(k, _)| k.as_str()) != Some(query);
                    // A watcher batch makes the results stale, but the walk is
                    // synchronous and whole-tree, so it waits out the debounce.
                    let refresh_due = self.search_dirty
                        && self
                            .last_search_walk
                            .is_none_or(|t| t.elapsed() >= LOCAL_SEARCH_REWALK);
                    if query_changed || refresh_due {
                        let hits = sidebar::search_tree(root.path(), query);
                        self.search_cache = Some((query.to_string(), hits));
                        self.search_dirty = false;
                        self.last_search_walk = Some(std::time::Instant::now());
                    } else if self.search_dirty {
                        let wait = self
                            .last_search_walk
                            .map(|t| LOCAL_SEARCH_REWALK.saturating_sub(t.elapsed()))
                            .unwrap_or_default();
                        ctx.request_repaint_after(wait);
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
                                None,
                                &mut delete_request,
                                &mut rename_request,
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
                            &mut delete_request,
                            &mut rename_request,
                        );
                    }
                });
                if let Some(path) = new_selection {
                    self.selected_image = Some(path);
                }
            }
        });
        // A Download action was chosen: pick a local destination and spawn the
        // copy — a recursive walk into a picked folder for a directory, a save
        // dialog prefilled with the file's name for a single file. The picker
        // runs here (not in the tree render) so it blocks the frame only once,
        // and the still-connected session is reused.
        if let Some((path, is_dir)) = download_request {
            let session = match &self.ssh {
                ssh::SshState::Connected { session, .. } => Some(session.clone()),
                _ => None,
            };
            // One transfer at a time: the new handle would replace the live one,
            // and `RemoteDownload::drop` cancels the walk it owns — the first
            // copy would stop wherever it got to, still reporting success.
            let busy = self
                .remote_download
                .as_ref()
                .is_some_and(|d| !d.is_finished());
            if busy {
                self.status_message =
                    Some("A download is already running — wait for it or cancel it".to_string());
            } else if let Some(session) = session {
                if is_dir {
                    if let Some(dest) = rfd::FileDialog::new().pick_folder() {
                        self.remote_download = Some(remote::spawn_remote_download(
                            session,
                            &self.runtime,
                            path,
                            dest,
                            ctx,
                        ));
                    }
                } else {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if let Some(target) = rfd::FileDialog::new().set_file_name(name).save_file() {
                        self.remote_download = Some(remote::spawn_remote_file_download(
                            session,
                            &self.runtime,
                            path,
                            target,
                            ctx,
                        ));
                    }
                }
            }
        }
        // A finished remote delete left entries behind: say so, since the
        // optimistic UI already cleared the selection and closed the dialog.
        if let Some((name, failed)) = delete_failed {
            self.status_message =
                Some(format!("Delete {name}: {failed} item(s) could not be removed"));
        }
        // A Refresh action was chosen: drop the folder's cached remote listing
        // so the next render re-lists it (expanded subfolders re-list lazily).
        if let Some(path) = refresh_request
            && let Some(root) = self.remote_root.as_mut()
        {
            root.reload(&path);
        }
        // A Delete action was chosen this frame: park it for the confirm modal.
        if let Some((path, is_dir)) = delete_request {
            let is_remote =
                matches!(self.ssh, ssh::SshState::Connected { .. }) && self.remote_root.is_some();
            self.pending_delete = Some(PendingDelete { path, is_dir, is_remote, error: None });
        }
        // A Rename action was chosen this frame: open the name-entry dialog.
        // Refused while one is still in flight — the second dialog could not be
        // submitted anyway (it renders as "Renaming…"), and replacing the target
        // is how the completion handler used to pair the wrong pair of paths.
        if let Some((path, is_dir)) = rename_request {
            if self.remote_rename.is_some() {
                self.status_message = Some("A rename is already running".to_string());
            } else {
                let is_remote = matches!(self.ssh, ssh::SshState::Connected { .. })
                    && self.remote_root.is_some();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.pending_rename = Some(PendingRename {
                    path,
                    is_dir,
                    is_remote,
                    name,
                    needs_focus: true,
                    error: None,
                });
            }
        }
        // Resolve an in-flight remote rename: refresh on success, surface the
        // server's error in the still-open dialog on failure.
        if let Some(rr) = self.remote_rename.as_mut() {
            rr.poll();
        }
        if self.remote_rename.as_ref().is_some_and(|r| r.is_finished()) {
            let rr = self.remote_rename.take().expect("just checked finished");
            match rr.result() {
                Some(Ok(())) => {
                    // Both paths come from the handle, captured at spawn. Reading
                    // the new name back out of `pending_rename` meant the dialog
                    // being closed mid-flight skipped the side effects entirely.
                    let old = rr.target().to_path_buf();
                    let new = rr.renamed().to_path_buf();
                    if let Some(parent) = old.parent()
                        && let Some(root) = self.remote_root.as_mut()
                    {
                        root.reload(parent);
                    }
                    self.pending_rename = None;
                    self.apply_rename_side_effects(&old, &new, ctx);
                }
                Some(Err(msg)) => {
                    if let Some(pr) = self.pending_rename.as_mut() {
                        pr.error = Some(msg.clone());
                    }
                }
                None => {}
            }
        } else if self.remote_rename.is_some() {
            ctx.request_repaint();
        }
        image_panel::render(self, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_rename_accepts_only_a_changed_simple_name() {
        assert!(valid_rename("new.jpg", "old.jpg"));
        assert!(!valid_rename("   ", "old.jpg")); // blank
        assert!(!valid_rename("old.jpg", "old.jpg")); // unchanged
        assert!(!valid_rename("a/b.jpg", "old.jpg")); // path separator
        assert!(!valid_rename("a\\b.jpg", "old.jpg")); // backslash separator
    }

    #[test]
    fn rebase_path_follows_a_rename() {
        // The renamed item itself follows.
        assert_eq!(
            rebase_path(Path::new("/a/b"), Path::new("/a/b"), Path::new("/a/c")),
            Some(PathBuf::from("/a/c"))
        );
        // A selected descendant of a renamed folder follows by prefix.
        assert_eq!(
            rebase_path(Path::new("/a/b/sub/x.jpg"), Path::new("/a/b"), Path::new("/a/c")),
            Some(PathBuf::from("/a/c/sub/x.jpg"))
        );
        // An unrelated selection is left alone.
        assert_eq!(
            rebase_path(Path::new("/a/other.jpg"), Path::new("/a/b"), Path::new("/a/c")),
            None
        );
    }
}
