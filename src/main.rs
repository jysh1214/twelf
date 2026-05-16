use eframe::egui;
use std::fs;
use std::path::{Path, PathBuf};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Twelf",
        options,
        Box::new(|cc| {
            // Must run before any `egui::Image` is rendered.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(TwelfApp::new()))
        }),
    )
}

struct TwelfApp {
    root: Option<PathBuf>,
    entries: Vec<String>,
    selected_image: Option<PathBuf>,
}

impl TwelfApp {
    fn new() -> Self {
        Self {
            root: None,
            entries: Vec::new(),
            selected_image: None,
        }
    }
}

fn list_entries(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp")
    )
}

impl eframe::App for TwelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open Folder").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.entries = list_entries(&path);
                            self.root = Some(path);
                            self.selected_image = None;
                        }
                        ui.close();
                    }
                });
            });
        });
        egui::SidePanel::left("entries").show(ctx, |ui| {
            if let Some(root) = &self.root {
                ui.heading(root.display().to_string());
                ui.separator();
            }
            // Outer = a click happened; inner = the new selection (Some=show, None=clear).
            // Deferred to dodge the borrow on `&self.entries`.
            let mut new_selection: Option<Option<PathBuf>> = None;
            for name in &self.entries {
                let full = self.root.as_ref().map(|r| r.join(name));
                let is_selected = full.as_deref() == self.selected_image.as_deref();
                if ui.selectable_label(is_selected, name).clicked() {
                    if let Some(p) = full {
                        new_selection = Some(if is_image(&p) { Some(p) } else { None });
                    }
                }
            }
            if let Some(sel) = new_selection {
                self.selected_image = sel;
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(path) = &self.selected_image {
                ui.add(
                    egui::Image::new(format!("file://{}", path.display()))
                        .max_size(ui.available_size())
                        .maintain_aspect_ratio(true),
                );
            }
        });
    }
}
