use eframe::egui;
use std::fs;
use std::path::{Path, PathBuf};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Twelf",
        options,
        Box::new(|_cc| Ok(Box::new(TwelfApp::new()))),
    )
}

struct TwelfApp {
    root: Option<PathBuf>,
    entries: Vec<String>,
}

impl TwelfApp {
    fn new() -> Self {
        Self {
            root: None,
            entries: Vec::new(),
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

impl eframe::App for TwelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open Folder").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.entries = list_entries(&path);
                            self.root = Some(path);
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
            for name in &self.entries {
                ui.label(name);
            }
        });
        egui::CentralPanel::default().show(ctx, |_ui| {});
    }
}
