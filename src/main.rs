mod access;
mod background;
mod bridge;
mod export;
mod git_overview;
mod home;
mod mcp;
mod mcp_metrics;
mod merge;
mod service;
mod store;
mod sync;
mod update;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::home::Home;

/// Self-hosted kanban for humans and agents.
///
/// Run `brokan` to start the board in the background. Agents connect with `brokan mcp`.
#[derive(Parser)]
#[command(name = "brokan", version)]
struct Cli {
    /// Data folder [default: $BROKAN_HOME, then ~/brokan]
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    /// Port for the board [default: from config.toml]
    #[arg(long)]
    port: Option<u16>,

    /// Start the board without opening the browser
    #[arg(long)]
    no_open: bool,

    /// Keep the board attached to this terminal (for service managers and debugging)
    #[arg(long)]
    foreground: bool,

    /// Address to listen on, like 0.0.0.0 on a server [default: $BROKAN_LISTEN, then config.toml]
    #[arg(long, value_name = "IP")]
    listen: Option<std::net::IpAddr>,

    /// The URL the board is reached at on a server, like https://brokan.example.com; needs
    /// $BROKAN_TOKEN or $BROKAN_TOKEN_FILE [default: $BROKAN_PUBLIC_URL]
    #[arg(long, value_name = "URL")]
    public_url: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server on stdio (agents launch this, not you). After `brokan connect`,
    /// it relays to that board instead of using this machine's
    Mcp {
        /// Use this machine's board even when connected to another one
        #[arg(long)]
        local: bool,
    },
    /// Make `brokan mcp` on this machine use the board at URL, like https://brokan.example.com.
    /// The token is read from $BROKAN_TOKEN, or asked for
    Connect { url: String },
    /// Make `brokan mcp` use this machine's board again
    Disconnect,
    /// Create the data folder; the first run does this too
    Init {
        /// Folder to create [default: the data folder]
        folder: Option<PathBuf>,
    },
    /// Commit the data folder and pull or push it with its git remote
    Sync {
        /// Sync with this git repo from now on, like https://github.com/you/brokan-data or you/brokan-data
        #[arg(long, value_name = "URL")]
        repo: Option<String>,
        /// Instead of merging when both sides changed, overwrite the remote with this machine's board
        #[arg(long, conflicts_with = "keep_remote")]
        keep_local: bool,
        /// Instead of merging when both sides changed, take the remote board and drop this machine's changes
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
    /// Install the latest release over this Brokan, and restart the board's service
    Update,
    /// Start the board automatically at login (macOS and Linux)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start a service that runs the board at login (LaunchAgent on macOS, systemd on Linux)
    Install,
    /// Stop and remove that service
    Uninstall,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // One thread runs all the async work: the board serves one person, and each
    // agent starts its own `brokan mcp`. Database calls run on a few blocking
    // threads that exit when idle. This keeps brokan light on small boards.
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(8)
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(run(cli)));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("brokan: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    // Before opening the data folder: it may be the reason to update.
    if let Some(Command::Update) = cli.command {
        return update_brokan();
    }
    // A machine whose agents use a board elsewhere needs no data folder of its own.
    match &cli.command {
        Some(Command::Mcp { local: false }) => {
            if let Some(remote) = bridge::Remote::configured()? {
                return tokio::task::spawn_blocking(move || bridge::run(remote)).await?;
            }
        }
        Some(Command::Connect { url }) => return bridge::connect(url),
        Some(Command::Disconnect) => return bridge::disconnect(),
        _ => {}
    }
    let dir = match &cli.command {
        Some(Command::Init { folder: Some(folder) }) => folder.clone(),
        _ => Home::resolve(cli.home)?,
    };
    let home = Home::open(dir)?;
    match cli.command {
        None => {
            let options = web::Options::from_env(cli.port, cli.listen, cli.public_url)?;
            if cli.foreground {
                web::serve(home, options, !cli.no_open).await
            } else {
                background::start(home, options, !cli.no_open).await
            }
        }
        Some(Command::Mcp { .. }) => mcp::serve(home).await,
        Some(Command::Init { .. }) => {
            println!("Brokan data folder ready at {}", home.dir.display());
            Ok(())
        }
        Some(Command::Sync { repo, keep_local, keep_remote }) => {
            if let Some(url) = repo {
                println!("The board syncs with {}.", sync::set_remote(&home, &url, true)?);
            }
            let keep = match (keep_local, keep_remote) {
                (true, _) => Some(sync::Keep::Local),
                (_, true) => Some(sync::Keep::Remote),
                _ => None,
            };
            println!("{}", sync::sync(&home, keep, true)?);
            Ok(())
        }
        Some(Command::Export { format, project, output }) => {
            export::export(&home, format, project.as_deref(), output.as_deref())
        }
        Some(Command::Service { action: ServiceAction::Install }) => service::install(&home),
        Some(Command::Service { action: ServiceAction::Uninstall }) => service::uninstall(),
        Some(Command::Update | Command::Connect { .. } | Command::Disconnect) => unreachable!("handled above"),
    }
}

/// `brokan update`: installs the latest release over this program, then restarts the
/// board's service, if there is one, so it runs the new version too.
fn update_brokan() -> anyhow::Result<()> {
    let Some(latest) = update::latest(true) else {
        anyhow::bail!("can't reach GitHub to check for updates");
    };
    if !update::newer(&latest) {
        println!("Brokan is up to date ({}).", update::CURRENT);
        return Ok(());
    }
    println!("Installing Brokan {latest} (you have {})…", update::CURRENT);
    update::install().map_err(anyhow::Error::msg)?;
    if service::restart()? {
        println!("Updated to {latest}, and restarted the board.");
    } else {
        println!("Updated to {latest}.");
    }
    Ok(())
}
