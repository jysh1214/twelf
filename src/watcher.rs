use eframe::egui;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc;

/// A recursive watch on the local root folder. Filesystem changes made outside
/// the app land as events; `changed_dirs` turns them into the directories whose
/// listing must be re-read. Dropping the handle stops the watch.
pub struct FsWatcher {
    // Held only to keep the watch alive; dropped with self.
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
}

impl FsWatcher {
    /// Start watching `root` recursively. `None` (with a log line) when the
    /// watch cannot be established — e.g. the inotify watch limit on a huge
    /// tree — in which case the tree just stays manually refreshed.
    pub fn spawn(root: &std::path::Path, ctx: &egui::Context) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let mut watcher = match notify::recommended_watcher(
            move |res: notify::Result<notify::Event>| {
                let _ = tx.send(res);
                // The app may be idle; without this the change sits unseen
                // until the next input-driven repaint.
                ctx.request_repaint();
            },
        ) {
            Ok(w) => w,
            Err(e) => {
                crate::log!("failed to create fs watcher: {e}");
                return None;
            }
        };
        if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
            crate::log!("failed to watch {}: {e}", root.display());
            return None;
        }
        Some(Self { _watcher: watcher, rx })
    }

    /// Drain pending events into the (deduplicated) directories whose listing
    /// changed. Non-blocking; empty when nothing happened.
    pub fn changed_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        for res in self.rx.try_iter() {
            match res {
                Ok(event) => collect_reload_dirs(&event, &mut dirs),
                Err(e) => crate::log!("fs watcher error: {e}"),
            }
        }
        dirs
    }
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

    #[test]
    fn rootless_path_contributes_nothing() {
        let event = notify::Event::new(EventKind::Create(CreateKind::File))
            .add_path(PathBuf::from("/"));
        assert_eq!(dirs_for(&[event]), Vec::<PathBuf>::new());
        assert_eq!(Path::new("/").parent(), None);
    }
}
