use crate::backoff::BackOff;
use eframe::egui;
use egui::load::{ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use egui::{ColorImage, Context};
use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a HEIC that failed to decode is left alone before being retried.
const DECODE_RETRY_BACKOFF: Duration = Duration::from_secs(30);

pub struct HeicLoader {
    cache: Mutex<HashMap<String, Arc<ColorImage>>>,
    /// Decoding runs synchronously on the UI thread and egui does not memoise a
    /// loader error, so without this a file libheif cannot read is re-read and
    /// re-decoded on every single repaint for as long as it stays selected.
    failed: Mutex<BackOff>,
}

impl HeicLoader {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            failed: Mutex::new(BackOff::new(DECODE_RETRY_BACKOFF)),
        }
    }
}

impl ImageLoader for HeicLoader {
    fn id(&self) -> &str {
        concat!(module_path!(), "::HeicLoader")
    }

    fn load(&self, _ctx: &Context, uri: &str, _size_hint: SizeHint) -> ImageLoadResult {
        if !is_heic(uri) {
            return Err(LoadError::NotSupported);
        }
        if let Some(cached) = self.cache.lock().unwrap().get(uri).cloned() {
            return Ok(ImagePoll::Ready { image: cached });
        }
        if self.failed.lock().unwrap().is_backed_off(uri) {
            return Err(LoadError::Loading("previous decode failed".to_string()));
        }
        let path = uri.strip_prefix("file://").unwrap_or(uri);
        let image = match crate::decoded::catching_panics(|| decode_heic(path)) {
            Ok(image) => image,
            Err(e) => {
                self.failed.lock().unwrap().record(uri.to_owned());
                return Err(LoadError::Loading(e));
            }
        };
        let arc = Arc::new(image);
        self.cache
            .lock()
            .unwrap()
            .insert(uri.to_owned(), arc.clone());
        Ok(ImagePoll::Ready { image: arc })
    }

    fn forget(&self, uri: &str) {
        self.cache.lock().unwrap().remove(uri);
        self.failed.lock().unwrap().clear(uri);
    }

    fn forget_all(&self) {
        self.cache.lock().unwrap().clear();
        self.failed.lock().unwrap().clear_all();
    }

    fn byte_size(&self) -> usize {
        self.cache
            .lock()
            .unwrap()
            .values()
            .map(|img| img.size[0] * img.size[1] * 4)
            .sum()
    }
}

pub fn is_heic(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.ends_with(".heic") || lower.ends_with(".heif")
}

fn decode_heic(path: &str) -> Result<ColorImage, String> {
    let ctx = HeifContext::read_from_file(path).map_err(|e| e.to_string())?;
    heif_to_image(&ctx)
}

pub fn decode_bytes(bytes: &[u8]) -> Result<ColorImage, String> {
    let ctx = HeifContext::read_from_bytes(bytes).map_err(|e| e.to_string())?;
    heif_to_image(&ctx)
}

/// Every way this can go wrong is an `Err`, never a panic: for a local file it
/// runs on the UI thread, where a panic takes the whole app down — over one
/// picture libheif decoded into a shape this code did not expect.
fn heif_to_image(ctx: &HeifContext) -> Result<ColorImage, String> {
    let lib = LibHeif::new();
    let handle = ctx.primary_image_handle().map_err(|e| e.to_string())?;
    let image = lib
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)
        .map_err(|e| e.to_string())?;
    let width = image.width() as usize;
    let height = image.height() as usize;
    let planes = image.planes();
    let plane = planes
        .interleaved
        .ok_or("libheif returned no interleaved RGBA plane")?;
    copy_rows(plane.data, plane.stride, width, height)
        .map(|pixels| ColorImage::from_rgba_unmultiplied([width, height], &pixels))
}

/// Copy `height` rows of `width` RGBA pixels out of a plane whose rows are
/// `stride` bytes apart, dropping the row padding. A plane too short for the
/// size it claims is an error rather than a slice out of bounds.
fn copy_rows(data: &[u8], stride: usize, width: usize, height: usize) -> Result<Vec<u8>, String> {
    let row_bytes = width * 4;
    let mut pixels = Vec::with_capacity(row_bytes * height);
    for y in 0..height {
        let start = y * stride;
        let row = data
            .get(start..start + row_bytes)
            .ok_or("libheif returned a plane shorter than the image it describes")?;
        pixels.extend_from_slice(row);
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plane_is_copied_without_its_row_padding() {
        // 2x2 pixels, rows 12 bytes apart: 8 of pixels, 4 of padding.
        let mut data = vec![0xEEu8; 24];
        data[0..8].copy_from_slice(&[1; 8]);
        data[12..20].copy_from_slice(&[2; 8]);
        let pixels = copy_rows(&data, 12, 2, 2).expect("fits");
        assert_eq!(pixels, [[1u8; 8], [2u8; 8]].concat());
    }

    #[test]
    fn a_plane_too_short_for_its_size_is_an_error_not_a_panic() {
        // Claims 2 rows but holds little more than one.
        assert!(copy_rows(&[0u8; 14], 12, 2, 2).is_err());
        // No plane data at all.
        assert!(copy_rows(&[], 8, 2, 1).is_err());
    }

    #[test]
    fn is_heic_accepts_both_extensions_in_any_case() {
        assert!(is_heic("file:///a/photo.heic"));
        // .heif is the other half of the format the decoder handles.
        assert!(is_heic("file:///a/photo.heif"));
        // Extensions off a filesystem or a server are not reliably lowercase.
        assert!(is_heic("file:///a/PHOTO.HEIC"));
        assert!(is_heic("sftp://nas/a/Photo.Heif"));
        assert!(!is_heic("file:///a/photo.jpg"));
    }
}
