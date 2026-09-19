use eframe::egui;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc;

/// A recursive watch on the local root folder. Filesystem changes made outside
/// the app land as events; `drain_changes` turns them into the directories whose
/// listing must be re-read. Dropping the handle stops the watch.
pub struct FsWatcher {
    // Held only to keep the watch alive; dropped with self.
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
}

impl FsWatcher {
    /// Start watching `root` recursively. Fails when the watch cannot be
    /// established — e.g. the inotify watch limit on a huge tree. The reason is
    /// handed back for the caller to show: the tree then only changes through
    /// its Refresh action, which the user has to be told.
    pub fn spawn(root: &std::path::Path, ctx: &egui::Context) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let _ = tx.send(res);
                // The app may be idle; without this the change sits unseen
                // until the next input-driven repaint.
                ctx.request_repaint();
            }) {
                Ok(w) => w,
                Err(e) => return Err(e.to_string()),
            };
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Drain pending events into the (deduplicated) directories whose listing
    /// changed, plus the old→new pair of every paired rename among them.
    /// Non-blocking; empty when nothing happened.
    pub fn drain_changes(&self) -> Changes {
        collect_changes(self.rx.try_iter())
    }
}

fn collect_changes(results: impl Iterator<Item = notify::Result<notify::Event>>) -> Changes {
    let mut changes = Changes::default();
    for res in results {
        match res {
            // The kernel's event queue overflowed and events were dropped — a bulk
            // rename or a big copy does it. Which ones cannot be known, so all
            // that can be said is that everything needs looking at again.
            Ok(event) if event.need_rescan() => changes.rescan = true,
            Ok(event) => {
                collect_reload_dirs(&event, &mut changes.dirs);
                collect_rename_pair(&event, &mut changes.renames);
                collect_rewritten(&event, &mut changes.rewritten);
                collect_removed(&event, &mut changes.removed);
            }
            Err(e) => changes.error = Some(e.to_string()),
        }
    }
    changes
}

/// Changes drained from the watcher: the directories whose listing must be
/// re-read, and the (old, new) pair of every paired rename event — the pairing
/// is what lets the app follow its selection when the selected file is renamed
/// outside it.
#[derive(Default)]
pub struct Changes {
    pub dirs: Vec<PathBuf>,
    pub renames: Vec<(PathBuf, PathBuf)>,
    /// Files whose contents were replaced under an unchanged name. No listing
    /// changes, so nothing above notices — but whatever is cached for them is
    /// now a picture of the old contents.
    pub rewritten: Vec<PathBuf>,
    /// Files and folders that were deleted. A save that deletes and recreates
    /// its file shows up here too, so whoever acts on this checks the disk.
    pub removed: Vec<PathBuf>,
    /// Events were lost, so `dirs` and `renames` are not the whole story: the
    /// tree has to be checked against the disk everywhere it is loaded.
    pub rescan: bool,
    /// The latest error the watcher reported, such as running out of inotify
    /// watches for a directory created after the watch began. Whatever it
    /// concerned is no longer being watched, so it is the caller's to show.
    pub error: Option<String>,
}

