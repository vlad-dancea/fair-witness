mod commands;
mod notifications;
mod setup;
mod tui;

#[tokio::main]
async fn main() {
    if let Err(e) = commands::run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
