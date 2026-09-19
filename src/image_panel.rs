use crate::TwelfApp;
use eframe::egui;
use std::collections::VecDeque;
use std::time::Duration;

/// How many recently-displayed images keep their decoded copy and GPU texture.
///
/// egui never evicts these on its own: a raster image occupies a texture bucket
/// of exactly one entry and `DefaultTextureLoader` only prunes a bucket holding
/// two or more, while `reduce_texture_memory` (off by default) is what would
/// drop the `egui_extras` byte and `ColorImage` copies. Without a window, every
/// image ever shown is retained for the whole session — roughly 100 MB per 12 MP
/// photo across all three layers, so a few hundred arrow-key presses exhaust RAM
/// and VRAM. The window is wide enough that stepping back and forth, and the
/// neighbours `drain_image_prefetch` warms, stay resident.
const DISPLAYED_WINDOW: usize = 8;

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
        if let Some(uri) = &uri {
            retain_displayed(app, uri, ctx);
        }
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
        app.zoom = zoom_after_scroll(app.zoom, zoom_scroll);
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
                            let frame = player.frame(ui.ctx());
                            if let Some(error) = player.error() {
                                // Worker died: show why; repaint only on input.
                                ui.colored_label(egui::Color32::RED, error);
                                None
                            } else {
                                match frame {
                                    Some((texture, delay)) => {
                                        ui.ctx().request_repaint_after(delay);
                                        Some(egui::Image::new(texture))
                                    }
                                    None => {
                                        // Decoding; nothing to show yet.
                                        ui.ctx().request_repaint_after(Duration::from_millis(50));
                                        None
                                    }
                                }
                            }
                        } else if let Some(anim) = app.animation.as_mut().filter(|a| a.uri == uri) {
                            let (texture, remaining) = anim.frame(ui.ctx());
                            ui.ctx().request_repaint_after(remaining);
                            Some(egui::Image::new(texture))
                        } else if !crate::video::is_video(&uri) {
                            Some(egui::Image::new(uri))
                        } else {
                            // A video with no live player (e.g. disconnected): the
                            // image loaders must not fetch it whole.
                            None
                        };
                        if let Some(image) = image {
                            ui.add(image.max_size(image_size).maintain_aspect_ratio(true));
                        }
                    },
                );
            });
            if video_active
                && let Some(player) = app.video.as_mut()
                && player.error().is_none()
            {
                // Anchor to the central panel's bottom-center, not the whole window
                // (the side panel would otherwise pull it off-center).
                let screen = ui.ctx().content_rect();
                let offset = egui::vec2(
                    panel_rect.center().x - screen.center().x,
                    panel_rect.bottom() - screen.bottom() - 16.0,
                );
                let bar_width = (panel_rect.width() * 0.6).clamp(120.0, 640.0);
                egui::Area::new(egui::Id::new("video_controls"))
                    .anchor(egui::Align2::CENTER_BOTTOM, offset)
                    .show(ui.ctx(), |ui| {
                        ui.horizontal(|ui| {
                            let label = if player.is_paused() { "▶" } else { "⏸" };
                            if ui.button(label).clicked() {
                                player.toggle_pause();
                            }
                            let duration = player.duration();
                            if duration > 0.0 {
                                draw_seek_bar(ui, player, duration, bar_width);
                            }
                        });
                    });
            }
        }
    });
}

/// Draw a draggable progress bar filled to the player's position; seek on release.
fn draw_seek_bar(
    ui: &mut egui::Ui,
    player: &mut crate::video::VideoPlayer,
    duration: f64,
    width: f32,
) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, 10.0), egui::Sense::click_and_drag());
    let played = (player.position() / duration).clamp(0.0, 1.0) as f32;
    let pointer_frac = response
        .interact_pointer_pos()
        .map(|p| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0));
    // While dragging, preview the dragged position; otherwise show playback.
    let shown = if response.dragged() {
        pointer_frac.unwrap_or(played)
    } else {
        played
    };
    let bg = ui.visuals().extreme_bg_color;
    let fg = ui.visuals().selection.bg_fill;
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0_f32, bg);
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * shown, rect.height()));
    painter.rect_filled(fill, 4.0_f32, fg);
    if (response.drag_stopped() || response.clicked())
        && let Some(frac) = pointer_frac
    {
        player.seek(frac as f64 * duration);
    }
}

/// The zoom after Ctrl-scrolling by `delta` points. Exponential in the delta, so
/// equal scrolls up and down cancel out and no delta can reach zero. The linear
/// `1 + delta * 0.01` it replaces went to zero or below on a fast wheel (three
/// notches in one frame is -120) and slammed the zoom to its minimum, and a
/// notch up then down multiplied out to 1.4 * 0.6 = 0.84, never back to 1.
fn zoom_after_scroll(zoom: f32, delta: f32) -> f32 {
    (zoom * (delta * 0.01).exp()).clamp(0.1, 10.0)
}

