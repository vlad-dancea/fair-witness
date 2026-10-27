mod commands;
mod diagnostics;
mod notifications;
mod settings;
mod setup;
mod terminal;
mod tui;

fn main() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: could not start: {e}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(commands::run());
    // A notification the desktop never answers, or a review stuck reading a file, must not keep
    // fair-witness from exiting.
    runtime.shutdown_background();
    if let Err(failure) = result {
        tracing::error!("{:#}", failure.error());
        eprintln!("error: {:#}", failure.error());
        std::process::exit(failure.exit_code());
    }
}
