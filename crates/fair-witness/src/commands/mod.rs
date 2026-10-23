mod logout;
mod run;
mod setup;

use std::env;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fair_witness_core::project::{NOT_SET_UP, Project};

use crate::setup::model;

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

pub async fn run() -> Result<()> {
    let command = Cli::parse().command;
    let here = env::current_dir().context("could not determine the current directory")?;
    let project = Project::find(&here);
    match command {
        Command::Setup { url, model } => {
            setup::run(project.as_ref(), Project::new(here), url, model).await
        }
        Command::Run { dmp } => run::run(&project.context(NOT_SET_UP)?, dmp).await,
        Command::Logout => logout::run(project.as_ref()),
    }
}
