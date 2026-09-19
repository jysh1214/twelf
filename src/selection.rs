use eframe::egui;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// How a row was clicked, as far as selecting goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClickKind {
    /// Select this row alone.
    Plain,
    /// Ctrl (Cmd on macOS): add this row to the selection, or take it out.
    Toggle,
    /// Shift: select everything from the anchor to this row.
    Range,
}

impl ClickKind {
    pub fn from_modifiers(modifiers: egui::Modifiers) -> Self {
        if modifiers.shift {
            ClickKind::Range
        } else if modifiers.command {
            ClickKind::Toggle
        } else {
            ClickKind::Plain
        }
    }
}

/// A file row that was clicked this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct RowClick {
    pub path: PathBuf,
    pub kind: ClickKind,
}

/// One tree's selection as its rows need it: what to highlight, and how much
/// an action chosen on a row would cover. Cheap to copy down a recursive render.
#[derive(Clone, Copy)]
pub struct Shown<'a> {
    pub primary: Option<&'a Path>,
    pub marked: &'a Marked,
}

impl Shown<'_> {
    pub fn is_selected(&self, path: &Path) -> bool {
        self.primary == Some(path) || self.marked.contains(path)
    }

    /// How many files an action chosen on `clicked` would apply to.
    pub fn target_count(&self, clicked: &Path) -> usize {
        self.marked.targets(clicked, self.primary).len()
    }
}

/// The rows of one tree selected *besides* the primary one — the file on
/// display, which the app keeps as `selected_image` / `selected_remote`. Held
/// apart from it so everything that already follows, clears or rebases the
/// primary keeps working unchanged; the whole selection is the two together.
///
/// Only files are ever in here. A folder row's click expands or collapses it.
#[derive(Debug, Default)]
pub struct Marked {
    paths: BTreeSet<PathBuf>,
    /// Where a Shift-click's range starts: the last row clicked without Shift.
    anchor: Option<PathBuf>,
}

