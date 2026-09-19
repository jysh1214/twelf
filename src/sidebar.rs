use eframe::egui;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub struct TreeNode {
    path: PathBuf,
    name: String,
    kind: NodeKind,
}

enum NodeKind {
    File,
    Dir { children: DirChildren },
}

enum DirChildren {
    /// Not listed yet; rendering the open folder lists it.
    Unloaded,
    Loaded(Vec<TreeNode>),
    /// The listing failed. Shown in place of the rows, because an unreadable
    /// folder — no permission, a NAS mount that dropped — must not pass for an
    /// empty one. Retried when the folder is reopened or the watcher reports a
    /// change in it.
    Error(String),
}

pub struct SearchHit {
    path: PathBuf,
    name: String,
    kind: SearchKind,
}

enum SearchKind {
    File,
    Dir {
        children: Vec<SearchHit>,
        /// The folder's own name matched the query — its full contents were
        /// kept, and it renders expanded-but-collapsible for browsing.
        matched: bool,
    },
}

impl SearchHit {
    pub(crate) fn file(path: PathBuf, name: String) -> Self {
        SearchHit { path, name, kind: SearchKind::File }
    }

    /// Build a directory hit, applying the keep rule: a folder is kept if its
    /// own name matched, if it lives under a matched ancestor (`keep_all` —
    /// that ancestor's full contents stay browsable), or if it has at least
    /// one kept descendant. Returns `None` when it should be dropped.
    /// Single-sources the rule for both the local (`search_dir`) and remote
    /// walks.
    pub(crate) fn dir(
        path: PathBuf,
        name: String,
        matched: bool,
        keep_all: bool,
        children: Vec<SearchHit>,
    ) -> Option<Self> {
        if matched || keep_all || !children.is_empty() {
            Some(SearchHit { path, name, kind: SearchKind::Dir { children, matched } })
        } else {
            None
        }
    }
}

impl TreeNode {
    pub fn root(path: PathBuf) -> Self {
        let name = path.display().to_string();
        Self {
            path,
            name,
            kind: NodeKind::Dir { children: DirChildren::Unloaded },
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Walk the loaded subtree depth-first and collect every media file's full
    /// path (image or video). Folders that are not loaded (not yet expanded, or
    /// unreadable) contribute nothing.
    pub fn collect_images(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        self.collect_images_into(&mut out);
        out
    }

    fn collect_images_into(&self, out: &mut Vec<PathBuf>) {
        match &self.kind {
            NodeKind::File => out.push(self.path.clone()),
            NodeKind::Dir { children: DirChildren::Loaded(children) } => {
                for child in children {
                    child.collect_images_into(out);
                }
            }
            NodeKind::Dir { .. } => {}
        }
    }

    /// Remove the node at `target` from this loaded subtree, returning true once
    /// found. A folder whose children aren't loaded (or a path not present) is a
    /// no-op — it isn't on screen to remove.
    pub fn remove_path(&mut self, target: &Path) -> bool {
        let NodeKind::Dir { children: DirChildren::Loaded(children) } = &mut self.kind else {
            return false;
        };
        if let Some(pos) = children.iter().position(|c| c.path == target) {
            children.remove(pos);
            return true;
        }
        for child in children {
            if target.starts_with(&child.path) && child.remove_path(target) {
                return true;
            }
        }
        false
    }

    /// Mark the directory at `target` not-yet-loaded so the next render re-lists
    /// it — used to refresh a folder after one of its entries is renamed. A no-op
    /// for an absent or not-yet-loaded path.
    pub fn reload(&mut self, target: &Path) -> bool {
        if self.path == target {
            if let NodeKind::Dir { children } = &mut self.kind {
                *children = DirChildren::Unloaded;
                return true;
            }
            return false;
        }
        if !target.starts_with(&self.path) {
            return false;
        }
        let NodeKind::Dir { children: DirChildren::Loaded(children) } = &mut self.kind else {
            return false;
        };
        for child in children {
            if target.starts_with(&child.path) && child.reload(target) {
                return true;
            }
        }
        false
    }

    fn child(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let kind = if path.is_dir() {
            NodeKind::Dir { children: DirChildren::Unloaded }
        } else {
            NodeKind::File
        };
        Self { path, name, kind }
    }
}

fn is_visible(path: &Path) -> bool {
    path.is_dir() || is_image(path) || crate::video::is_video(&path.to_string_lossy())
}

fn list_children(root: &Path) -> DirChildren {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => return DirChildren::Error(e.to_string()),
    };
    let mut nodes: Vec<TreeNode> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_visible(p))
        .map(TreeNode::child)
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    DirChildren::Loaded(nodes)
}

