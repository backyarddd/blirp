#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    match blirp_desktop::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("blirp: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
