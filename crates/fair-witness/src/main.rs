mod auth;
mod commands;
mod setup;
mod tui;

fn main() {
    if let Err(e) = commands::run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