/// Recursively walk the filesystem under `root`, keeping entries whose name
/// contains `query` (case-insensitive) plus the ancestor folders that lead to a
/// match. A folder whose own name matches keeps its full contents, so it can be
/// browsed from the results. Unlike the live `TreeNode`, the result is fully
/// materialized, so it can never lazy-load an unfiltered directory when rendered.
pub fn search_tree(root: &Path, query: &str) -> Vec<SearchHit> {
    let query_lc = query.to_lowercase();
    let mut visited = HashSet::new();
    search_dir(root, &query_lc, &mut visited, false)
}

fn search_dir(
    dir: &Path,
    query_lc: &str,
    visited: &mut HashSet<PathBuf>,
    keep_all: bool,
) -> Vec<SearchHit> {
    // Skip a directory already entered, so a symlink pointing back at an ancestor
    // can't make the walk loop forever (`is_dir()` follows symlinks).
    if let Ok(canonical) = dir.canonicalize() {
        if !visited.insert(canonical) {
            return Vec::new();
        }
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_visible(p))
        .collect();
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    let mut hits = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let matches = name.to_lowercase().contains(query_lc);
        if path.is_dir() {
            let children = search_dir(&path, query_lc, visited, keep_all || matches);
            if let Some(hit) = SearchHit::dir(path, name, matches, keep_all, children) {
                hits.push(hit);
            }
        } else if matches || keep_all {
            hits.push(SearchHit::file(path, name));
        }
    }
    hits
}

pub fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif")
    )
}

pub fn render_tree(
    ui: &mut egui::Ui,
    node: &mut TreeNode,
    is_root: bool,
    selected_image: &Option<PathBuf>,
    scroll_target: &mut Option<PathBuf>,
    new_selection: &mut Option<PathBuf>,
    delete_request: &mut Option<(PathBuf, bool)>,
    rename_request: &mut Option<(PathBuf, bool)>,
) {
    match &mut node.kind {
        NodeKind::File => {
            // The local tree offers no Download — the file is already local.
            render_file_row(
                ui,
                &node.path,
                &node.name,
                selected_image,
                scroll_target,
                new_selection,
                None,
                delete_request,
                rename_request,
            );
        }
        NodeKind::Dir { children } => {
            let path = node.path.clone();
            // Force this ancestor folder open so the selected row gets rendered.
            // `.open(Some(true))` toggles the underlying CollapsingState if needed
            // and requests a repaint, so the change persists across frames.
            let force_open = scroll_target
                .as_deref()
                .is_some_and(|t| t.starts_with(&node.path));
            let mut header = egui::CollapsingHeader::new(&node.name)
                .id_salt(&node.path)
                .default_open(is_root);
            if force_open {
                header = header.open(Some(true));
            }
            let resp = header.show(ui, |ui| {
                if matches!(children, DirChildren::Unloaded) {
                    *children = list_children(&path);
                }
                match &mut *children {
                    DirChildren::Loaded(children) => {
                        for child in children {
                            render_tree(
                                ui,
                                child,
                                false,
                                selected_image,
                                scroll_target,
                                new_selection,
                                delete_request,
                                rename_request,
                            );
                        }
                    }
                    DirChildren::Error(msg) => {
                        ui.colored_label(egui::Color32::RED, msg.as_str());
                    }
                    DirChildren::Unloaded => {}
                }
            });
            // Closing the folder forgets a failed listing, so reopening it is
            // the retry. The error is not re-read every frame it stays open: a
            // dead network mount can block each read_dir for seconds.
            if resp.fully_closed() && matches!(children, DirChildren::Error(_)) {
                *children = DirChildren::Unloaded;
            }
            // No Rename/Delete on the root row — it's the browse entry point.
            if !is_root {
                resp.header_response.context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        *rename_request = Some((path.clone(), true));
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        *delete_request = Some((path.clone(), true));
                        ui.close();
                    }
                });
            }
        }
    }
}

