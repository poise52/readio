#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    if let Err(error) = readio_desktop::run() {
        eprintln!("Не удалось запустить Readio: {error}");
        std::process::exit(1);
    }
}