impl Marked {
    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    /// Back to a single selection (arrow keys, a new tree, a finished delete).
    pub fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
    }

    /// Apply a click and return the new primary. `order` is the rows as the
    /// sidebar shows them, top to bottom, which is what a range runs along.
    pub fn click(
        &mut self,
        click: &RowClick,
        primary: Option<&Path>,
        order: &[PathBuf],
    ) -> Option<PathBuf> {
        let clicked = click.path.as_path();
        match click.kind {
            ClickKind::Plain => {
                self.paths.clear();
                self.anchor = Some(click.path.clone());
                Some(click.path.clone())
            }
            ClickKind::Toggle => {
                self.anchor = Some(click.path.clone());
                if primary == Some(clicked) {
                    // Taking the displayed row out: show another selected one,
                    // if there is one, rather than going blank.
                    self.paths.pop_last()
                } else if self.paths.remove(clicked) {
                    primary.map(Path::to_path_buf)
                } else {
                    self.paths.extend(primary.map(Path::to_path_buf));
                    Some(click.path.clone())
                }
            }
            ClickKind::Range => {
                let from = self.anchor.as_deref().or(primary).unwrap_or(clicked);
                let ends = (
                    order.iter().position(|p| p == from),
                    order.iter().position(|p| p == clicked),
                );
                self.paths.clear();
                if let (Some(a), Some(b)) = ends {
                    let (low, high) = (a.min(b), a.max(b));
                    self.paths.extend(order[low..=high].iter().cloned());
                }
                // The anchor stays, so a second Shift-click re-stretches the
                // range from the same place instead of extending the last one.
                if self.anchor.is_none() {
                    self.anchor = Some(from.to_path_buf());
                }
                self.paths.remove(clicked);
                Some(click.path.clone())
            }
        }
    }

    /// What an action chosen on `clicked` applies to: the whole selection when
    /// the row is part of one, and just that row otherwise — a right-click
    /// outside the selection is about the row under the pointer.
    pub fn targets(&self, clicked: &Path, primary: Option<&Path>) -> Vec<PathBuf> {
        let in_selection = primary == Some(clicked) || self.paths.contains(clicked);
        if !in_selection || self.paths.is_empty() {
            return vec![clicked.to_path_buf()];
        }
        let mut all: BTreeSet<PathBuf> = self.paths.clone();
        all.extend(primary.map(Path::to_path_buf));
        all.into_iter().collect()
    }

    /// Carry the marked paths through a rename; `moved_to` answers for one path
    /// with its new location, or `None` if it has not moved.
    pub fn rebase(&mut self, moved_to: impl Fn(&Path) -> Option<PathBuf>) {
        let rebased = |path: PathBuf| moved_to(&path).unwrap_or(path);
        self.paths = std::mem::take(&mut self.paths)
            .into_iter()
            .map(rebased)
            .collect();
        self.anchor = self.anchor.take().map(rebased);
    }

    /// Forget every path `gone` says no longer exists.
    pub fn remove_where(&mut self, gone: impl Fn(&Path) -> bool) {
        self.paths.retain(|path| !gone(path));
        if self.anchor.as_deref().is_some_and(&gone) {
            self.anchor = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> Vec<PathBuf> {
        ["a", "b", "c", "d", "e"]
            .iter()
            .map(|n| PathBuf::from(format!("/r/{n}.jpg")))
            .collect()
    }

    fn click(name: &str, kind: ClickKind) -> RowClick {
        RowClick {
            path: PathBuf::from(format!("/r/{name}.jpg")),
            kind,
        }
    }

    fn p(name: &str) -> PathBuf {
        PathBuf::from(format!("/r/{name}.jpg"))
    }

    /// Everything selected, primary included, as a sorted list of names.
    fn selected(marked: &Marked, primary: &Option<PathBuf>) -> Vec<String> {
        let mut all: BTreeSet<PathBuf> = marked.paths.clone();
        all.extend(primary.clone());
        all.iter()
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_plain_click_selects_one_row_and_drops_the_rest() {
        let mut marked = Marked::default();
        let mut primary = marked.click(&click("a", ClickKind::Plain), None, &order());
        primary = marked.click(&click("c", ClickKind::Toggle), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["a", "c"]);
        primary = marked.click(&click("d", ClickKind::Plain), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["d"]);
        assert_eq!(primary, Some(p("d")));
    }

    #[test]
    fn ctrl_click_adds_a_row_and_takes_it_out_again() {
        let mut marked = Marked::default();
        let mut primary = marked.click(&click("a", ClickKind::Plain), None, &order());
        primary = marked.click(&click("c", ClickKind::Toggle), primary.as_deref(), &order());
        primary = marked.click(&click("e", ClickKind::Toggle), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["a", "c", "e"]);
        // The row just added is the one on display.
        assert_eq!(primary, Some(p("e")));

        // Out again: one that is not on display…
        primary = marked.click(&click("a", ClickKind::Toggle), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["c", "e"]);
        assert_eq!(primary, Some(p("e")));
        // …and the one that is, which hands the display to another.
        primary = marked.click(&click("e", ClickKind::Toggle), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["c"]);
        assert_eq!(primary, Some(p("c")));
        // The last one out leaves nothing selected.
        primary = marked.click(&click("c", ClickKind::Toggle), primary.as_deref(), &order());
        assert_eq!(primary, None);
        assert!(selected(&marked, &primary).is_empty());
    }

    #[test]
    fn shift_click_selects_the_run_from_the_anchor_in_either_direction() {
        let mut marked = Marked::default();
        let mut primary = marked.click(&click("b", ClickKind::Plain), None, &order());
        primary = marked.click(&click("d", ClickKind::Range), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["b", "c", "d"]);
        assert_eq!(primary, Some(p("d")));
        // A second Shift-click re-stretches from the same anchor; upwards works.
        primary = marked.click(&click("a", ClickKind::Range), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["a", "b"]);
        // Ctrl-click moves the anchor.
        primary = marked.click(&click("d", ClickKind::Toggle), primary.as_deref(), &order());
        primary = marked.click(&click("e", ClickKind::Range), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["d", "e"]);
    }

    #[test]
    fn shift_click_with_nothing_selected_or_an_anchor_out_of_sight_selects_the_row() {
        let mut marked = Marked::default();
        let primary = marked.click(&click("c", ClickKind::Range), None, &order());
        assert_eq!(selected(&marked, &primary), ["c"]);

        // The anchor is not among the rows shown (a search has replaced the tree).
        let mut marked = Marked::default();
        let elsewhere = RowClick {
            path: PathBuf::from("/elsewhere/z.jpg"),
            kind: ClickKind::Plain,
        };
        let primary = marked.click(&elsewhere, None, &order());
        let primary = marked.click(&click("c", ClickKind::Range), primary.as_deref(), &order());
        assert_eq!(selected(&marked, &primary), ["c"]);
    }

    #[test]
    fn an_action_covers_the_selection_only_when_chosen_on_a_selected_row() {
        let mut marked = Marked::default();
        let mut primary = marked.click(&click("a", ClickKind::Plain), None, &order());
        primary = marked.click(&click("c", ClickKind::Range), primary.as_deref(), &order());
        // On a marked row, or on the displayed one: all three.
        assert_eq!(
            marked.targets(&p("b"), primary.as_deref()),
            [p("a"), p("b"), p("c")]
        );
        assert_eq!(
            marked.targets(&p("c"), primary.as_deref()),
            [p("a"), p("b"), p("c")]
        );
        // On a row outside the selection: that row alone.
        assert_eq!(marked.targets(&p("e"), primary.as_deref()), [p("e")]);
        // With a single selection it is always just the row.
        let single = Marked::default();
        assert_eq!(single.targets(&p("a"), Some(&p("a"))), [p("a")]);
    }

    #[test]
    fn marked_rows_follow_a_rename_and_forget_what_is_gone() {
        let mut marked = Marked::default();
        let mut primary = marked.click(&click("a", ClickKind::Plain), None, &order());
        primary = marked.click(&click("c", ClickKind::Range), primary.as_deref(), &order());
        assert_eq!(primary, Some(p("c")));
        // The folder holding them is renamed.
        marked.rebase(|path| {
            path.strip_prefix("/r")
                .ok()
                .map(|rest| Path::new("/renamed").join(rest))
        });
        assert!(marked.contains(Path::new("/renamed/a.jpg")));
        assert!(marked.contains(Path::new("/renamed/b.jpg")));
        assert!(!marked.contains(&p("a")));

        marked.remove_where(|path| path.ends_with("a.jpg"));
        assert!(!marked.contains(Path::new("/renamed/a.jpg")));
        assert!(marked.contains(Path::new("/renamed/b.jpg")));
        // The anchor was a.jpg, which is gone; a range now starts from the primary.
        let order = vec![
            PathBuf::from("/renamed/b.jpg"),
            PathBuf::from("/renamed/c.jpg"),
        ];
        let shift = RowClick {
            path: PathBuf::from("/renamed/b.jpg"),
            kind: ClickKind::Range,
        };
        let primary = marked.click(&shift, Some(Path::new("/renamed/c.jpg")), &order);
        assert_eq!(primary, Some(PathBuf::from("/renamed/b.jpg")));
        assert!(marked.contains(Path::new("/renamed/c.jpg")));
    }
}