/// Center `response`'s row vertically without moving the tree sideways. The
/// sidebar's `ScrollArea` scrolls both axes and `Response::scroll_to_me`
/// targets both, so centering a long-named row used to drag the whole tree
/// horizontally. With a centered alignment the horizontal correction is the
/// distance between the target's center and the viewport's center, so
/// substituting the viewport's own x-range makes it exactly zero.
pub fn scroll_row_into_view(ui: &egui::Ui, response: &egui::Response) {
    let mut rect = response.rect;
    rect.min.x = ui.clip_rect().min.x;
    rect.max.x = ui.clip_rect().max.x;
    ui.scroll_to_rect(rect, Some(egui::Align::Center));
}

fn render_file_row(
    ui: &mut egui::Ui,
    path: &Path,
    name: &str,
    selected_image: &Option<PathBuf>,
    scroll_target: &mut Option<PathBuf>,
    new_selection: &mut Option<PathBuf>,
    download_request: Option<&mut Option<(PathBuf, bool)>>,
    delete_request: &mut Option<(PathBuf, bool)>,
    rename_request: &mut Option<(PathBuf, bool)>,
) {
    let is_selected = selected_image.as_deref() == Some(path);
    let response = ui.selectable_label(is_selected, name);
    if scroll_target.as_deref() == Some(path) {
        scroll_row_into_view(ui, &response);
        *scroll_target = None;
    }
    if response.clicked() {
        *new_selection = Some(path.to_path_buf());
    }
    response.context_menu(|ui| {
        if let Some(download_request) = download_request
            && ui.button("Download").clicked()
        {
            *download_request = Some((path.to_path_buf(), false));
            ui.close();
        }
        if ui.button("Rename").clicked() {
            *rename_request = Some((path.to_path_buf(), false));
            ui.close();
        }
        if ui.button("Delete").clicked() {
            *delete_request = Some((path.to_path_buf(), false));
            ui.close();
        }
    });
}

/// Render pruned search results under `search:`-prefixed ids, so their
/// expansion never touches the live tree's persisted state. Scaffolding
/// folders (kept only because a descendant matched) are forced open so the
/// chain to every match stays visible; a folder whose own name matched carries
/// its full contents and is user-collapsible (open by default), as is
/// everything below it. File rows and folder headers carry Rename/Delete
/// context actions; `download_request` is `Some` only for remote results — a
/// local file has nothing to download — and adds a Download action to both.
pub fn render_search_results(
    ui: &mut egui::Ui,
    hits: &[SearchHit],
    selected_image: &Option<PathBuf>,
    scroll_target: &mut Option<PathBuf>,
    new_selection: &mut Option<PathBuf>,
    download_request: Option<&mut Option<(PathBuf, bool)>>,
    delete_request: &mut Option<(PathBuf, bool)>,
    rename_request: &mut Option<(PathBuf, bool)>,
) {
    render_search_hits(
        ui,
        hits,
        false,
        selected_image,
        scroll_target,
        new_selection,
        download_request,
        delete_request,
        rename_request,
    );
}

/// `in_matched`: this level lies inside a matched folder's kept-in-full
/// contents, where folders collapse normally (closed unless themselves
/// matched) instead of being forced open.
fn render_search_hits(
    ui: &mut egui::Ui,
    hits: &[SearchHit],
    in_matched: bool,
    selected_image: &Option<PathBuf>,
    scroll_target: &mut Option<PathBuf>,
    new_selection: &mut Option<PathBuf>,
    mut download_request: Option<&mut Option<(PathBuf, bool)>>,
    delete_request: &mut Option<(PathBuf, bool)>,
    rename_request: &mut Option<(PathBuf, bool)>,
) {
    for hit in hits {
        match &hit.kind {
            SearchKind::File => {
                render_file_row(
                    ui,
                    &hit.path,
                    &hit.name,
                    selected_image,
                    scroll_target,
                    new_selection,
                    download_request.as_deref_mut(),
                    delete_request,
                    rename_request,
                );
            }
            SearchKind::Dir { children, matched } => {
                let mut header = egui::CollapsingHeader::new(&hit.name)
                    .id_salt(format!("search:{}", hit.path.display()));
                if in_matched || *matched {
                    header = header.default_open(*matched);
                } else {
                    header = header.open(Some(true));
                }
                let resp = header.show(ui, |ui| {
                    render_search_hits(
                        ui,
                        children,
                        in_matched || *matched,
                        selected_image,
                        scroll_target,
                        new_selection,
                        download_request.as_deref_mut(),
                        delete_request,
                        rename_request,
                    );
                });
                resp.header_response.context_menu(|ui| {
                    if let Some(download_request) = download_request.as_deref_mut()
                        && ui.button("Download").clicked()
                    {
                        *download_request = Some((hit.path.clone(), true));
                        ui.close();
                    }
                    if ui.button("Rename").clicked() {
                        *rename_request = Some((hit.path.clone(), true));
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        *delete_request = Some((hit.path.clone(), true));
                        ui.close();
                    }
                });
            }
        }
    }
}

