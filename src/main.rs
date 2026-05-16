use eframe::egui;
use std::fs;
use std::path::PathBuf;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Twelf",
        options,
        Box::new(|_cc| Ok(Box::new(TwelfApp::new()))),
    )
}

struct TwelfApp {
    entries: Vec<String>,
}

impl TwelfApp {
    fn new() -> Self {
        Self {
            entries: list_entries(),
        }
    }
}

fn list_entries() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let root = PathBuf::from(home).join("Pictures");
    let Ok(entries) = fs::read_dir(&root) else {
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
        egui::SidePanel::left("entries").show(ctx, |ui| {
            for name in &self.entries {
                ui.label(name);
            }
        });
        egui::CentralPanel::default().show(ctx, |_ui| {});
    }
}
