use eframe::egui;
use egui::load::{ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use egui::{ColorImage, Context};
use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct HeicLoader {
    cache: Mutex<HashMap<String, Arc<ColorImage>>>,
}

impl HeicLoader {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl ImageLoader for HeicLoader {
    fn id(&self) -> &str {
        concat!(module_path!(), "::HeicLoader")
    }

    fn load(&self, _ctx: &Context, uri: &str, _size_hint: SizeHint) -> ImageLoadResult {
        let lower = uri.to_ascii_lowercase();
        if !(lower.ends_with(".heic") || lower.ends_with(".heif")) {
            return Err(LoadError::NotSupported);
        }
        if let Some(cached) = self.cache.lock().unwrap().get(uri).cloned() {
            return Ok(ImagePoll::Ready { image: cached });
        }
        let path = uri.strip_prefix("file://").unwrap_or(uri);
        let image = decode_heic(path).map_err(|e| LoadError::Loading(e.to_string()))?;
        let arc = Arc::new(image);
        self.cache.lock().unwrap().insert(uri.to_owned(), arc.clone());
        Ok(ImagePoll::Ready { image: arc })
    }

    fn forget(&self, uri: &str) {
        self.cache.lock().unwrap().remove(uri);
    }

    fn forget_all(&self) {
        self.cache.lock().unwrap().clear();
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

fn decode_heic(path: &str) -> Result<ColorImage, libheif_rs::HeifError> {
    let lib = LibHeif::new();
    let ctx = HeifContext::read_from_file(path)?;
    let handle = ctx.primary_image_handle()?;
    let image = lib.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)?;
    let width = image.width() as usize;
    let height = image.height() as usize;
    let planes = image.planes();
    let plane = planes
        .interleaved
        .expect("RGBA decode should produce an interleaved plane");
    let stride = plane.stride;
    let row_bytes = width * 4;
    let mut pixels = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let start = y * stride;
        pixels.extend_from_slice(&plane.data[start..start + row_bytes]);
    }
    Ok(ColorImage::from_rgba_unmultiplied(
        [width, height],
        &pixels,
    ))
}
