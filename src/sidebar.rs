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
    Dir {
        children: Option<Vec<TreeNode>>,
    },
}

pub struct SearchHit {
    path: PathBuf,
    name: String,
    kind: SearchKind,
}

enum SearchKind {
    File,
    Dir { children: Vec<SearchHit> },
}

impl SearchHit {
    pub(crate) fn file(path: PathBuf, name: String) -> Self {
        SearchHit { path, name, kind: SearchKind::File }
    }

    /// Build a directory hit, applying the prune rule: a folder is kept only if
    /// its own name matched or it has at least one kept descendant. Returns
    /// `None` when it should be dropped. Single-sources the keep/drop rule for
    /// both the local (`search_dir`) and remote walks.
    pub(crate) fn dir(
        path: PathBuf,
        name: String,
        matches: bool,
        children: Vec<SearchHit>,
    ) -> Option<Self> {
        if matches || !children.is_empty() {
            Some(SearchHit { path, name, kind: SearchKind::Dir { children } })
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
            kind: NodeKind::Dir { children: None },
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Walk the loaded subtree depth-first and collect every media file's full
    /// path (image or video). Folders whose children are `None` (not yet
    /// expanded) contribute nothing.
    pub fn collect_images(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        self.collect_images_into(&mut out);
        out
    }

    fn collect_images_into(&self, out: &mut Vec<PathBuf>) {
        match &self.kind {
            NodeKind::File => out.push(self.path.clone()),
            NodeKind::Dir { children: Some(children) } => {
                for child in children {
                    child.collect_images_into(out);
                }
            }
            NodeKind::Dir { children: None } => {}
        }
    }

    /// Remove the node at `target` from this loaded subtree, returning true once
    /// found. A folder whose children aren't loaded (or a path not present) is a
    /// no-op — it isn't on screen to remove.
    pub fn remove_path(&mut self, target: &Path) -> bool {
        let NodeKind::Dir { children: Some(children) } = &mut self.kind else {
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
                *children = None;
                return true;
            }
            return false;
        }
        if !target.starts_with(&self.path) {
            return false;
        }
        let NodeKind::Dir { children: Some(children) } = &mut self.kind else {
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
            NodeKind::Dir { children: None }
        } else {
            NodeKind::File
        };
        Self { path, name, kind }
    }
}

fn is_visible(path: &Path) -> bool {
    path.is_dir() || is_image(path) || crate::video::is_video(&path.to_string_lossy())
}

fn list_children(root: &Path) -> Vec<TreeNode> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut nodes: Vec<TreeNode> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_visible(p))
        .map(TreeNode::child)
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes
}

/// Recursively walk the filesystem under `root`, keeping only entries whose name
/// contains `query` (case-insensitive) plus the ancestor folders that lead to a
/// match. Unlike the live `TreeNode`, the result is fully materialized, so it can
/// never lazy-load an unfiltered directory when rendered.
pub fn search_tree(root: &Path, query: &str) -> Vec<SearchHit> {
    let query_lc = query.to_lowercase();
    let mut visited = HashSet::new();
    search_dir(root, &query_lc, &mut visited)
}

