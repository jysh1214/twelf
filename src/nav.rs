use crate::sidebar;
use std::path::{Path, PathBuf};

pub struct Navigator {
    image_list: Option<Vec<PathBuf>>,
    scroll_to_selected: bool,
}

impl Navigator {
    pub fn new() -> Self {
        Self {
            image_list: None,
            scroll_to_selected: false,
        }
    }

    pub fn invalidate(&mut self) {
        self.image_list = None;
    }

    pub fn take_scroll_flag(&mut self) -> bool {
        std::mem::replace(&mut self.scroll_to_selected, false)
    }

    pub fn navigate(&mut self, root: &Path, current: &Path, delta: i32) -> Option<PathBuf> {
        if self.image_list.is_none() {
            self.image_list = Some(sidebar::collect_images(root));
        }
        let images = self.image_list.as_ref()?;
        if images.is_empty() {
            return None;
        }
        let idx = images.iter().position(|p| p.as_path() == current)?;
        let len = images.len() as i32;
        let new_idx = (idx as i32 + delta).rem_euclid(len) as usize;
        self.scroll_to_selected = true;
        Some(images[new_idx].clone())
    }
}