/// Record `uri` as the newest displayed image and forget whatever drops out of
/// the window.
fn retain_displayed(app: &mut TwelfApp, uri: &str, ctx: &egui::Context) {
    for evicted in touch_displayed(&mut app.displayed_uris, uri) {
        for key in forgettable_keys(&evicted) {
            ctx.forget_image(&key);
        }
    }
}

/// Move `uri` to the newest end of `displayed`, returning the URIs that fall out
/// of the window. A revisited URI is moved rather than appended again, so
/// stepping back and forth through a folder cannot evict the whole window.
fn touch_displayed(displayed: &mut VecDeque<String>, uri: &str) -> Vec<String> {
    if let Some(pos) = displayed.iter().position(|u| u == uri) {
        displayed.remove(pos);
    }
    displayed.push_back(uri.to_string());
    let mut evicted = Vec::new();
    while displayed.len() > DISPLAYED_WINDOW {
        let Some(old) = displayed.pop_front() else {
            break;
        };
        evicted.push(old);
    }
    evicted
}

/// Every cache key one displayed URI can be held under. egui rewrites an
/// animated-format URI to `uri#<frame>` before handing it to the texture loader
/// (webp goes down this path even when single-frame), so forgetting the bare URI
/// alone would leave that texture behind. Frame 0 is the only one the still
/// path can produce — a real animation is played by `webp::Animation`, which
/// owns its textures and drops them with the selection.
fn forgettable_keys(uri: &str) -> [String; 2] {
    [uri.to_string(), format!("{uri}#0")]
}

/// Forget everything cached for the local file at `path` — bytes, decode and
/// texture — so the next time it is shown it is read from disk again.
pub fn forget_local_image(ctx: &egui::Context, path: &std::path::Path) {
    for key in forgettable_keys(&format!("file://{}", path.display())) {
        ctx.forget_image(&key);
    }
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
    // On the UI thread, so a decoder panic would end the app; caught, it just
    // falls back to the still path, which is guarded the same way.
    let frames = crate::decoded::catching_panics(|| {
        crate::webp::decode_frames(bytes).map_err(|e| e.to_string())
    })
    .ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps_are_symmetric_and_never_collapse() {
        // A notch up and a notch down land back where they started.
        let up = zoom_after_scroll(1.0, 40.0);
        assert!(up > 1.0);
        assert!((zoom_after_scroll(up, -40.0) - 1.0).abs() < 1e-6);
        // Several notches in one frame zoom out further, not to the floor.
        let out = zoom_after_scroll(1.0, -120.0);
        assert!(out > 0.1 && out < zoom_after_scroll(1.0, -40.0));
        // The limits still hold.
        assert_eq!(zoom_after_scroll(9.0, 400.0), 10.0);
        assert_eq!(zoom_after_scroll(0.2, -400.0), 0.1);
    }

    fn fill_window() -> VecDeque<String> {
        let mut displayed = VecDeque::new();
        for i in 0..DISPLAYED_WINDOW {
            assert!(touch_displayed(&mut displayed, &format!("file:///{i}.jpg")).is_empty());
        }
        displayed
    }

    #[test]
    fn window_holds_its_size_then_evicts_oldest_first() {
        let mut displayed = fill_window();
        assert_eq!(
            touch_displayed(&mut displayed, "file:///new.jpg"),
            vec!["file:///0.jpg".to_string()]
        );
        assert_eq!(displayed.len(), DISPLAYED_WINDOW);
    }

    #[test]
    fn revisiting_moves_instead_of_duplicating() {
        let mut displayed = fill_window();
        // Stepping back to the oldest evicts nothing and does not grow the window.
        assert!(touch_displayed(&mut displayed, "file:///0.jpg").is_empty());
        assert_eq!(displayed.len(), DISPLAYED_WINDOW);
        // It is now newest, so the next arrival drops what became oldest instead.
        assert_eq!(
            touch_displayed(&mut displayed, "file:///new.jpg"),
            vec!["file:///1.jpg".to_string()]
        );
        assert!(displayed.iter().any(|u| u == "file:///0.jpg"));
    }

    #[test]
    fn eviction_covers_the_animated_frame_key() {
        // egui stores a webp texture under "…#0", so the bare URI is not enough.
        assert_eq!(
            forgettable_keys("sftp://nas/a.webp"),
            [
                "sftp://nas/a.webp".to_string(),
                "sftp://nas/a.webp#0".to_string()
            ]
        );
    }
}
