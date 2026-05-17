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
