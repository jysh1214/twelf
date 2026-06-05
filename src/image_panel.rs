use crate::TwelfApp;
use eframe::egui;

pub fn render(app: &mut TwelfApp, ctx: &egui::Context) {
    let prev_zoom = app.zoom;
    let prev_displayed = app.last_displayed.clone();
    let current_displayed = app
        .selected_remote
        .clone()
        .or_else(|| app.selected_image.clone());
    let uri = selected_uri(app);
    if current_displayed != app.last_displayed {
        app.zoom = 1.0;
        app.last_displayed = current_displayed;
        app.animation = None;
        app.anim_pending = uri.clone();
        app.video = open_video(app);
    }
    // Build the animation once its bytes are available. Remote bytes arrive
    // asynchronously, so keep retrying each frame until the decode resolves to
    // an animation or a still image.
    if let Some(pending) = app.anim_pending.clone() {
        match build_animation(ctx, &pending) {
            AnimBuild::Pending => {}
            AnimBuild::Resolved(anim) => {
                app.animation = anim;
                app.anim_pending = None;
            }
        }
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
        if let Some(uri) = uri {
            let panel_rect = ui.max_rect();
            let panel_avail = ui.available_size();
            let image_size = panel_avail * app.zoom;
            let content_size = egui::vec2(
                image_size.x.max(panel_avail.x),
                image_size.y.max(panel_avail.y),
            );
            let video_active = app.video.as_ref().is_some_and(|p| p.uri == uri);
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
                        let image = if let Some(player) =
                            app.video.as_mut().filter(|p| p.uri == uri)
                        {
                            match player.frame(ui.ctx()) {
                                Some((texture, delay)) => {
                                    ui.ctx().request_repaint_after(delay);
                                    Some(egui::Image::new(texture))
                                }
                                None => {
                                    ui.ctx().request_repaint(); // decoding; nothing yet
                                    None
                                }
                            }
                        } else if let Some(anim) =
                            app.animation.as_mut().filter(|a| a.uri == uri)
                        {
                            let (texture, remaining) = anim.frame(ui.ctx());
                            ui.ctx().request_repaint_after(remaining);
                            Some(egui::Image::new(texture))
                        } else {
                            Some(egui::Image::new(uri))
                        };
                        if let Some(image) = image {
                            ui.add(image.max_size(image_size).maintain_aspect_ratio(true));
                        }
                    },
                );
            });
            if video_active && let Some(player) = app.video.as_mut() {
                let label = if player.is_paused() { "▶" } else { "⏸" };
                // Anchor to the central panel's bottom-center, not the whole window
                // (the side panel would otherwise pull it off-center).
                let screen = ui.ctx().content_rect();
                let offset = egui::vec2(
                    panel_rect.center().x - screen.center().x,
                    panel_rect.bottom() - screen.bottom() - 16.0,
                );
                egui::Area::new(egui::Id::new("video_controls"))
                    .anchor(egui::Align2::CENTER_BOTTOM, offset)
                    .show(ui.ctx(), |ui| {
                        if ui.button(label).clicked() {
                            player.toggle_pause();
                        }
                    });
            }
        }
    });
}

fn selected_uri(app: &TwelfApp) -> Option<String> {
    if let Some(path) = &app.selected_remote {
        let host = match &app.ssh {
            crate::ssh::SshState::Connected { info, .. } => info.host.as_str(),
            _ => "",
        };
        Some(format!("sftp://{host}{}", path.display()))
    } else {
        app.selected_image
            .as_ref()
            .map(|path| format!("file://{}", path.display()))
    }
}

enum AnimBuild {
    /// Bytes are not available yet; retry on a later frame.
    Pending,
    /// Decided: `Some` plays as an animation, `None` falls back to the still path.
    Resolved(Option<crate::webp::Animation>),
}

/// Try to build a multi-frame WebP player for `uri`. Local files read
/// synchronously; remote files draw bytes from the async SFTP loader cache.
fn build_animation(ctx: &egui::Context, uri: &str) -> AnimBuild {
    if !crate::webp::is_webp(uri) {
        return AnimBuild::Resolved(None);
    }
    if let Some(path) = uri.strip_prefix("file://") {
        match std::fs::read(path) {
            Ok(bytes) => AnimBuild::Resolved(animation_from_bytes(uri, &bytes)),
            Err(_) => AnimBuild::Resolved(None),
        }
    } else if uri.starts_with("sftp://") {
        match ctx.try_load_bytes(uri) {
            Ok(egui::load::BytesPoll::Ready { bytes, .. }) => {
                AnimBuild::Resolved(animation_from_bytes(uri, bytes.as_ref()))
            }
            Ok(egui::load::BytesPoll::Pending { .. }) => AnimBuild::Pending,
            Err(_) => AnimBuild::Resolved(None),
        }
    } else {
        AnimBuild::Resolved(None)
    }
}

fn animation_from_bytes(uri: &str, bytes: &[u8]) -> Option<crate::webp::Animation> {
    let frames = crate::webp::decode_frames(bytes).ok()?;
    if frames.len() <= 1 {
        return None;
    }
    Some(crate::webp::Animation::new(uri.to_string(), frames))
}

/// Start a player for the current selection when it is a video file, local or
/// remote (the remote case downloads over SFTP before decoding).
fn open_video(app: &TwelfApp) -> Option<crate::video::VideoPlayer> {
    if let Some(path) = &app.selected_remote {
        let crate::ssh::SshState::Connected { session, info } = &app.ssh else {
            return None;
        };
        let uri = format!("sftp://{}{}", info.host, path.display());
        if !crate::video::is_video(&uri) {
            return None;
        }
        return Some(crate::video::VideoPlayer::open_remote(
            uri,
            session.clone(),
            app.runtime.handle().clone(),
            path.to_string_lossy().into_owned(),
        ));
    }
    let path = app.selected_image.as_ref()?;
    let uri = format!("file://{}", path.display());
    if !crate::video::is_video(&uri) {
        return None;
    }
    Some(crate::video::VideoPlayer::open(uri, path.clone()))
}
