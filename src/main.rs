mod fonts;
mod sidebar;

use eframe::egui;
use std::path::PathBuf;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Twelf",
        options,
        Box::new(|cc| {
            // Must run before any `egui::Image` is rendered.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            fonts::apply_fonts(&cc.egui_ctx);
            Ok(Box::new(TwelfApp::new()))
        }),
    )
}

struct TwelfApp {
    root_node: Option<sidebar::TreeNode>,
    selected_image: Option<PathBuf>,
}

impl TwelfApp {
    fn new() -> Self {
        Self {
            root_node: None,
            selected_image: None,
        }
    }
}

impl eframe::App for TwelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open Folder").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.root_node = Some(sidebar::TreeNode::root(path));
                            self.selected_image = None;
                        }
                        ui.close();
                    }
                });
            });
        });
        egui::SidePanel::left("entries").show(ctx, |ui| {
            // Outer = a click happened; inner = the new selection (Some=show, None=clear).
            // Deferred to dodge the borrow on `&mut self.root_node`.
            let mut new_selection: Option<Option<PathBuf>> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(root_node) = &mut self.root_node {
                    sidebar::render_tree(
                        ui,
                        root_node,
                        true,
                        &self.selected_image,
                        &mut new_selection,
                    );
                }
            });
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