/// Push the parent of every path in a tree-shape-changing event (create,
/// remove, rename). Content-only events (data or metadata writes, reads)
/// change no listing and are ignored.
fn collect_reload_dirs(event: &notify::Event, out: &mut Vec<PathBuf>) {
    let relevant = matches!(
        event.kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Remove(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    if !relevant {
        return;
    }
    for path in &event.paths {
        if let Some(parent) = path.parent() {
            if !out.iter().any(|d| d == parent) {
                out.push(parent.to_path_buf());
            }
        }
    }
}

/// Push every path a remove event names.
fn collect_removed(event: &notify::Event, out: &mut Vec<PathBuf>) {
    if !matches!(event.kind, notify::EventKind::Remove(_)) {
        return;
    }
    for path in &event.paths {
        if !out.contains(path) {
            out.push(path.clone());
        }
    }
}

/// Push every file an event says was written to: `cp new.png plot.png`, an
/// editor that truncates and rewrites. On Linux that is the close of a file that
/// was open for writing — once per save, when the contents are complete — and
/// not the data-change events, which arrive throughout a large copy and would
/// have a half-written file reloaded over and over. Other backends report no
/// close, so there a data change is all there is to go on.
fn collect_rewritten(event: &notify::Event, out: &mut Vec<PathBuf>) {
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    let written = match event.kind {
        notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        notify::EventKind::Modify(ModifyKind::Data(_)) => cfg!(not(target_os = "linux")),
        _ => false,
    };
    if !written {
        return;
    }
    for path in &event.paths {
        if !out.contains(path) {
            out.push(path.clone());
        }
    }
}

/// Push the (old, new) pair of a paired rename. Only `RenameMode::Both`
/// carries both ends (ordered source, then destination); an unpaired From or
/// To half doesn't say where the file went, so it contributes nothing here and
/// only reloads listings via `collect_reload_dirs`.
fn collect_rename_pair(event: &notify::Event, out: &mut Vec<(PathBuf, PathBuf)>) {
    let paired = matches!(
        event.kind,
        notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both
        ))
    );
    if !paired {
        return;
    }
    if let [old, new] = event.paths.as_slice() {
        out.push((old.clone(), new.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use notify::event::{AccessKind, CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
    use std::path::Path;

    fn dirs_for(events: &[notify::Event]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for event in events {
            collect_reload_dirs(event, &mut out);
        }
        out
    }

    #[test]
    fn a_file_closed_after_writing_counts_as_rewritten() {
        use notify::event::AccessMode;
        let saved = |path: &str| {
            Ok(
                notify::Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)))
                    .add_path(PathBuf::from(path)),
            )
        };
        let results = vec![
            saved("/r/plot.png"),
            // A second save of the same file in one batch is still one file.
            saved("/r/plot.png"),
            // Merely being read is not a change.
            Ok(
                notify::Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)))
                    .add_path(PathBuf::from("/r/other.png")),
            ),
        ];
        let changes = collect_changes(results.into_iter());
        assert_eq!(changes.rewritten, vec![PathBuf::from("/r/plot.png")]);
        // Nothing was added or removed, so no listing needs re-reading.
        assert!(changes.dirs.is_empty());
    }

    /// The real watcher, not a hand-built event: overwriting a file in place is
    /// reported, which is what the hand-built tests above take on trust.
    #[test]
    #[cfg(target_os = "linux")]
    fn overwriting_a_file_in_place_is_seen_by_the_real_watcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("plot.png");
        std::fs::write(&file, b"old").unwrap();
        let watcher = FsWatcher::spawn(dir.path(), &egui::Context::default()).expect("watch");
        std::fs::write(&file, b"new contents").unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut rewritten = Vec::new();
        while !rewritten.contains(&file) {
            assert!(
                std::time::Instant::now() < deadline,
                "no rewrite reported within 5s (saw {rewritten:?})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
            rewritten.extend(watcher.drain_changes().rewritten);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn data_changes_mid_write_are_left_to_the_close_event_on_linux() {
        let writing = Ok(notify::Event::new(EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        )))
        .add_path(PathBuf::from("/r/big.mkv")));
        assert!(
            collect_changes(vec![writing].into_iter())
                .rewritten
                .is_empty()
        );
    }

    #[test]
    fn removals_are_reported_by_path() {
        let results = vec![
            Ok(notify::Event::new(EventKind::Remove(RemoveKind::File))
                .add_path(PathBuf::from("/r/a.jpg"))),
            Ok(notify::Event::new(EventKind::Remove(RemoveKind::Folder))
                .add_path(PathBuf::from("/r/old"))),
        ];
        let changes = collect_changes(results.into_iter());
        assert_eq!(
            changes.removed,
            vec![PathBuf::from("/r/a.jpg"), PathBuf::from("/r/old")]
        );
        // Their listings need re-reading as well.
        assert_eq!(changes.dirs, vec![PathBuf::from("/r")]);
    }

    #[test]
    fn lost_events_ask_for_a_rescan() {
        let results = vec![
            Ok(notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(PathBuf::from("/r/new.jpg"))),
            // What notify sends for an inotify queue overflow.
            Ok(notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)),
        ];
        let changes = collect_changes(results.into_iter());
        assert!(changes.rescan);
        // What did arrive is still reported.
        assert_eq!(changes.dirs, vec![PathBuf::from("/r")]);
        assert!(!collect_changes(std::iter::empty()).rescan);
    }

    #[test]
    fn a_watcher_error_is_handed_on_alongside_the_events() {
        let results = vec![
            Ok(notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(PathBuf::from("/r/new.jpg"))),
            Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch)),
        ];
        let changes = collect_changes(results.into_iter());
        assert_eq!(changes.dirs, vec![PathBuf::from("/r")]);
        assert!(changes.error.is_some());
        // Quiet when nothing went wrong.
        assert!(collect_changes(std::iter::empty()).error.is_none());
    }

    #[test]
    fn create_and_remove_reload_the_parent() {
        let events = [
            notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(PathBuf::from("/r/sub/new.jpg")),
            notify::Event::new(EventKind::Remove(RemoveKind::Folder))
                .add_path(PathBuf::from("/r/old")),
        ];
        assert_eq!(
            dirs_for(&events),
            vec![PathBuf::from("/r/sub"), PathBuf::from("/r")]
        );
    }

    #[test]
    fn rename_reloads_both_ends() {
        // A cross-directory rename carries the old and the new path; both
        // listings changed.
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/r/a/x.jpg"))
            .add_path(PathBuf::from("/r/b/x.jpg"));
        assert_eq!(
            dirs_for(&[event]),
            vec![PathBuf::from("/r/a"), PathBuf::from("/r/b")]
        );
    }

    #[test]
    fn content_and_access_events_are_ignored() {
        let events = [
            notify::Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
                .add_path(PathBuf::from("/r/edited.jpg")),
            notify::Event::new(EventKind::Access(AccessKind::Read))
                .add_path(PathBuf::from("/r/seen.jpg")),
        ];
        assert_eq!(dirs_for(&events), Vec::<PathBuf>::new());
    }

    #[test]
    fn duplicate_parents_are_deduplicated() {
        let events = [
            notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(PathBuf::from("/r/sub/a.jpg")),
            notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(PathBuf::from("/r/sub/b.jpg")),
        ];
        assert_eq!(dirs_for(&events), vec![PathBuf::from("/r/sub")]);
    }

    fn pairs_for(events: &[notify::Event]) -> Vec<(PathBuf, PathBuf)> {
        let mut out = Vec::new();
        for event in events {
            collect_rename_pair(event, &mut out);
        }
        out
    }

    #[test]
    fn paired_rename_yields_its_old_new_pair() {
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/r/old.jpg"))
            .add_path(PathBuf::from("/r/new.jpg"));
        assert_eq!(
            pairs_for(&[event]),
            vec![(PathBuf::from("/r/old.jpg"), PathBuf::from("/r/new.jpg"))]
        );
    }

    #[test]
    fn unpaired_halves_and_other_events_yield_no_pair() {
        // From/To halves arrive when the kernel splits a rename across reads —
        // they carry one end each, so there is nothing to follow.
        let events = [
            notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                .add_path(PathBuf::from("/r/old.jpg")),
            notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
                .add_path(PathBuf::from("/r/new.jpg")),
            notify::Event::new(EventKind::Create(CreateKind::File))
                .add_path(PathBuf::from("/r/made.jpg")),
        ];
        assert_eq!(pairs_for(&events), Vec::<(PathBuf, PathBuf)>::new());
    }

    #[test]
    fn both_event_missing_a_path_yields_no_pair() {
        // Defensive: a malformed Both event with a single path must not pair
        // that path with garbage.
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/r/only.jpg"));
        assert!(pairs_for(&[event]).is_empty());
    }

    #[test]
    fn rootless_path_contributes_nothing() {
        let event =
            notify::Event::new(EventKind::Create(CreateKind::File)).add_path(PathBuf::from("/"));
        assert_eq!(dirs_for(&[event]), Vec::<PathBuf>::new());
        assert_eq!(Path::new("/").parent(), None);
    }
}
