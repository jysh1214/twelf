use crate::TwelfApp;
use eframe::egui;

pub fn render(app: &mut TwelfApp, ctx: &egui::Context) {
    let prev_zoom = app.zoom;
    let prev_displayed = app.last_displayed.clone();
    let current_displayed = app
        .selected_remote
        .clone()
        .or_else(|| app.selected_image.clone());
    if current_displayed != app.last_displayed {
        app.zoom = 1.0;
        app.last_displayed = current_displayed;
        app.animation = load_animation(app);
    }
    let zoom_scroll = ctx.input(|i| {
        if i.modifiers.ctrl {
            i.raw_scroll_delta.y
        } else {
            0.0
        }
    });
    if zoom_scroll != 0.0 {
        app.zoom = (app.zoom * (1.0 + zoom_scroll * 0.01)).clamp(0.1, 10.0);
    }
    let recenter_image =
        (app.zoom - prev_zoom).abs() > f32::EPSILON || app.last_displayed != prev_displayed;

    egui::CentralPanel::default().show(ctx, |ui| {
        let uri = if let Some(path) = &app.selected_remote {
            let host = match &app.ssh {
                crate::ssh::SshState::Connected { info, .. } => info.host.as_str(),
                _ => "",
            };
            Some(format!("sftp://{host}{}", path.display()))
        } else {
            app.selected_image
                .as_ref()
                .map(|path| format!("file://{}", path.display()))
        };
        if let Some(uri) = uri {
            let panel_avail = ui.available_size();
            let image_size = panel_avail * app.zoom;
            let content_size = egui::vec2(
                image_size.x.max(panel_avail.x),
                image_size.y.max(panel_avail.y),
            );
            let mut scroll_area = egui::ScrollArea::both();
            if recenter_image {
                scroll_area = scroll_area.scroll_offset(egui::vec2(
                    (content_size.x - panel_avail.x) * 0.5,
                    (content_size.y - panel_avail.y) * 0.5,
                ));
            }
            scroll_area.show(ui, |ui| {
                ui.allocate_ui_with_layout(
                    content_size,
                    egui::Layout::centered_and_justified(egui::Direction::TopDown),
                    |ui| {
                        let image = match app.animation.as_mut().filter(|a| a.uri == uri) {
                            Some(anim) => {
                                let (texture, remaining) = anim.frame(ui.ctx());
                                ui.ctx().request_repaint_after(remaining);
                                egui::Image::new(texture)
                            }
                            None => egui::Image::new(uri),
                        };
                        ui.add(image.max_size(image_size).maintain_aspect_ratio(true));
                    },
                );
            });
        }
    });
}

/// Build an animation player for the current selection when it is a local,
/// multi-frame WebP; otherwise `None` so the still-image path is used.
fn load_animation(app: &TwelfApp) -> Option<crate::webp::Animation> {
    if app.selected_remote.is_some() {
        return None;
    }
    let path = app.selected_image.as_ref()?;
    if !crate::webp::is_webp(&path.to_string_lossy()) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let frames = crate::webp::decode_frames(&bytes).ok()?;
    if frames.len() <= 1 {
        return None;
    }
    Some(crate::webp::Animation::new(
        format!("file://{}", path.display()),
        frames,
    ))
}
