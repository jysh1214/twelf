use std::path::{Path, PathBuf};

pub struct Navigator {
    scroll_to_selected: bool,
}

impl Navigator {
    pub fn new() -> Self {
        Self {
            scroll_to_selected: false,
        }
    }

    pub fn take_scroll_flag(&mut self) -> bool {
        std::mem::replace(&mut self.scroll_to_selected, false)
    }

    pub fn navigate(
        &mut self,
        image_list: &[PathBuf],
        current: &Path,
        delta: i32,
    ) -> Option<PathBuf> {
        if image_list.is_empty() {
            return None;
        }
        let idx = image_list.iter().position(|p| p.as_path() == current)?;
        let len = image_list.len() as i32;
        let new_idx = (idx as i32 + delta).rem_euclid(len) as usize;
        self.scroll_to_selected = true;
        Some(image_list[new_idx].clone())
    }
}
