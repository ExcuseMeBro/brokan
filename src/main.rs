mod export;
mod home;
mod mcp;
mod service;
mod store;
mod sync;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::home::Home;

/// A local kanban board for you and your agents.
///
/// Run `kanzo` to open the board. Agents connect with `kanzo mcp`.
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
    /// Run the MCP server on stdio (agents launch this, not you)
    Mcp,
    /// Create the data folder; the first run does this too
    Init {
        /// Folder to create [default: the data folder]
        folder: Option<PathBuf>,
    },
    /// Commit the data folder and pull or push it with its git remote
    Sync {
        /// When both sides changed, keep this machine's board and overwrite the remote
        #[arg(long, conflicts_with = "keep_remote")]
        keep_local: bool,
        /// When both sides changed, take the remote board and drop this machine's changes
        #[arg(long)]
        keep_remote: bool,
    },
    /// Write the whole board as Markdown or JSON
    Export {
        /// Output format
        #[arg(long, short, value_enum, default_value = "markdown")]
        format: export::Format,
        /// Only this project
        #[arg(long, short)]
        project: Option<String>,
        /// Write to this file instead of the terminal
        #[arg(long, short, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Start the board automatically at login (macOS)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start a LaunchAgent that runs the board at login
    Install,
    /// Stop and remove the LaunchAgent
    Uninstall,
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
        Some(Command::Mcp) => mcp::serve(home).await,
        Some(Command::Init { .. }) => {
            println!("Kanzo data folder ready at {}", home.dir.display());
            Ok(())
        }
        Some(Command::Sync { keep_local, keep_remote }) => {
            let keep = match (keep_local, keep_remote) {
                (true, _) => Some(sync::Keep::Local),
                (_, true) => Some(sync::Keep::Remote),
                _ => None,
            };
            sync::sync(&home, keep)
        }
        Some(Command::Export { format, project, output }) => {
            export::export(&home, format, project.as_deref(), output.as_deref())
        }
        Some(Command::Service { action: ServiceAction::Install }) => service::install(&home),
        Some(Command::Service { action: ServiceAction::Uninstall }) => service::uninstall(),
    }
}
