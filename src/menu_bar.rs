use crate::{TwelfApp, sidebar, ssh};
use eframe::egui;

pub fn render(app: &mut TwelfApp, ctx: &egui::Context) {
    egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open Folder").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        app.root_node = Some(sidebar::TreeNode::root(path));
                        app.selected_image = None;
                        app.scroll_target = None;
                        app.search_active = false;
                        app.search_query.clear();
                        app.search_cache = None;
                        app.remote_search = None;
                        app.remote_search_changed = None;
                        app.remote_root = None;
                        app.selected_remote = None;
                        *app.session_holder.lock().unwrap() = None;
                        ctx.forget_all_images();
                    }
                    ui.close();
                }
                if ui.button("Connect SSH").clicked() {
                    app.ssh_dialog.open = true;
                    ui.close();
                }
            });
            ui.menu_button("Cache", |ui| {
                if app.cache.is_initialized() {
                    ui.label(format!("Size: {}", format_bytes(app.cache.total_size_bytes())));
                    ui.separator();
                    if ui.button("Clear Cache").clicked() {
                        app.cache.clear();
                        ctx.forget_all_images();
                        ui.close();
                    }
                } else {
                    ui.label("Not initialized");
                }
            });
            let status = match &app.ssh {
                ssh::SshState::Disconnected => String::new(),
                ssh::SshState::Connecting => "Connecting…".to_string(),
                ssh::SshState::Connected { info, .. } => {
                    format!("Connected: {}@{}:{}", info.user, info.host, info.port)
                }
                ssh::SshState::Failed { error } => format!("SSH error: {error}"),
            };
            if !status.is_empty() {
                ui.label(status);
            }
        });
    });
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n >= GB {
        format!("{:.2} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}