fn search_dir(dir: &Path, query_lc: &str, visited: &mut HashSet<PathBuf>) -> Vec<SearchHit> {
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
            let children = search_dir(&path, query_lc, visited);
            if let Some(hit) = SearchHit::dir(path, name, matches, children) {
                hits.push(hit);
            }
        } else if matches {
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
            render_file_row(
                ui,
                &node.path,
                &node.name,
                selected_image,
                scroll_target,
                new_selection,
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
                if children.is_none() {
                    *children = Some(list_children(&path));
                }
                if let Some(children) = children {
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
            });
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

fn render_file_row(
    ui: &mut egui::Ui,
    path: &Path,
    name: &str,
    selected_image: &Option<PathBuf>,
    scroll_target: &mut Option<PathBuf>,
    new_selection: &mut Option<PathBuf>,
    delete_request: &mut Option<(PathBuf, bool)>,
    rename_request: &mut Option<(PathBuf, bool)>,
) {
    let is_selected = selected_image.as_deref() == Some(path);
    let response = ui.selectable_label(is_selected, name);
    if scroll_target.as_deref() == Some(path) {
        response.scroll_to_me(Some(egui::Align::Center));
        *scroll_target = None;
    }
    if response.clicked() {
        *new_selection = Some(path.to_path_buf());
    }
    response.context_menu(|ui| {
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

/// Render pruned search results. Every folder is forced open (it only appears
/// because it or a descendant matched) under a `search:`-prefixed id, so this
/// transient expansion never touches the live tree's persisted state.
pub fn render_search_results(
    ui: &mut egui::Ui,
    hits: &[SearchHit],
    selected_image: &Option<PathBuf>,
    scroll_target: &mut Option<PathBuf>,
    new_selection: &mut Option<PathBuf>,
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
                    delete_request,
                    rename_request,
                );
            }
            SearchKind::Dir { children } => {
                egui::CollapsingHeader::new(&hit.name)
                    .id_salt(format!("search:{}", hit.path.display()))
                    .open(Some(true))
                    .show(ui, |ui| {
                        render_search_results(
                            ui,
                            children,
                            selected_image,
                            scroll_target,
                            new_selection,
                            delete_request,
                            rename_request,
                        );
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
                SearchKind::Dir { children } => {
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
    fn matching_folder_kept_with_empty_children() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("holiday")).unwrap();
        touch(&root.join("holiday/a.jpg"));
        touch(&root.join("holiday/b.jpg"));
        // The folder name matches; its children don't, so it is kept with no children.
        assert_eq!(
            flatten(&search_tree(root, "holiday")),
            vec![("holiday".to_string(), true)]
        );
    }

    #[test]
    fn matching_folder_keeps_only_matching_child() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("trip")).unwrap();
        touch(&root.join("trip/trip-photo.jpg"));
        touch(&root.join("trip/random.jpg"));
        assert_eq!(
            flatten(&search_tree(root, "trip")),
            vec![
                ("trip".to_string(), true),
                ("trip-photo.jpg".to_string(), false),
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
    fn matches_file_name_not_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("vacation")).unwrap();
        touch(&root.join("vacation/sunset.jpg"));
        // Query equals the ancestor folder name; the folder matches, but its
        // non-matching child is not pulled in via the path.
        assert_eq!(
            flatten(&search_tree(root, "vacation")),
            vec![("vacation".to_string(), true)]
        );
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
    fn dir_constructor_applies_prune_rule() {
        let p = || PathBuf::from("/x");
        let child = || SearchHit::file(PathBuf::from("/x/c.jpg"), "c.jpg".to_string());
        // matched folder, no children -> kept
        assert!(SearchHit::dir(p(), "x".to_string(), true, vec![]).is_some());
        // unmatched folder, no children -> dropped
        assert!(SearchHit::dir(p(), "x".to_string(), false, vec![]).is_none());
        // unmatched folder with a kept child -> kept as scaffolding
        assert!(SearchHit::dir(p(), "x".to_string(), false, vec![child()]).is_some());
        // matched folder with a child -> kept
        assert!(SearchHit::dir(p(), "x".to_string(), true, vec![child()]).is_some());
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
            kind: NodeKind::Dir { children: Some(children) },
        }
    }

    fn child_paths(node: &TreeNode) -> Vec<String> {
        match &node.kind {
            NodeKind::Dir { children: Some(c) } => {
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
        let NodeKind::Dir { children: Some(c) } = &root.kind else { unreachable!() };
        assert_eq!(child_paths(&c[0]), vec!["/r/sub/c.png"]);
    }

    #[test]
    fn remove_path_absent_or_unloaded_is_noop() {
        let mut root = dir_node("/r", vec![file_node("/r/a.jpg")]);
        assert!(!root.remove_path(Path::new("/r/zzz.jpg")));
        assert_eq!(child_paths(&root), vec!["/r/a.jpg"]);

        // A folder whose children haven't been loaded yet (children: None).
        let mut unloaded = TreeNode::root(PathBuf::from("/r"));
        assert!(!unloaded.remove_path(Path::new("/r/a.jpg")));
    }

    #[test]
    fn reload_resets_loaded_dir_and_noops_otherwise() {
        let mut root = dir_node("/r", vec![dir_node("/r/sub", vec![file_node("/r/sub/a.jpg")])]);
        // Re-list a loaded subdir: its children drop to None (re-read next render).
        assert!(root.reload(Path::new("/r/sub")));
        let NodeKind::Dir { children: Some(c) } = &root.kind else { unreachable!() };
        assert!(matches!(c[0].kind, NodeKind::Dir { children: None }));

        // Absent path and not-yet-loaded folder are no-ops.
        assert!(!root.reload(Path::new("/r/zzz")));
        let mut unloaded = TreeNode::root(PathBuf::from("/r"));
        assert!(!unloaded.reload(Path::new("/r/sub")));
    }
}