/// The sidebar search field: a full-width single-line `TextEdit` followed by a
/// separator. `focus` requests keyboard focus this frame — pass `true` only on
/// the frame search opened, or the caret gets trapped and clicks can't land.
pub fn search_bar(ui: &mut egui::Ui, query: &mut String, focus: bool) {
    let response = ui.add(
        egui::TextEdit::singleline(query)
            .hint_text("Search…")
            .desired_width(f32::INFINITY),
    );
    if focus {
        response.request_focus();
    }
    ui.separator();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn touch(path: &Path) {
        fs::write(path, b"x").unwrap();
    }

    /// Pre-order (dir before its children) flatten to `(name, is_dir)` for asserts.
    fn flatten(hits: &[SearchHit]) -> Vec<(String, bool)> {
        fn go(hit: &SearchHit, out: &mut Vec<(String, bool)>) {
            match &hit.kind {
                SearchKind::File => out.push((hit.name.clone(), false)),
                SearchKind::Dir { children, .. } => {
                    out.push((hit.name.clone(), true));
                    for child in children {
                        go(child, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        for hit in hits {
            go(hit, &mut out);
        }
        out
    }

    #[test]
    fn deep_file_match_keeps_only_its_chain() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        touch(&root.join("a/b/target.jpg"));
        touch(&root.join("a/b/other.jpg"));
        touch(&root.join("c/nope.jpg"));
        assert_eq!(
            flatten(&search_tree(root, "target")),
            vec![
                ("a".to_string(), true),
                ("b".to_string(), true),
                ("target.jpg".to_string(), false),
            ]
        );
    }

    #[test]
    fn matching_folder_includes_its_full_contents() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("holiday")).unwrap();
        touch(&root.join("holiday/a.jpg"));
        touch(&root.join("holiday/b.jpg"));
        // The folder name matches; its non-matching children are kept so the
        // folder can be browsed from the results.
        assert_eq!(
            flatten(&search_tree(root, "holiday")),
            vec![
                ("holiday".to_string(), true),
                ("a.jpg".to_string(), false),
                ("b.jpg".to_string(), false),
            ]
        );
    }

    #[test]
    fn matching_folder_keeps_whole_subtree() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("trip/sub")).unwrap();
        touch(&root.join("trip/random.jpg"));
        touch(&root.join("trip/sub/x.jpg"));
        // Nothing under `trip` matches by name, yet the whole subtree —
        // including the empty-of-matches subfolder — stays browsable.
        assert_eq!(
            flatten(&search_tree(root, "trip")),
            vec![
                ("trip".to_string(), true),
                ("random.jpg".to_string(), false),
                ("sub".to_string(), true),
                ("x.jpg".to_string(), false),
            ]
        );
    }

    #[test]
    fn case_insensitive_including_non_ascii() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("FOO.jpg"));
        touch(&root.join("Ärger.png"));
        assert_eq!(
            flatten(&search_tree(root, "foo")),
            vec![("FOO.jpg".to_string(), false)]
        );
        assert_eq!(
            flatten(&search_tree(root, "ärger")),
            vec![("Ärger.png".to_string(), false)]
        );
    }

    #[test]
    fn matches_single_names_not_the_joined_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("ab")).unwrap();
        touch(&root.join("ab/ba.jpg"));
        // "abba" spans the folder/file boundary — neither single name contains
        // it, so nothing matches even though the joined path nearly does.
        assert!(search_tree(root, "abba").is_empty());
    }

    #[test]
    fn no_match_returns_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("a.jpg"));
        assert!(search_tree(root, "zzz").is_empty());
    }

    #[test]
    fn includes_videos_excludes_non_media() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("clip.mkv"));
        touch(&root.join("clip.txt"));
        assert_eq!(
            flatten(&search_tree(root, "clip")),
            vec![("clip.mkv".to_string(), false)]
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_cycle_terminates() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        touch(&root.join("sub/match-me.jpg"));
        // A symlink back to the root would loop forever without the visited guard.
        symlink(root, root.join("sub/loop")).unwrap();
        let names: Vec<String> = flatten(&search_tree(root, "match-me"))
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert!(names.contains(&"match-me.jpg".to_string()));
    }

    #[test]
    fn dir_constructor_applies_keep_rule() {
        let p = || PathBuf::from("/x");
        let child = || SearchHit::file(PathBuf::from("/x/c.jpg"), "c.jpg".to_string());
        // matched folder, no children -> kept
        assert!(SearchHit::dir(p(), "x".to_string(), true, false, vec![]).is_some());
        // unmatched folder, no children, outside any match -> dropped
        assert!(SearchHit::dir(p(), "x".to_string(), false, false, vec![]).is_none());
        // unmatched folder with a kept child -> kept as scaffolding
        assert!(SearchHit::dir(p(), "x".to_string(), false, false, vec![child()]).is_some());
        // unmatched, empty, but under a matched ancestor -> kept for browsing
        assert!(SearchHit::dir(p(), "x".to_string(), false, true, vec![]).is_some());
    }

    #[test]
    fn dir_constructor_stores_the_matched_flag() {
        let matched = SearchHit::dir(PathBuf::from("/x"), "x".to_string(), true, false, vec![])
            .expect("matched folder is kept");
        assert!(matches!(matched.kind, SearchKind::Dir { matched: true, .. }));
        let scaffolding = SearchHit::dir(
            PathBuf::from("/x"),
            "x".to_string(),
            false,
            true,
            vec![],
        )
        .expect("keep_all folder is kept");
        assert!(matches!(scaffolding.kind, SearchKind::Dir { matched: false, .. }));
    }

    #[test]
    fn file_constructor_builds_file_hit() {
        let hit = SearchHit::file(PathBuf::from("/a/b.jpg"), "b.jpg".to_string());
        assert_eq!(flatten(&[hit]), vec![("b.jpg".to_string(), false)]);
    }

    fn file_node(path: &str) -> TreeNode {
        TreeNode { path: PathBuf::from(path), name: path.to_string(), kind: NodeKind::File }
    }

    fn dir_node(path: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            path: PathBuf::from(path),
            name: path.to_string(),
            kind: NodeKind::Dir { children: DirChildren::Loaded(children) },
        }
    }

    fn child_paths(node: &TreeNode) -> Vec<String> {
        match &node.kind {
            NodeKind::Dir { children: DirChildren::Loaded(c) } => {
                c.iter().map(|n| n.path.display().to_string()).collect()
            }
            _ => Vec::new(),
        }
    }

    #[test]
    fn remove_path_drops_node_and_keeps_siblings() {
        let mut root = dir_node(
            "/r",
            vec![
                file_node("/r/a.jpg"),
                dir_node("/r/sub", vec![file_node("/r/sub/b.png")]),
                file_node("/r/d.jpg"),
            ],
        );
        assert!(root.remove_path(Path::new("/r/a.jpg")));
        assert_eq!(child_paths(&root), vec!["/r/sub", "/r/d.jpg"]);
    }

    #[test]
    fn remove_path_reaches_into_nested_dir() {
        let mut root = dir_node(
            "/r",
            vec![dir_node(
                "/r/sub",
                vec![file_node("/r/sub/b.png"), file_node("/r/sub/c.png")],
            )],
        );
        assert!(root.remove_path(Path::new("/r/sub/b.png")));
        let NodeKind::Dir { children: DirChildren::Loaded(c) } = &root.kind else {
            unreachable!()
        };
        assert_eq!(child_paths(&c[0]), vec!["/r/sub/c.png"]);
    }

    #[test]
    fn remove_path_absent_or_unloaded_is_noop() {
        let mut root = dir_node("/r", vec![file_node("/r/a.jpg")]);
        assert!(!root.remove_path(Path::new("/r/zzz.jpg")));
        assert_eq!(child_paths(&root), vec!["/r/a.jpg"]);

        // A folder whose children haven't been loaded yet.
        let mut unloaded = TreeNode::root(PathBuf::from("/r"));
        assert!(!unloaded.remove_path(Path::new("/r/a.jpg")));
    }

    #[test]
    fn reload_resets_loaded_dir_and_noops_otherwise() {
        let mut root = dir_node("/r", vec![dir_node("/r/sub", vec![file_node("/r/sub/a.jpg")])]);
        // Re-list a loaded subdir: it drops to Unloaded (re-read next render).
        assert!(root.reload(Path::new("/r/sub")));
        let NodeKind::Dir { children: DirChildren::Loaded(c) } = &root.kind else {
            unreachable!()
        };
        assert!(matches!(c[0].kind, NodeKind::Dir { children: DirChildren::Unloaded }));

        // Absent path and not-yet-loaded folder are no-ops.
        assert!(!root.reload(Path::new("/r/zzz")));
        let mut unloaded = TreeNode::root(PathBuf::from("/r"));
        assert!(!unloaded.reload(Path::new("/r/sub")));
    }

    #[test]
    fn an_unreadable_folder_is_an_error_not_an_empty_listing() {
        let dir = tempdir().unwrap();
        touch(&dir.path().join("b.jpg"));
        touch(&dir.path().join("a.jpg"));
        touch(&dir.path().join("notes.txt"));
        let DirChildren::Loaded(nodes) = list_children(dir.path()) else {
            panic!("a readable folder lists");
        };
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["a.jpg", "b.jpg"]);

        // Gone (or unreadable, or on a dropped mount): say so.
        let missing = dir.path().join("missing");
        assert!(matches!(list_children(&missing), DirChildren::Error(_)));
    }

    #[test]
    fn reload_retries_a_folder_whose_listing_failed() {
        let mut root = dir_node("/r", vec![dir_node("/r/sub", Vec::new())]);
        let NodeKind::Dir { children: DirChildren::Loaded(c) } = &mut root.kind else {
            unreachable!()
        };
        c[0].kind = NodeKind::Dir { children: DirChildren::Error("denied".to_string()) };
        // A watcher event for the folder gives the listing another go.
        assert!(root.reload(Path::new("/r/sub")));
        let NodeKind::Dir { children: DirChildren::Loaded(c) } = &root.kind else {
            unreachable!()
        };
        assert!(matches!(c[0].kind, NodeKind::Dir { children: DirChildren::Unloaded }));
    }

    #[test]
    fn search_matches_uppercase_query_against_lowercase_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("photo.jpg"));
        // The query is lowercased too (not just the name), so an uppercase query
        // still matches a lowercase filename.
        assert_eq!(
            flatten(&search_tree(root, "PHOTO")),
            vec![("photo.jpg".to_string(), false)]
        );
    }

    /// Drive a headless two-axis ScrollArea for a few frames: centering a row
    /// far wider than the viewport must scroll vertically only.
    #[test]
    fn scroll_row_into_view_never_scrolls_horizontally() {
        let ctx = egui::Context::default();
        let mut offset = egui::Vec2::ZERO;
        for frame in 0..10 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(120.0, 100.0),
                )),
                // Jump a second per frame so the scroll animation finishes.
                time: Some(frame as f64),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    let out = egui::ScrollArea::both().show(ui, |ui| {
                        for i in 0..50 {
                            let name = format!("{i}-long-name-{}.jpg", "x".repeat(120));
                            let response = ui.selectable_label(false, name);
                            if i == 40 && frame == 0 {
                                scroll_row_into_view(ui, &response);
                            }
                        }
                    });
                    offset = out.state.offset;
                });
            });
        }
        assert!(offset.x.abs() < 0.5, "tree shifted horizontally: {offset:?}");
        assert!(offset.y > 0.0, "row 40 is off-screen, so it must scroll vertically");
    }

    #[test]
    fn is_image_accepts_uppercase_extensions() {
        assert!(is_image(Path::new("holiday.JPG")));
        assert!(is_image(Path::new("scan.HEIC")));
        assert!(!is_image(Path::new("notes.TXT")));
    }
}
