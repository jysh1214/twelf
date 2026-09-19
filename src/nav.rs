use std::path::{Path, PathBuf};

pub fn navigate(image_list: &[PathBuf], current: &Path, delta: i32) -> Option<PathBuf> {
    if image_list.is_empty() {
        return None;
    }
    let idx = image_list.iter().position(|p| p.as_path() == current)?;
    let len = image_list.len() as i32;
    let new_idx = (idx as i32 + delta).rem_euclid(len) as usize;
    Some(image_list[new_idx].clone())
}

/// Where an arrow key lands when the current selection is not in `image_list`
/// at all — a selection made in the tree, with the search results now on
/// screen: forward enters the list at its top, backward at its bottom.
pub fn enter(image_list: &[PathBuf], delta: i32) -> Option<PathBuf> {
    if delta >= 0 {
        image_list.first().cloned()
    } else {
        image_list.last().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entering_a_list_starts_at_the_end_being_walked_from() {
        let l = list();
        assert_eq!(enter(&l, 1), Some(PathBuf::from("a.jpg")));
        assert_eq!(enter(&l, -1), Some(PathBuf::from("c.jpg")));
        assert_eq!(enter(&[], 1), None);
    }

    fn list() -> Vec<PathBuf> {
        ["a.jpg", "b.jpg", "c.jpg"]
            .iter()
            .map(PathBuf::from)
            .collect()
    }

    #[test]
    fn steps_forward_and_backward() {
        let l = list();
        assert_eq!(
            navigate(&l, Path::new("a.jpg"), 1),
            Some(PathBuf::from("b.jpg"))
        );
        assert_eq!(
            navigate(&l, Path::new("b.jpg"), -1),
            Some(PathBuf::from("a.jpg"))
        );
    }

    #[test]
    fn wraps_around_both_ends() {
        let l = list();
        // Past the end wraps to the start…
        assert_eq!(
            navigate(&l, Path::new("c.jpg"), 1),
            Some(PathBuf::from("a.jpg"))
        );
        // …and before the start wraps to the end (negative rem_euclid, not `%`).
        assert_eq!(
            navigate(&l, Path::new("a.jpg"), -1),
            Some(PathBuf::from("c.jpg"))
        );
    }

    #[test]
    fn empty_list_or_absent_current_yields_none() {
        assert_eq!(navigate(&[], Path::new("a.jpg"), 1), None);
        assert_eq!(navigate(&list(), Path::new("missing.jpg"), 1), None);
    }
}
