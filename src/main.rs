mod home;
mod store;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::home::Home;

/// A local kanban board for you and your agents.
///
/// Run `kanzo` to open the board.
#[derive(Parser)]
#[command(name = "kanzo", version)]
struct Cli {
    /// Data folder [default: $KANZO_HOME, then ~/kanzo]
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    /// Port for the board [default: from config.toml]
    #[arg(long)]
    port: Option<u16>,

    /// Start the board without opening the browser
    #[arg(long)]
    no_open: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create the data folder; the first run does this too
    Init {
        /// Folder to create [default: the data folder]
        folder: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kanzo: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let dir = match &cli.command {
        Some(Command::Init { folder: Some(folder) }) => folder.clone(),
        _ => Home::resolve(cli.home)?,
    };
    let home = Home::open(dir)?;
    match cli.command {
        None => web::serve(home, cli.port, !cli.no_open).await,
        Some(Command::Init { .. }) => {
            println!("Kanzo data folder ready at {}", home.dir.display());
            Ok(())
        }
    }
}
