mod export;
mod home;
mod mcp;
mod reload;
mod service;
mod share;
mod store;
mod sync;
mod update;
mod web;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::LazyLock;

use clap::{Parser, Subcommand, ValueEnum};

use crate::home::Home;

/// Self-hosted kanban for humans and agents.
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
        /// Sync with this git repo from now on, like https://github.com/you/kanzo-data or you/kanzo-data
        #[arg(long, value_name = "URL")]
        repo: Option<String>,
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
    /// Share the board with other computers on your network, or stop; without on or off, show whether it is
    Share {
        #[arg(value_enum)]
        state: Option<OnOff>,
    },
    /// Use the board another computer shares, from this computer's board and agents
    Join {
        /// The address `kanzo share on` printed there, like 192.168.0.12
        host: String,
    },
    /// Stop using the shared board, and go back to this computer's own
    Leave,
    /// Install the latest release over this Kanzo, and restart the board's service
    Update,
    /// Start the board automatically at login
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Make, list or remove the accounts people and agents pass as `token` to say who they are
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// Add, list or remove a project's own columns
    Status {
        #[command(subcommand)]
        action: StatusAction,
    },
}

#[derive(Subcommand)]
enum StatusAction {
    /// Add a column to a project
    Add {
        /// The project's slug
        project: String,
        name: String,
        /// What it means, for the agents that move tasks there
        #[arg(long, short)]
        description: String,
        /// The column it comes right after [default: review]
        #[arg(long, short)]
        after: Option<String>,
        /// Its icon on the board: dot, eye, flag, star, check, bug, rocket, clock, person, chat, shield or beaker
        #[arg(long, short)]
        icon: Option<String>,
    },
    /// List a project's columns, or every project's
    List { project: Option<String> },
    /// Remove an empty column from a project
    Remove { project: String, name: String },
}

