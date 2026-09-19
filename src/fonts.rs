use eframe::egui;
use std::sync::Arc;

const FONT_ASSETS: &[(&str, &'static [u8])] = &[(
    "noto_mono_cjk_sc",
    include_bytes!("../assets/NotoSansMonoCJKsc-Regular.otf"),
)];

/// The label on the small dismiss and remove buttons. It used to be "✕"
/// (U+2715), which none of the loaded fonts has, so every one of those buttons
/// was drawn as an empty box.
pub const DISMISS: &str = "×";

pub fn apply_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in FONT_ASSETS {
        fonts.font_data.insert(
            (*name).to_owned(),
            Arc::new(egui::FontData::from_static(bytes)),
        );
        fonts
            .families
            .get_mut(&egui::FontFamily::Monospace)
            .unwrap()
            .push((*name).to_owned());
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .push((*name).to_owned());
    }
    ctx.set_fonts(fonts);

    ctx.style_mut(|s| {
        for (_, font_id) in s.text_styles.iter_mut() {
            font_id.family = egui::FontFamily::Monospace;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_symbols_the_ui_draws_are_in_the_loaded_fonts() {
        let ctx = egui::Context::default();
        apply_fonts(&ctx);
        let mut missing = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let font = egui::FontId::monospace(14.0);
            // The dismiss button, and the punctuation the status texts use.
            for symbol in [DISMISS, "…", "→", "—"] {
                if !ctx.fonts_mut(|fonts| fonts.has_glyphs(&font, symbol)) {
                    missing.push(symbol);
                }
            }
        });
        assert!(missing.is_empty(), "no glyph for {missing:?}");
    }
}
