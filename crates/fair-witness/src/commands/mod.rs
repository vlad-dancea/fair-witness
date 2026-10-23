mod logout;
mod run;
mod setup;

use std::env;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fair_witness_core::project::Project;

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
    #[command(
        about = "Watch this folder for changes, on screen or, with --headless, as JSON lines",
        after_help = "\
With --headless, each event is printed as one line of JSON on stdout, and messages for people \
go to stderr. It never asks for a DAMAP login, and stops when stdin closes, on Ctrl+C or on \
SIGTERM.

Exit codes: 0 when stopped, 1 when it fails while watching, 2 when it cannot start.

Set FAIR_WITNESS_DEBOUNCE_MS to change how long the folder has to be quiet before its changes \
count as one set (default 2000)."
    )]
    Run(run::Args),
    #[command(about = "Forget the saved login for this folder")]
    Logout,
}

/// Why fair-witness stopped with an error, which decides its exit code.
pub enum Failure {
    /// `run` could not start watching, such as in a folder that is not set up.
    Setup(anyhow::Error),
    Runtime(anyhow::Error),
}

impl Failure {
    /// 2 for a run that never started, as clap exits on bad arguments, so scripts can tell it
    /// from one that failed while watching.
    pub fn exit_code(&self) -> i32 {
        match self {
            Failure::Setup(_) => 2,
            Failure::Runtime(_) => 1,
        }
    }

    pub fn error(&self) -> &anyhow::Error {
        match self {
            Failure::Setup(e) | Failure::Runtime(e) => e,
        }
    }
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Runtime(e)
    }
}

pub async fn run() -> Result<(), Failure> {
    let command = Cli::parse().command;
    let here = env::current_dir()
        .context("could not determine the current directory")
        .map_err(Failure::Setup)?;
    match command {
        Command::Setup { url, model } => {
            let project = Project::find(&here);
            Ok(setup::run(project.as_ref(), Project::new(here), url, model).await?)
        }
        Command::Run(args) => run::run(&here, args).await,
        Command::Logout => Ok(logout::run(Project::find(&here).as_ref())?),
    }
}
