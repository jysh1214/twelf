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
                        app.remote_root = None;
                        app.selected_remote = None;
                        *app.session_holder.lock().unwrap() = None;
                    }
                    ui.close();
                }
                if ui.button("Connect SSH").clicked() {
                    app.ssh_dialog.open = true;
                    ui.close();
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
