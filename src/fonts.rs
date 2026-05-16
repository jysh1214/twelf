use eframe::egui;
use std::sync::Arc;

const FONT_ASSETS: &[(&str, &'static [u8])] = &[(
    "noto_mono_cjk_sc",
    include_bytes!("../assets/NotoSansMonoCJKsc-Regular.otf"),
)];

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
