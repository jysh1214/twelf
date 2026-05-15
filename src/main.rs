use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Twelf",
        options,
        Box::new(|_cc| Ok(Box::<TwelfApp>::default())),
    )
}

#[derive(Default)]
struct TwelfApp;

impl eframe::App for TwelfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |_ui| {});
    }
}
