mod agent;
mod auth;
mod cli;
mod config;
mod damap;
mod watcher;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
