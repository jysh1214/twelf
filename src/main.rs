mod fonts;
mod heic;
mod nav;
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
            cc.egui_ctx
                .add_image_loader(std::sync::Arc::new(heic::HeicLoader::new()));
            fonts::apply_fonts(&cc.egui_ctx);
            Ok(Box::new(TwelfApp::new()))
        }),
    )
}

struct TwelfApp {
    root_node: Option<sidebar::TreeNode>,
    selected_image: Option<PathBuf>,
    nav: nav::Navigator,
}

impl TwelfApp {
    fn new() -> Self {
        Self {
            root_node: None,
            selected_image: None,
            nav: nav::Navigator::new(),
        }
    }

    fn navigate_image(&mut self, delta: i32) {
        let Some(root) = self.root_node.as_ref() else { return };
        let Some(current) = self.selected_image.clone() else { return };
        if let Some(new) = self.nav.navigate(root.path(), &current, delta) {
            self.selected_image = Some(new);
        }
    }
}

impl eframe::App for TwelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let nav_delta = ctx.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowUp) {
                Some(-1_i32)
            } else if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowDown) {
                Some(1)
            } else {
                None
            }
        });
        if let Some(delta) = nav_delta {
            self.navigate_image(delta);
        }

        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open Folder").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.root_node = Some(sidebar::TreeNode::root(path));
                            self.selected_image = None;
                            self.nav.invalidate();
                        }
                        ui.close();
                    }
                });
            });
        });
        egui::SidePanel::left("entries").show(ctx, |ui| {
            ui.set_min_width(ui.available_width());
            // Captures the clicked image path — deferred to dodge the borrow
            // on `&mut self.root_node` taken by `render_tree`.
            let mut new_selection: Option<PathBuf> = None;
            let scroll_to_selected = self.nav.take_scroll_flag();
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(root_node) = &mut self.root_node {
                    sidebar::render_tree(
                        ui,
                        root_node,
                        true,
                        &self.selected_image,
                        scroll_to_selected,
                        &mut new_selection,
                    );
                }
            });
            if let Some(path) = new_selection {
                self.selected_image = Some(path);
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(path) = &self.selected_image {
                ui.centered_and_justified(|ui| {
                    ui.add(
                        egui::Image::new(format!("file://{}", path.display()))
                            .max_size(ui.available_size())
                            .maintain_aspect_ratio(true),
                    );
                });
            }
        });
    }
}
