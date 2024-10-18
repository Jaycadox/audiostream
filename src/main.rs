use ui::MyApp;

mod audio;
mod server;
mod skip_verification;
mod ui;

fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Basic eframe UI",
        options,
        Box::new(|_cc| Ok(Box::new(MyApp::new()))),
    )
    .unwrap();
}
