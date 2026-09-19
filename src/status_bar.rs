use crate::{TwelfApp, menu_bar, ssh};
use eframe::egui;
use std::path::Path;

/// A notice shown in the status bar until dismissed. It carries how it should
/// read, because painting every message in the error colour made "Saved
/// favorite" look exactly like "3 item(s) could not be removed".
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub text: String,
    pub is_error: bool,
}

impl Message {
    /// Something happened as asked.
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// Something failed, or a request was refused.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }

    /// The colour to override the label with: errors only. A plain notice keeps
    /// the theme's text colour.
    fn color(&self, visuals: &egui::Visuals) -> Option<egui::Color32> {
        self.is_error.then_some(visuals.error_fg_color)
    }
}

/// The bottom status bar: the selected node's full path on the left (truncated
/// when it doesn't fit), then any operation result awaiting acknowledgement,
/// then the in-flight download's progress on the right.
pub fn render(app: &mut TwelfApp, ctx: &egui::Context) {
    let host = match &app.ssh {
        ssh::SshState::Connected { info, .. } => info.host.as_str(),
        _ => "",
    };
    let path_text = selected_path_text(
        host,
        app.selected_remote.as_deref(),
        app.selected_image.as_deref(),
    );
    let mut cancel_download = false;
    let mut dismiss_message = false;
    egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            // Download progress hugs the right edge; the path truncates into
            // whatever width is left, so a long path can't push it off screen.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(dl) = app.remote_download.as_mut() {
                    dl.poll();
                    let finished = dl.is_finished();
                    if !finished && ui.button("Cancel").clicked() {
                        cancel_download = true;
                    }
                    ui.label(download_status_text(
                        finished,
                        dl.files(),
                        dl.bytes(),
                        dl.errors(),
                        dl.skipped(),
                        dl.target(),
                    ));
                    if !finished {
                        ctx.request_repaint();
                    }
                }
                // Dismissable so a failure notice can't be missed, and can't
                // linger past the point the user has taken it in.
                if let Some(msg) = app.status_message.as_ref() {
                    if ui.small_button("✕").clicked() {
                        dismiss_message = true;
                    }
                    let mut text = egui::RichText::new(&msg.text);
                    if let Some(color) = msg.color(ui.visuals()) {
                        text = text.color(color);
                    }
                    ui.add(egui::Label::new(text).truncate());
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(path_text.unwrap_or_default()).truncate());
                });
            });
        });
    });
    // Dropping the handle flips its cancel flag, stopping the walk.
    if cancel_download {
        app.remote_download = None;
    }
    if dismiss_message {
        app.status_message = None;
    }
}

/// The status-bar path for the current selection: the remote one (prefixed
/// `host:`, scp-style) when set, else the local one. Mirrors the image panel's
/// remote-over-local precedence.
fn selected_path_text(
    host: &str,
    selected_remote: Option<&Path>,
    selected_image: Option<&Path>,
) -> Option<String> {
    if let Some(path) = selected_remote {
        return Some(if host.is_empty() {
            path.display().to_string()
        } else {
            format!("{host}:{}", path.display())
        });
    }
    selected_image.map(|path| path.display().to_string())
}

/// The one-line progress text for a download: live counters while running, the
/// local target once finished, and a parenthetical for anything that did not
/// land — failures, and files left alone because a local copy already existed.
fn download_status_text(
    finished: bool,
    files: usize,
    bytes: u64,
    errors: usize,
    skipped: usize,
    target: &Path,
) -> String {
    let mut text = if finished {
        format!(
            "Downloaded {files} file(s), {} → {}",
            menu_bar::format_bytes(bytes),
            target.display()
        )
    } else {
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        format!(
            "Downloading {name}: {files} file(s), {}…",
            menu_bar::format_bytes(bytes)
        )
    };
    let mut notes = Vec::new();
    if errors > 0 {
        notes.push(format!("{errors} failed"));
    }
    if skipped > 0 {
        notes.push(format!("{skipped} already existed"));
    }
    if !notes.is_empty() {
        text.push_str(&format!(" ({})", notes.join(", ")));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_error_is_painted_in_the_error_colour() {
        let visuals = egui::Visuals::dark();
        assert_eq!(
            Message::info("Saved favorite nas:/photos").color(&visuals),
            None
        );
        assert_eq!(
            Message::error("Delete trip: 3 item(s) could not be removed").color(&visuals),
            Some(visuals.error_fg_color)
        );
    }
    use std::path::PathBuf;

    #[test]
    fn path_prefers_remote_with_host_prefix() {
        assert_eq!(
            selected_path_text(
                "nas",
                Some(Path::new("/photos/a.jpg")),
                Some(Path::new("/local/b.jpg")),
            ),
            Some("nas:/photos/a.jpg".to_string())
        );
    }

    #[test]
    fn path_omits_prefix_without_a_host() {
        // A remote selection lingering after a disconnect still shows its path.
        assert_eq!(
            selected_path_text("", Some(Path::new("/photos/a.jpg")), None),
            Some("/photos/a.jpg".to_string())
        );
    }

    #[test]
    fn path_falls_back_to_local_selection() {
        assert_eq!(
            selected_path_text("nas", None, Some(Path::new("/local/b.jpg"))),
            Some("/local/b.jpg".to_string())
        );
    }

    #[test]
    fn path_empty_when_nothing_selected() {
        assert_eq!(selected_path_text("nas", None, None), None);
    }

    #[test]
    fn download_text_in_progress_uses_folder_name() {
        assert_eq!(
            download_status_text(false, 3, 1536, 0, 0, &PathBuf::from("/dl/trip")),
            "Downloading trip: 3 file(s), 1.5 KB…"
        );
    }

    #[test]
    fn download_text_finished_shows_full_target() {
        assert_eq!(
            download_status_text(true, 3, 1536, 0, 0, &PathBuf::from("/dl/trip")),
            "Downloaded 3 file(s), 1.5 KB → /dl/trip"
        );
    }

    #[test]
    fn download_text_appends_failure_count() {
        assert_eq!(
            download_status_text(true, 2, 1024, 1, 0, &PathBuf::from("/dl/trip")),
            "Downloaded 2 file(s), 1.0 KB → /dl/trip (1 failed)"
        );
    }

    #[test]
    fn download_text_reports_skipped_and_combines_notes() {
        assert_eq!(
            download_status_text(true, 2, 1024, 0, 3, &PathBuf::from("/dl/trip")),
            "Downloaded 2 file(s), 1.0 KB → /dl/trip (3 already existed)"
        );
        assert_eq!(
            download_status_text(true, 2, 1024, 1, 3, &PathBuf::from("/dl/trip")),
            "Downloaded 2 file(s), 1.0 KB → /dl/trip (1 failed, 3 already existed)"
        );
    }
}
