use crate::{TwelfApp, sidebar, ssh};
use eframe::egui;

pub fn render(app: &mut TwelfApp, ctx: &egui::Context) {
    egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open Folder").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        app.fs_watcher = match crate::watcher::FsWatcher::spawn(&path, ctx) {
                            Ok(watcher) => Some(watcher),
                            Err(e) => {
                                app.status_message =
                                    Some(crate::status_bar::Message::error(format!(
                                        "Not watching {} for changes ({e}). \
                                     Right-click a folder and Refresh to re-list it",
                                        path.display()
                                    )));
                                None
                            }
                        };
                        app.root_node = Some(sidebar::TreeNode::root(path));
                        app.selected_image = None;
                        // Browsing locally now. Without this the menu bar went on
                        // saying "Connected" over a local tree, with the session
                        // kept alive and no way back to it short of reconnecting.
                        app.leave_remote_session(ctx);
                        app.ssh = ssh::SshState::Disconnected;
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
                    ui.label(format!(
                        "Size: {}",
                        format_bytes(app.cache.total_size_bytes())
                    ));
                    ui.separator();
                    if ui.button("Clear Cache").clicked() {
                        // Every key's blobs go, which can be a lot of unlinking:
                        // not on the update loop.
                        let cache = app.cache.clone();
                        app.runtime.spawn_blocking(move || cache.clear());
                        app.forget_all_images(ctx);
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

pub(crate) fn format_bytes(n: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_each_unit_at_its_threshold() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B"); // just below KB
        assert_eq!(format_bytes(1024), "1.0 KB"); // exact KB threshold (>=)
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
    }
}
