mod connect;
mod debug_pane;
mod model;
mod model_panel;
mod run;
mod screen;
mod setup;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::Credentials;

#[derive(Parser)]
#[command(
    version,
    about = "Keeps a research folder in line with its DAMAP data management plan",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Set up this folder, or change its settings: DAMAP login, DMP and AI model")]
    Setup {
        #[arg(
            long,
            help = "DAMAP's URL, as opened in the browser; asked for if not given"
        )]
        url: Option<String>,
        #[command(flatten)]
        model: model::ModelArgs,
    },
    #[command(about = "Watch this folder for changes")]
    Run {
        #[arg(
            long,
            help = "Check changes against the DMP with this id instead of the one chosen in `setup`"
        )]
        dmp: Option<i64>,
    },
    #[command(about = "Forget the saved login for this folder")]
    Logout,
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Setup { url, model } => setup::run(url, model),
        Command::Run { dmp } => run::run(dmp),
        Command::Logout => {
            let mut credentials = Credentials::load()?;
            if credentials.refresh_token.take().is_some() {
                credentials.save()?;
                println!("Logged out.");
            } else {
                println!("Not logged in.");
            }
            Ok(())
        }
    }
}
