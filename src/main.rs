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
mod video;
mod watcher;
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
            cc.egui_ctx
                .add_bytes_loader(Arc::new(sftp_loader::SftpBytesLoader::new(
                    app.session_holder.clone(),
                    app.runtime.handle().clone(),
                    app.cache.clone(),
                )));
            // Registered after the others so egui's reverse-order lookup tries it
            // first: it decodes every image but local HEIC, off-thread and with
            // the EXIF orientation applied, and defers the rest.
            cc.egui_ctx
                .add_image_loader(Arc::new(decoded::DecodedImageLoader::new(
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

/// A connection held up on the user: the server's key is in no known_hosts
/// file, so they are shown its fingerprint and asked. `request` is what to
/// retry once the key is trusted.
struct PendingHostKey {
    request: ssh::ConnectRequest,
    key: russh::keys::PublicKey,
    /// Why recording the key failed, if it did.
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
    /// A Connect click still being resolved. `ssh` is left as it was meanwhile.
    connecting: Option<ssh::ConnectAttempt>,
    ssh_dialog: ssh::ConnectDialog,
    /// Saved connections, mirrored to `config.toml` whenever they change.
    favorites: Vec<config::Favorite>,
    remote_root: Option<remote::RemoteTreeNode>,
    selected_remote: Option<PathBuf>,
    remote_download: Option<remote::RemoteDownload>,
    remote_delete: Option<remote::RemoteDelete>,
    /// Deletes started on a session the app has since left (a reconnect, Open
    /// Folder). They run to completion and report here: dropping the handle
    /// would cancel a deepest-first walk halfway, leaving the files gone and the
    /// directory skeleton standing, with nobody told. Kept apart from
    /// `remote_delete` so they don't hold up a delete on the new session.
    detached_deletes: Vec<remote::RemoteDelete>,
    pending_delete: Option<PendingDelete>,
    pending_rename: Option<PendingRename>,
    pending_host_key: Option<PendingHostKey>,
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
    status_message: Option<status_bar::Message>,
    /// The config file exists but could not be loaded. The next save renames it
    /// to `config.toml.bad` instead of writing the defaults over it.
    config_unusable: bool,
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
        // Read once: loading it per field would drop whatever the other fields
        // hold, which is how a save could lose the favorites.
        let config::Loaded { config, problem } = config::load();
        let status_message = problem.as_ref().map(|problem| {
            status_bar::Message::error(format!(
                "config.toml was not loaded ({problem}); using defaults. \
                 It will be kept as config.toml.bad"
            ))
        });
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
            connecting: None,
            ssh_dialog: ssh::ConnectDialog::from_settings(config.ssh),
            favorites: config.favorites,
            remote_root: None,
            selected_remote: None,
            remote_download: None,
            remote_delete: None,
            detached_deletes: Vec::new(),
            pending_delete: None,
            pending_rename: None,
            pending_host_key: None,
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
            status_message,
            config_unusable: problem.is_some(),
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
            let Some(uri) = self.image_prefetch.pop_front() else {
                break;
            };
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
        let remote_mode = self.remote_shown();
        let (current, list) = if remote_mode {
            let Some(current) = self.selected_remote.clone() else {
                return;
            };
            let Some(root) = self.remote_root.as_ref() else {
                return;
            };
            (current, root.collect_images())
        } else {
            let Some(current) = self.selected_image.clone() else {
                return;
            };
            let Some(root) = self.root_node.as_ref() else {
                return;
            };
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
        let Some(pd) = self.pending_delete.as_ref() else {
            return;
        };
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

    /// Drop everything that belongs to the remote session the app is on, so
    /// nothing of it can show up in, or act on, wherever the app looks next.
    /// Both connect outcomes and Open Folder go through here; they used to keep
    /// three hand-copied lists, which had drifted — Open Folder left the poll
    /// state and the Connected label behind.
    ///
    /// `selected_remote` has to go because the image panel gives it precedence
    /// over any local selection: left set, clicking a local row did nothing
    /// visible. `session_holder` has to go because the loader would keep serving
    /// the old host's bytes. The search is closed so no walk or result list is
    /// stranded against the old host. A running delete is detached, not dropped.
    fn leave_remote_session(&mut self, ctx: &egui::Context) {
        self.remote_root = None;
        self.selected_remote = None;
        self.scroll_target = None;
        self.search_active = false;
        self.search_query.clear();
        self.search_cache = None;
        self.remote_search = None;
        self.remote_search_changed = None;
        self.pending_delete = None;
        self.pending_rename = None;
        self.detach_remote_delete();
        self.remote_rename = None;
        // Listings and poll results are matched to the tree by path alone, so a
        // request still in flight on the old session must have nowhere to land:
        // a slow listing from host A would populate /photos on host B, and a
        // timeout from a dead link would overwrite a folder the new session had
        // just loaded. Draining once was not enough — the old tasks keep their
        // sender and go on sending. New channels leave them a closed one; the
        // old poll cycle likewise keeps its own `running` flag to finish with.
        self.last_remote_poll = None;
        self.remote_poll_running = Arc::new(AtomicBool::new(false));
        (self.remote_listings_tx, self.remote_listings_rx) = tokio::sync::mpsc::channel(64);
        (self.remote_poll_tx, self.remote_poll_rx) = tokio::sync::mpsc::channel(64);
        *self.session_holder.lock().unwrap() = None;
        self.clear_image_prefetch();
        self.forget_all_images(ctx);
    }

    /// Whether the sidebar is showing the remote tree rather than the local one:
    /// connected, with a remote root to browse.
    fn remote_shown(&self) -> bool {
        matches!(self.ssh, ssh::SshState::Connected { .. }) && self.remote_root.is_some()
    }

    /// Apply the outcome of a connection attempt to `target`. Success replaces
    /// the current session. Failure only costs the session when there was none
    /// worth keeping: a working connection used to be torn down the moment
    /// Connect was clicked, so a typo in the host lost its expanded tree, and
    /// the failed attempt then cleared what was left.
    fn finish_connect(
        &mut self,
        request: ssh::ConnectRequest,
        result: ssh::ConnectResult,
        ctx: &egui::Context,
    ) {
        match result {
            Ok((session, info)) => {
                self.leave_remote_session(ctx);
                self.remote_root = Some(remote::RemoteTreeNode::root(PathBuf::from(&info.root)));
                *self.session_holder.lock().unwrap() = Some(session.clone());
                // Off the update loop: opening the cache is sqlite and file
                // I/O, and rebuilding a corrupt one unlinks every blob. Until
                // it lands the loader uses the previous cache, or none.
                let cache = self.cache.clone();
                let key_path = ssh::expand_home(&info.key_path);
                self.runtime
                    .spawn_blocking(move || cache.initialize(&key_path));
                self.ssh = ssh::SshState::Connected { session, info };
            }
            // Not a failure yet: ask, and retry if the key is trusted.
            Err(ssh::ConnectError::UnknownHostKey { key }) => {
                self.pending_host_key = Some(PendingHostKey {
                    request,
                    key,
                    error: None,
                });
            }
            Err(ssh::ConnectError::Other(error))
                if matches!(self.ssh, ssh::SshState::Connected { .. }) =>
            {
                self.status_message = Some(status_bar::Message::error(format!(
                    "Could not connect to {}: {error}",
                    request.target()
                )));
            }
            Err(ssh::ConnectError::Other(error)) => self.ssh = ssh::SshState::Failed { error },
        }
    }

    /// Ask about a server key no known_hosts file has. Trusting it records it in
    /// the app's own list and connects again; anything else drops the attempt.
    fn host_key_prompt(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_host_key else {
            return;
        };
        let mut open = true;
        let mut trust = false;
        let mut cancel = false;
        egui::Window::new("Unknown host key")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{}:{} is not in ~/.ssh/known_hosts, and has not been trusted here before.",
                    pending.request.host, pending.request.port
                ));
                ui.label("Its key is:");
                ui.monospace(ssh::fingerprint(&pending.key));
                ui.label(
                    "Trust it only if this is the fingerprint the server itself reports \
                     (ssh-keygen -lf on its host key). Anyone able to answer on that \
                     address could be presenting this one.",
                );
                if let Some(err) = &pending.error {
                    ui.colored_label(egui::Color32::RED, err.as_str());
                }
                ui.horizontal(|ui| {
                    if ui.button("Trust and connect").clicked() {
                        trust = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if trust && let Some(mut pending) = self.pending_host_key.take() {
            let request = &pending.request;
            match ssh::trust_host_key(&request.host, request.port, &pending.key) {
                Ok(()) => {
                    self.connecting = Some(ssh::ConnectAttempt::spawn(
                        pending.request,
                        &self.runtime,
                        ctx,
                    ));
                }
                Err(e) => {
                    pending.error = Some(e);
                    self.pending_host_key = Some(pending);
                }
            }
        } else if cancel || !open {
            self.pending_host_key = None;
        }
    }

    /// Leave the current session without stopping a delete it is in the middle
    /// of; see `detached_deletes`.
    fn detach_remote_delete(&mut self) {
        self.detached_deletes.extend(self.remote_delete.take());
    }

    /// Settle every remote delete that has finished, whichever session it ran
    /// on and whatever is on screen by now: a failure count goes to the status
    /// bar, since the optimistic UI already cleared the selection and closed the
    /// dialog. Only the current session's delete refreshes the tree — a detached
    /// one belongs to a tree that is gone.
    fn resolve_remote_deletes(&mut self) {
        let mut finished = Vec::new();
        if let Some(del) = self.remote_delete.as_mut() {
            del.poll();
        }
        if self.remote_delete.as_ref().is_some_and(|d| d.is_finished())
            && let Some(del) = self.remote_delete.take()
        {
            if let Some(parent) = del.target().parent()
                && let Some(root) = self.remote_root.as_mut()
            {
                root.reload(parent);
            }
            finished.push(del);
        }
        for del in &mut self.detached_deletes {
            del.poll();
        }
        let (done, running) = std::mem::take(&mut self.detached_deletes)
            .into_iter()
            .partition(|d| d.is_finished());
        self.detached_deletes = running;
        finished.extend::<Vec<_>>(done);
        for del in finished {
            let failed = del.failed();
            if failed > 0 {
                let name = del
                    .target()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                self.status_message = Some(status_bar::Message::error(format!(
                    "Delete {name}: {failed} item(s) could not be removed"
                )));
            }
        }
    }

    /// Persist the whole config. Always writes both halves — writing only the
    /// one that changed would blank the other. Returns whether it was written;
    /// a failure goes to the status bar, since release builds log nothing.
    fn save_config(&mut self) -> bool {
        let config = config::Config {
            ssh: self.ssh_dialog.to_settings(),
            favorites: self.favorites.clone(),
        };
        match config::save(&config, self.config_unusable) {
            Ok(()) => {
                self.config_unusable = false;
                true
            }
            Err(e) => {
                self.status_message = Some(status_bar::Message::error(format!(
                    "Settings not saved: {e}"
                )));
                false
            }
        }
    }

    /// Save `favorite`, reporting in the status bar either way — silently doing
    /// nothing on a duplicate would read as the action having failed.
    fn add_favorite(&mut self, favorite: config::Favorite) {
        let label = favorite.label.clone();
        if !config::add_favorite(&mut self.favorites, favorite) {
            self.status_message =
                Some(status_bar::Message::info(format!("Already saved: {label}")));
        } else if self.save_config() {
            self.status_message =
                Some(status_bar::Message::info(format!("Saved favorite {label}")));
        }
    }

    /// Abandon the prefetch queue along with the session it was built for. Its
    /// URIs name a host, but the loader reads from whichever session is current:
    /// left alone, a "Load" queued against one host kept draining against the
    /// next, hundreds of round trips spent on the wrong machine.
    fn clear_image_prefetch(&mut self) {
        self.image_prefetch.clear();
        self.prefetch_in_flight.clear();
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
        if self
            .selected_image
            .as_deref()
            .is_some_and(|p| p.starts_with(deleted))
        {
            self.selected_image = None;
            cleared = true;
        }
        if self
            .selected_remote
            .as_deref()
            .is_some_and(|p| p.starts_with(deleted))
        {
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
        let Some(pr) = self.pending_rename.as_ref() else {
            return;
        };
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
            // Said in the dialog, as `execute_delete` does. Returning quietly left
            // a Rename button that did nothing, with no clue why.
            let ssh::SshState::Connected { session, .. } = &self.ssh else {
                if let Some(pr) = self.pending_rename.as_mut() {
                    pr.error = Some("Not connected".to_string());
                }
                return;
            };
            self.remote_rename = Some(remote::spawn_remote_rename(
                session.clone(),
                &self.runtime,
                old,
                new,
                ctx,
            ));
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
        self.apply_rename_side_effects(&old, &new, false, ctx);
        self.pending_rename = None;
    }

    /// Settle an in-flight remote rename. Success refreshes the folder and
    /// follows the selection. A failure goes into the dialog if it is still
    /// open — and into the status bar if it is not: the dialog can be closed
    /// while the request is in flight, and the server's refusal used to vanish
    /// with it, leaving a rename that had silently not happened.
    fn resolve_remote_rename(&mut self, ctx: &egui::Context) {
        if let Some(rr) = self.remote_rename.as_mut() {
            rr.poll();
        }
        if !self.remote_rename.as_ref().is_some_and(|r| r.is_finished()) {
            if self.remote_rename.is_some() {
                ctx.request_repaint();
            }
            return;
        }
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
                self.apply_rename_side_effects(&old, &new, true, ctx);
            }
            Some(Err(msg)) => match self.pending_rename.as_mut() {
                Some(pr) => pr.error = Some(msg.clone()),
                None => {
                    let name = rr
                        .target()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    self.status_message =
                        Some(status_bar::Message::error(format!("Rename {name}: {msg}")));
                }
            },
            None => {}
        }
    }

    /// After a successful rename `old`→`new` on the local tree, or on the remote
    /// one when `is_remote`: follow that tree's selection to the new path
    /// (including a selected descendant of a renamed folder), scroll the tree to
    /// it, and close the search so a stale row can't linger.
    ///
    /// Only the renamed side's selection is followed. Both used to be rebased by
    /// path prefix, so with the same library path on both machines
    /// (/home/alex/pics here and on the server) renaming a remote folder rewrote
    /// the local selection to a path that does not exist locally.
    fn apply_rename_side_effects(
        &mut self,
        old: &Path,
        new: &Path,
        is_remote: bool,
        ctx: &egui::Context,
    ) {
        let selection = if is_remote {
            &mut self.selected_remote
        } else {
            &mut self.selected_image
        };
        // Following the path alone still loses the row from view: the new name
        // may sort somewhere off-screen, a renamed folder gets a fresh
        // collapsing-state id and renders collapsed over the selection, and a
        // rename from search results closes into a tree whose ancestors were
        // never expanded. The scroll target force-opens the chain and centers
        // the row, so the selection visibly survives the rename.
        if let Some(p) = selection.as_deref().and_then(|s| rebase_path(s, old, new)) {
            *selection = Some(p.clone());
            self.scroll_target = Some(p);
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
    !trimmed.is_empty() && trimmed != current && !trimmed.contains('/') && !trimmed.contains('\\')
}

/// If `selected` is `old` (or lives under it, for a renamed folder), return the
/// path with the `old` prefix swapped for `new`; otherwise `None`.
fn rebase_path(selected: &Path, old: &Path, new: &Path) -> Option<PathBuf> {
    if selected == old {
        return Some(new.to_path_buf());
    }
    selected.strip_prefix(old).ok().map(|rest| new.join(rest))
}

/// Chase `selected` through a batch of external rename pairs in event order:
/// a selection under a renamed folder follows the folder, and a file renamed
/// twice in one batch lands on its final name. `None` when no pair touched it.
fn follow_renames(selected: Option<&Path>, renames: &[(PathBuf, PathBuf)]) -> Option<PathBuf> {
    let selected = selected?;
    let mut followed: Option<PathBuf> = None;
    for (old, new) in renames {
        if let Some(p) = rebase_path(followed.as_deref().unwrap_or(selected), old, new) {
            followed = Some(p);
        }
    }
    followed
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
        let changes = self.fs_watcher.as_ref().map(|w| w.drain_changes());
        if let Some(changes) = changes {
            if let Some(error) = &changes.error {
                self.status_message = Some(status_bar::Message::error(format!(
                    "File watcher: {error}. Some changes may not show up; \
                     right-click a folder and Refresh to re-list it"
                )));
            }
            if !changes.dirs.is_empty() {
                if let Some(root) = self.root_node.as_mut() {
                    for dir in &changes.dirs {
                        root.reload(dir);
                    }
                }
                // Only flag the search stale; re-walking here would run the
                // whole-tree walk once per frame while the folder churns.
                self.search_dirty = true;
            }
            // A rename outside the app moved the selected file (or a folder
            // above it): follow it, like an in-app rename does. The scroll is
            // set only while the local tree is the one on screen — with the
            // remote tree or search results showing, the target would dangle
            // unconsumed and force-open folders toward a row that isn't there.
            if let Some(p) = follow_renames(self.selected_image.as_deref(), &changes.renames) {
                if !self.remote_shown() && !self.search_active {
                    self.scroll_target = Some(p.clone());
                }
                self.selected_image = Some(p);
                self.forget_all_images(ctx);
            }
        }

        if let Some(result) = self.connecting.as_mut().and_then(|attempt| attempt.poll())
            && let Some(attempt) = self.connecting.take()
        {
            self.finish_connect(attempt.request.clone(), result, ctx);
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
                } else if i.key_pressed(egui::Key::ArrowRight)
                    || i.key_pressed(egui::Key::ArrowDown)
                {
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
        // Chosen in the favorites list, applied after the window closes its
        // borrow of `self.ssh_dialog` / `self.favorites`.
        let mut load_favorite: Option<usize> = None;
        let mut remove_favorite: Option<usize> = None;
        let mut save_favorite = false;
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
                        if ui.text_edit_singleline(&mut self.ssh_dialog.port).changed() {
                            self.ssh_dialog.port_error = None;
                        }
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
                if let Some(err) = &self.ssh_dialog.port_error {
                    ui.colored_label(egui::Color32::RED, err.as_str());
                }
                if ui.button("Connect").clicked() {
                    connect_clicked = true;
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Favorites");
                    if ui.button("Save current").clicked() {
                        save_favorite = true;
                    }
                });
                if self.favorites.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "None yet — save the fields above, or right-click a remote folder.",
                        )
                        .italics()
                        .weak(),
                    );
                }
                for (i, favorite) in self.favorites.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✕").clicked() {
                            remove_favorite = Some(i);
                        }
                        // Full width so the whole row is the target, and long
                        // paths truncate instead of widening the dialog.
                        if ui
                            .add(
                                egui::Button::new(&favorite.label)
                                    .truncate()
                                    .min_size(egui::vec2(ui.available_width(), 0.0)),
                            )
                            .on_hover_text("Load into the fields above")
                            .clicked()
                        {
                            load_favorite = Some(i);
                        }
                    });
                }
            });
        self.ssh_dialog.open = dialog_open;
        if let Some(i) = remove_favorite
            && i < self.favorites.len()
        {
            self.favorites.remove(i);
            self.save_config();
        }
        if let Some(favorite) = load_favorite.and_then(|i| self.favorites.get(i)).cloned() {
            self.ssh_dialog.load_favorite(&favorite);
        }
        if save_favorite {
            self.add_favorite(self.ssh_dialog.to_favorite());
        }
        // A port that does not parse keeps the dialog open with the reason in
        // it, and is not written to the config either.
        let mut port = None;
        if connect_clicked {
            match ssh::parse_port(&self.ssh_dialog.port) {
                Ok(parsed) => port = Some(parsed),
                Err(error) => self.ssh_dialog.port_error = Some(error),
            }
        }
        if let Some(port) = port {
            self.ssh_dialog.port_error = None;
            self.save_config();
            let req = ssh::ConnectRequest {
                host: self.ssh_dialog.host.clone(),
                port,
                user: self.ssh_dialog.user.clone(),
                key_path: self.ssh_dialog.key_path.clone(),
                root: self.ssh_dialog.root.clone(),
            };
            // Replacing an attempt still in flight drops it, which abandons it.
            self.connecting = Some(ssh::ConnectAttempt::spawn(req, &self.runtime, ctx));
            self.pending_host_key = None;
            self.ssh_dialog.open = false;
        }
        self.host_key_prompt(ctx);
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
                        ui.label(format!(
                            "Delete folder \"{name}\" and everything inside it?"
                        ));
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
                    if valid && edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
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

        self.resolve_remote_deletes();

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
        // Set by the remote tree's Add to Favorites action; consumed after the
        // panel into the saved-connection list.
        let mut favorite_request: Option<PathBuf> = None;
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
                        .scroll_bar_visibility(
                            egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                        );
                    if reset_scroll {
                        area.scroll_offset(egui::Vec2::ZERO)
                    } else {
                        area
                    }
                };
                if let (Some(sftp), Some(remote_root)) = (sftp, self.remote_root.as_mut()) {
                    let mut new_remote_selection: Option<PathBuf> = None;
                    if let Some(del) = self.remote_delete.as_ref() {
                        let name = del
                            .target()
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy();
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
                        let same =
                            matches!(&self.remote_search_changed, Some((q, _)) if *q == query);
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
                                &mut favorite_request,
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
                                &mut refresh_request,
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
                self.status_message = Some(status_bar::Message::error(
                    "A download is already running — wait for it or cancel it",
                ));
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
        // An Add to Favorites action was chosen: save the live connection with
        // that folder as its root, so reconnecting lands straight in it.
        if let Some(path) = favorite_request
            && let ssh::SshState::Connected { info, .. } = &self.ssh
        {
            let root = path.to_string_lossy().into_owned();
            let favorite = config::Favorite {
                label: config::Favorite::derive_label(&info.user, &info.host, &root),
                host: info.host.clone(),
                port: info.port.to_string(),
                user: info.user.clone(),
                key_path: info.key_path.clone(),
                root,
            };
            self.add_favorite(favorite);
        }
        // A Refresh action was chosen: drop the folder's cached listing so the
        // next render re-lists it (expanded subfolders re-list lazily). Only one
        // tree renders per frame, so the request came from the one on screen.
        if let Some(path) = refresh_request {
            if self.remote_shown() {
                if let Some(root) = self.remote_root.as_mut() {
                    root.reload(&path);
                }
            } else if let Some(root) = self.root_node.as_mut() {
                root.reload(&path);
                self.search_dirty = true;
            }
        }
        // A Delete action was chosen this frame: park it for the confirm modal.
        if let Some((path, is_dir)) = delete_request {
            let is_remote = self.remote_shown();
            self.pending_delete = Some(PendingDelete {
                path,
                is_dir,
                is_remote,
                error: None,
            });
        }
        // A Rename action was chosen this frame: open the name-entry dialog.
        // Refused while one is still in flight — the second dialog could not be
        // submitted anyway (it renders as "Renaming…"), and replacing the target
        // is how the completion handler used to pair the wrong pair of paths.
        if let Some((path, is_dir)) = rename_request {
            if self.remote_rename.is_some() {
                self.status_message =
                    Some(status_bar::Message::error("A rename is already running"));
            } else {
                let is_remote = self.remote_shown();
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
        self.resolve_remote_rename(ctx);
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
            rebase_path(
                Path::new("/a/b/sub/x.jpg"),
                Path::new("/a/b"),
                Path::new("/a/c")
            ),
            Some(PathBuf::from("/a/c/sub/x.jpg"))
        );
        // An unrelated selection is left alone.
        assert_eq!(
            rebase_path(
                Path::new("/a/other.jpg"),
                Path::new("/a/b"),
                Path::new("/a/c")
            ),
            None
        );
    }

    #[test]
    fn follow_renames_chases_file_folder_and_chained_pairs() {
        let pair = |a: &str, b: &str| (PathBuf::from(a), PathBuf::from(b));
        // The renamed file itself.
        assert_eq!(
            follow_renames(Some(Path::new("/r/a.jpg")), &[pair("/r/a.jpg", "/r/b.jpg")]),
            Some(PathBuf::from("/r/b.jpg"))
        );
        // A selection inside a renamed folder.
        assert_eq!(
            follow_renames(Some(Path::new("/r/d/a.jpg")), &[pair("/r/d", "/r/e")]),
            Some(PathBuf::from("/r/e/a.jpg"))
        );
        // Two pairs in one drained batch chain onto the final name.
        assert_eq!(
            follow_renames(
                Some(Path::new("/r/a.jpg")),
                &[pair("/r/a.jpg", "/r/b.jpg"), pair("/r/b.jpg", "/r/c.jpg")]
            ),
            Some(PathBuf::from("/r/c.jpg"))
        );
    }

    #[test]
    fn follow_renames_ignores_unrelated_pairs_and_no_selection() {
        let pairs = [(PathBuf::from("/r/a.jpg"), PathBuf::from("/r/b.jpg"))];
        assert_eq!(follow_renames(Some(Path::new("/r/keep.jpg")), &pairs), None);
        assert_eq!(follow_renames(None, &pairs), None);
    }

    #[test]
    fn rename_side_effects_follow_and_scroll_to_the_selection() {
        let mut app = TwelfApp::new();
        let ctx = egui::Context::default();
        app.selected_image = Some(PathBuf::from("/r/old.jpg"));
        app.search_active = true;
        app.apply_rename_side_effects(
            Path::new("/r/old.jpg"),
            Path::new("/r/new.jpg"),
            false,
            &ctx,
        );
        assert_eq!(app.selected_image.as_deref(), Some(Path::new("/r/new.jpg")));
        // The tree must also walk open and scroll to the followed selection —
        // the new name may sort off-screen, and a rename from search results
        // closes into a tree whose ancestors were never expanded.
        assert_eq!(app.scroll_target.as_deref(), Some(Path::new("/r/new.jpg")));
        assert!(!app.search_active);
    }

    #[test]
    fn rename_side_effects_follow_a_remote_selection_too() {
        let mut app = TwelfApp::new();
        let ctx = egui::Context::default();
        app.selected_remote = Some(PathBuf::from("/srv/pics/old.jpg"));
        app.apply_rename_side_effects(
            Path::new("/srv/pics/old.jpg"),
            Path::new("/srv/pics/new.jpg"),
            true,
            &ctx,
        );
        assert_eq!(
            app.selected_remote.as_deref(),
            Some(Path::new("/srv/pics/new.jpg"))
        );
        assert_eq!(
            app.scroll_target.as_deref(),
            Some(Path::new("/srv/pics/new.jpg"))
        );
    }

    #[test]
    fn a_rename_on_one_side_leaves_the_same_path_on_the_other_alone() {
        let ctx = egui::Context::default();
        let mut app = TwelfApp::new();
        // The same library path on this machine and on the server.
        app.selected_image = Some(PathBuf::from("/home/alex/pics/d/x.jpg"));
        app.selected_remote = Some(PathBuf::from("/home/alex/pics/d/x.jpg"));
        let (old, new) = (
            Path::new("/home/alex/pics/d"),
            Path::new("/home/alex/pics/e"),
        );

        app.apply_rename_side_effects(old, new, true, &ctx);
        assert_eq!(
            app.selected_remote.as_deref(),
            Some(Path::new("/home/alex/pics/e/x.jpg"))
        );
        // The local folder was not renamed; its file is where it was.
        assert_eq!(
            app.selected_image.as_deref(),
            Some(Path::new("/home/alex/pics/d/x.jpg"))
        );

        app.selected_remote = Some(PathBuf::from("/home/alex/pics/d/x.jpg"));
        app.apply_rename_side_effects(old, new, false, &ctx);
        assert_eq!(
            app.selected_image.as_deref(),
            Some(Path::new("/home/alex/pics/e/x.jpg"))
        );
        assert_eq!(
            app.selected_remote.as_deref(),
            Some(Path::new("/home/alex/pics/d/x.jpg"))
        );
    }

    #[test]
    fn rename_side_effects_leave_an_unrelated_selection_alone() {
        let mut app = TwelfApp::new();
        let ctx = egui::Context::default();
        app.selected_image = Some(PathBuf::from("/r/keep.jpg"));
        app.apply_rename_side_effects(Path::new("/r/a.jpg"), Path::new("/r/b.jpg"), false, &ctx);
        assert_eq!(
            app.selected_image.as_deref(),
            Some(Path::new("/r/keep.jpg"))
        );
        // No scroll either — nothing moved, so the tree must not jump.
        assert_eq!(app.scroll_target, None);
    }

    fn remote_rename_dialog(path: &str, name: &str) -> PendingRename {
        PendingRename {
            path: PathBuf::from(path),
            is_dir: false,
            is_remote: true,
            name: name.to_string(),
            needs_focus: false,
            error: None,
        }
    }

    #[test]
    fn a_failed_remote_rename_is_reported_wherever_it_can_be_seen() {
        let ctx = egui::Context::default();
        let refused = || {
            remote::RemoteRename::finished(
                "/photos/a.jpg",
                "/photos/b.jpg",
                Err("Permission denied".to_string()),
            )
        };

        // Dialog still open: the error belongs in it.
        let mut app = TwelfApp::new();
        app.status_message = None;
        app.pending_rename = Some(remote_rename_dialog("/photos/a.jpg", "b.jpg"));
        app.remote_rename = Some(refused());
        app.resolve_remote_rename(&ctx);
        assert_eq!(
            app.pending_rename
                .as_ref()
                .and_then(|pr| pr.error.as_deref()),
            Some("Permission denied")
        );
        assert_eq!(app.status_message, None);

        // Dialog closed while the request was in flight: the status bar has it.
        app.pending_rename = None;
        app.remote_rename = Some(refused());
        app.resolve_remote_rename(&ctx);
        assert_eq!(
            app.status_message,
            Some(status_bar::Message::error(
                "Rename a.jpg: Permission denied"
            ))
        );
        assert!(app.remote_rename.is_none());
    }

    #[test]
    fn a_remote_rename_without_a_connection_says_so() {
        let ctx = egui::Context::default();
        let mut app = TwelfApp::new();
        app.pending_rename = Some(remote_rename_dialog("/photos/a.jpg", "b.jpg"));
        app.execute_rename(&ctx);
        assert_eq!(
            app.pending_rename
                .as_ref()
                .and_then(|pr| pr.error.as_deref()),
            Some("Not connected")
        );
        assert!(app.remote_rename.is_none());
    }

    fn request() -> ssh::ConnectRequest {
        ssh::ConnectRequest {
            host: "nas".to_string(),
            port: 22,
            user: "alex".to_string(),
            key_path: "~/.ssh/id".to_string(),
            root: "/photos".to_string(),
        }
    }

    #[test]
    fn an_unknown_host_key_is_a_question_not_a_failure() {
        let ctx = egui::Context::default();
        let mut app = TwelfApp::new();
        app.status_message = None;
        let key = russh::keys::PublicKey::from_openssh(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDas5exaMxO62/EkqANCSvgMPxGV3gACEVvq2yzyf7p+",
        )
        .expect("test key");
        app.finish_connect(
            request(),
            Err(ssh::ConnectError::UnknownHostKey { key: key.clone() }),
            &ctx,
        );
        // Held for the prompt, with what is needed to try again…
        let pending = app.pending_host_key.as_ref().expect("prompt pending");
        assert_eq!(pending.request.target(), "alex@nas:22");
        assert_eq!(pending.key, key);
        // …and nothing is reported as having gone wrong.
        assert!(matches!(app.ssh, ssh::SshState::Disconnected));
        assert_eq!(app.status_message, None);
    }

    #[test]
    fn a_failed_connect_with_no_session_to_keep_reports_in_the_menu_bar() {
        let ctx = egui::Context::default();
        let mut app = TwelfApp::new();
        app.status_message = None;
        // A local search is nothing of the remote side's, and stays open.
        app.search_active = true;
        app.search_query = "trip".to_string();
        app.finish_connect(
            request(),
            Err(ssh::ConnectError::Other(
                "no connection after 20 s".to_string(),
            )),
            &ctx,
        );
        assert!(matches!(
            &app.ssh,
            ssh::SshState::Failed { error } if error == "no connection after 20 s"
        ));
        assert!(app.search_active);
        assert_eq!(app.status_message, None);
    }

    #[test]
    fn leaving_the_remote_session_takes_everything_of_it_along() {
        let ctx = egui::Context::default();
        let mut app = TwelfApp::new();
        app.remote_root = Some(remote::RemoteTreeNode::root(PathBuf::from("/photos")));
        app.selected_remote = Some(PathBuf::from("/photos/a.jpg"));
        app.scroll_target = Some(PathBuf::from("/photos/a.jpg"));
        app.search_active = true;
        app.search_query = "trip".to_string();
        app.image_prefetch
            .push_back("sftp://nas/photos/b.jpg".to_string());
        let (delete, worker) = remote::RemoteDelete::running("/photos/old");
        app.remote_delete = Some(delete);
        // What a listing and a poll cycle started on this session hold on to.
        let old_listings = app.remote_listings_tx.clone();
        let old_poll = app.remote_poll_tx.clone();

        app.leave_remote_session(&ctx);

        assert!(app.remote_root.is_none());
        // Left set, this would shadow every local selection in the image panel.
        assert_eq!(app.selected_remote, None);
        assert_eq!(app.scroll_target, None);
        assert!(!app.search_active && app.search_query.is_empty());
        assert!(app.image_prefetch.is_empty());
        assert!(app.session_holder.lock().unwrap().is_none());
        // Requests still in flight on the old session have nowhere to land.
        assert!(old_listings.is_closed() && old_poll.is_closed());
        assert!(!app.remote_listings_tx.is_closed() && !app.remote_poll_tx.is_closed());
        // The delete carries on, detached.
        assert_eq!(app.detached_deletes.len(), 1);
        assert!(!worker.is_cancelled());
    }

    #[test]
    fn leaving_the_session_lets_a_running_delete_finish_and_report() {
        let mut app = TwelfApp::new();
        app.status_message = None;
        let (delete, worker) = remote::RemoteDelete::running("/photos/trip");
        app.remote_delete = Some(delete);

        // What a reconnect or Open Folder does to it.
        app.detach_remote_delete();
        assert!(
            !worker.is_cancelled(),
            "the walk must not be stopped halfway"
        );
        // The new session can start a delete of its own meanwhile.
        assert!(app.remote_delete.is_none());

        // Still running: nothing to report yet, and the handle is kept.
        app.resolve_remote_deletes();
        assert_eq!(app.detached_deletes.len(), 1);
        assert_eq!(app.status_message, None);

        worker.finish(2);
        app.resolve_remote_deletes();
        assert!(app.detached_deletes.is_empty());
        assert_eq!(
            app.status_message,
            Some(status_bar::Message::error(
                "Delete trip: 2 item(s) could not be removed"
            ))
        );
    }

    #[test]
    fn a_clean_delete_on_the_current_session_settles_quietly() {
        let mut app = TwelfApp::new();
        app.status_message = None;
        app.remote_root = Some(remote::RemoteTreeNode::root(PathBuf::from("/photos")));
        let (delete, worker) = remote::RemoteDelete::running("/photos/trip");
        app.remote_delete = Some(delete);
        worker.finish(0);
        app.resolve_remote_deletes();
        assert!(app.remote_delete.is_none());
        // A clean delete has nothing to say.
        assert_eq!(app.status_message, None);
    }
}