#[derive(Subcommand)]
enum AccountAction {
    /// Make an account and print its token
    Add {
        /// Its name, which nobody else may have, whatever the case
        name: String,
        #[arg(long, short, value_enum, default_value = "human")]
        kind: AccountKind,
        /// The project an orchestrator or agent works for [default: all]
        #[arg(long, short)]
        project: Option<String>,
    },
    /// List the accounts with their tokens
    List,
    /// Remove an account; its tasks become unassigned
    Remove { name: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum AccountKind {
    /// Can do anything, like the board
    Human,
    /// Makes and reviews tasks and task agents, in its project
    Orchestrator,
    /// Works on the tasks assigned to it
    Agent,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start a service that runs the board at login (LaunchAgent on macOS, systemd on Linux, Task Scheduler on Windows)
    Install,
    /// Stop and remove that service
    Uninstall,
}

fn main() -> ExitCode {
    // Before anything can replace the program file.
    LazyLock::force(&reload::BUILD);
    let cli = Cli::parse();
    if cfg!(windows)
        && let Ok(exe) = std::env::current_exe()
    {
        update::remove_old_exes(&exe);
    }
    // The board serves one person and each agent starts its own `kanzo mcp`, so one async
    // thread is enough, and blocking threads exit when idle: this keeps kanzo light on small boards.
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(8)
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(run(cli)));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kanzo: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    // Before opening the data folder: it may be the reason to update.
    if let Some(Command::Update) = cli.command {
        return update_kanzo();
    }
    let dir = match &cli.command {
        Some(Command::Init { folder: Some(folder) }) => folder.clone(),
        _ => Home::resolve(cli.home)?,
    };
    let mut home = Home::open(dir)?;
    if let Some(port) = cli.port {
        home.config.port = port;
    }
    match cli.command {
        None => web::serve(home, !cli.no_open).await,
        Some(Command::Mcp) => mcp::serve(home).await,
        Some(Command::Init { .. }) => {
            println!("Kanzo data folder ready at {}", home.dir.display());
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
        Some(Command::Share { state }) => {
            let change = state.map(|s| match s {
                OnOff::On => share::Change::On,
                OnOff::Off => share::Change::Off,
            });
            network(home, change, !cli.no_open).await
        }
        Some(Command::Join { host }) => network(home, Some(share::Change::Join(host)), !cli.no_open).await,
        Some(Command::Leave) => network(home, Some(share::Change::Leave), !cli.no_open).await,
        Some(Command::Service { action: ServiceAction::Install }) => service::install(&home),
        Some(Command::Service { action: ServiceAction::Uninstall }) => service::uninstall(),
        Some(Command::Account { action }) => account(&home, action),
        Some(Command::Status { action }) => status(&home, action),
        Some(Command::Update) => unreachable!("handled above"),
    }
}

fn account(home: &Home, action: AccountAction) -> anyhow::Result<()> {
    let store = home.store()?;
    match action {
        AccountAction::Add { name, kind, project } => {
            let kind = match kind {
                AccountKind::Human => store::Kind::Human,
                AccountKind::Orchestrator => store::Kind::Orchestrator,
                AccountKind::Agent => store::Kind::Agent,
            };
            let project = project.map(|p| store.find_project(&p)).transpose()?;
            let a = store.create_account(&name, kind, project.map(|p| p.id))?;
            println!("Added {}, {}. Its token: {}", a.name, a.kind.label(), a.token);
        }
        AccountAction::List => {
            let accounts = store.list_accounts()?;
            if accounts.is_empty() {
                println!("No accounts yet. Add one with `kanzo account add <name>`.");
            }
            let width = accounts.iter().map(|a| a.name.chars().count()).max().unwrap_or(0);
            for a in accounts {
                let project = a.project.as_deref().unwrap_or("all");
                println!("{:width$}  {:12}  {:16}  {}", a.name, a.kind.label(), project, a.token);
            }
        }
        AccountAction::Remove { name } => {
            let a = store.delete_account(&name)?;
            println!("Removed {}, {}.", a.name, a.kind.label());
        }
    }
    Ok(())
}

fn status(home: &Home, action: StatusAction) -> anyhow::Result<()> {
    let store = home.store()?;
    match action {
        StatusAction::Add { project, name, description, after, icon } => {
            let p = store.find_project(&project)?;
            let s = store.add_status(p.id, &name, &description, after.as_deref(), icon.as_deref())?;
            println!("Added {} to {}, after {}.", s.name, p.slug, s.after);
        }
        StatusAction::List { project } => {
            let project = project.map(|p| store.find_project(&p)).transpose()?;
            let id = project.as_ref().map(|p| p.id);
            println!("{}", store.statuses_for(id)?.join(", "));
            for s in store.custom_statuses(id)? {
                println!("{}: {} (in {}, after {})", s.name, s.description, s.project, s.after);
            }
        }
        StatusAction::Remove { project, name } => {
            let p = store.find_project(&project)?;
            let s = store.remove_status(p.id, &name)?;
            println!("Removed {} from {}.", s.name, p.slug);
        }
    }
    Ok(())
}

fn update_kanzo() -> anyhow::Result<()> {
    let Some(latest) = update::latest(true) else {
        anyhow::bail!("can't reach GitHub to check for updates");
    };
    if !update::newer(&latest) {
        println!("Kanzo is up to date ({}).", update::CURRENT);
        return Ok(());
    }
    println!("Installing Kanzo {latest} (you have {})…", update::CURRENT);
    update::install().map_err(anyhow::Error::msg)?;
    if service::restart()? {
        println!("Updated to {latest}, and restarted the board.");
    } else {
        println!("Updated to {latest}.");
    }
    Ok(())
}

async fn network(home: Home, change: Option<share::Change>, open: bool) -> anyhow::Result<()> {
    let sharing = matches!(change, Some(share::Change::On));
    let (status, running) = share::request(&home, change).await?;
    if sharing && !running {
        return web::serve(home, open).await;
    }
    println!("{}", status.message());
    Ok(())
}
