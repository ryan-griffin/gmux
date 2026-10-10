#![deny(deprecated)]

mod accessible_terminal;
mod app;
mod branch;
mod commands;
mod find_field;
mod font;
mod menu;
mod render;
mod renderer;
mod terminal;
mod terminal_canvas;

use clap::{Parser, Subcommand};
use gmux::{ipc, model, storage, theme};
use serde_json::json;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "A GNOME terminal with tabs and split panes")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Open the first tab in this directory.
    #[arg(long)]
    directory: Option<PathBuf>,
    /// Open another window connected to the same daemon.
    #[arg(long)]
    new_window: bool,
}
#[derive(Subcommand)]
enum Command {
    /// List tabs, panes, working directories, and unread indicators.
    List,
    /// Create and focus a tab.
    New {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        directory: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<u64>,
    },
    /// Add a persistent workspace, or remove one while keeping its tabs open.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Move a tab to a workspace, or remove its assignment when no workspace is supplied.
    Move {
        #[arg(long, env = "GMUX_TAB")]
        tab: Option<u64>,
        #[arg(long)]
        workspace: Option<u64>,
    },
    /// Split a terminal pane. Defaults to the pane in this shell.
    Split {
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
        #[arg(long)]
        down: bool,
    },
    /// Send literal text to a pane. Add --enter to run a command.
    Send {
        text: String,
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
        #[arg(long)]
        enter: bool,
    },
    /// Read the visible contents of a terminal pane.
    Read {
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Notify the user that a task needs attention.
    Notify {
        message: String,
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Focus a tab or pane.
    Focus {
        #[arg(long)]
        tab: Option<u64>,
        #[arg(long)]
        workspace: Option<u64>,
        #[arg(long)]
        pane: Option<u64>,
    },
    /// Close a pane and terminate its process.
    Close {
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Save a PNG of the application window.
    Screenshot { path: PathBuf },
    /// Stop the daemon and terminate all terminal processes.
    Quit,
    /// Send a raw JSON request to the local socket API.
    Rpc { request: String },
}
#[derive(Subcommand)]
enum WorkspaceCommand {
    New {
        #[arg(long)]
        name: String,
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    Delete {
        id: u64,
    },
}
fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        let request = match command {
            Command::Quit => json!({"method":"quit"}),
            Command::List => json!({"method":"list"}),
            Command::New {
                name,
                directory,
                workspace,
            } => serde_json::to_value(gmux::control::Command::New {
                name,
                directory: directory.map(std::path::absolute).transpose()?,
                workspace: workspace.map(Some),
            })?,
            Command::Workspace { command } => match command {
                WorkspaceCommand::New { name, directory } => {
                    serde_json::to_value(gmux::control::Command::WorkspaceNew {
                        name,
                        directory: directory.map(std::path::absolute).transpose()?,
                        tab: None,
                    })?
                }
                WorkspaceCommand::Delete { id } => {
                    serde_json::to_value(gmux::control::Command::WorkspaceDelete { workspace: id })?
                }
            },
            Command::Move { tab, workspace } => {
                serde_json::to_value(gmux::control::Command::Move { tab, workspace })?
            }
            Command::Split { pane, down } => {
                serde_json::to_value(gmux::control::Command::Split { pane, down })?
            }
            Command::Send { text, pane, enter } => {
                json!({"method":"send", "pane":pane, "text":text, "paste":true, "enter":enter})
            }
            Command::Read { pane } => json!({"method":"read", "pane":pane}),
            Command::Notify { message, pane } => {
                json!({"method":"notify", "pane":pane, "message":message})
            }
            Command::Focus {
                tab,
                workspace,
                pane,
            } => {
                json!({"method":"focus", "tab":tab, "workspace":workspace, "pane":pane})
            }
            Command::Close { pane } => {
                serde_json::to_value(gmux::control::Command::Close { pane })?
            }
            Command::Screenshot { path } => {
                json!({"method":"screenshot", "path":std::path::absolute(path)?})
            }
            Command::Rpc { request } => serde_json::from_str(&request)?,
        };
        let response = ipc::call(request)?;
        if let Some(text) = response.get("text").and_then(|v| v.as_str()) {
            println!("{text}");
        } else {
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        return Ok(());
    }
    app::run(cli.directory, cli.new_window)
}
