#![deny(deprecated)]
mod client;

mod accessible_terminal;
mod activity;
mod app;
mod clipboard;
mod commands;
mod find_field;
mod font;
mod menu;
mod progress;
mod render;
mod renderer;
mod settings;
mod terminal;
mod terminal_canvas;

use crate::client::control::{Command as LayoutCommand, PaneRequest, PublicRequest, TabCommand};
use crate::client::model;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use gmux_client::control::pane_method;
use gmux_client::ipc;
use gmux_client::theme;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "gmux-gtk",
    version,
    about = "GNOME client for persistent local and remote terminals"
)]
struct Cli {
    /// Select a connection by name, or local for this computer.
    #[arg(long, global = true)]
    connection: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
    /// Open a new tab in this directory.
    #[arg(long)]
    directory: Option<PathBuf>,
    /// Open another window, inheriting the active window's default connection.
    #[arg(long)]
    new_window: bool,
}
#[derive(Subcommand)]
enum Command {
    /// List tabs, panes, working directories, program status, progress, and unread indicators.
    ///
    /// With GTK open and no --connection, include all connections in the active window.
    List,
    /// Create a terminal tab.
    ///
    /// With GTK open, use the window's default connection and workspace unless overridden.
    New {
        #[arg(long)]
        name: Option<String>,
        /// Starting directory; remote paths must be absolute or start with ~/.
        #[arg(long)]
        directory: Option<PathBuf>,
        #[arg(long)]
        workspace: Option<u64>,
    },
    /// Manage the workspaces shown in GTK. Removing one keeps its tabs open.
    ///
    /// Requires a running GTK app. Omit --connection.
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
    /// Split a pane. Defaults to the calling shell's pane or the window's active pane.
    Split {
        /// Pane ID; defaults to GMUX_PANE or the active pane when GTK is open.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
        #[arg(long)]
        down: bool,
    },
    /// Send literal text to a pane. Add --enter to run a command.
    Send {
        text: String,
        /// Pane ID; defaults to GMUX_PANE or the active pane when GTK is open.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
        #[arg(long)]
        enter: bool,
    },
    /// Read a pane's visible terminal contents.
    Read {
        /// Pane ID; defaults to GMUX_PANE or the active pane when GTK is open.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Read OSC 8 targets and ranges from the displayed GTK viewport.
    Links {
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Deliver a notification to clients viewing a pane.
    Notify {
        message: String,
        /// Pane ID; defaults to GMUX_PANE or the active pane when GTK is open.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Focus a tab, pane, or workspace in the graphical app.
    ///
    /// Requires a running GTK app. For a workspace, omit --connection.
    #[command(group(clap::ArgGroup::new("target").required(true).args(["tab", "pane", "workspace"])))]
    Focus {
        /// Tab ID on the selected connection.
        #[arg(long)]
        tab: Option<u64>,
        /// Workspace ID in the GTK app.
        #[arg(long)]
        workspace: Option<u64>,
        /// Pane ID on the selected connection.
        #[arg(long)]
        pane: Option<u64>,
    },
    /// Close a pane and terminate its process.
    Close {
        /// Pane ID; defaults to GMUX_PANE or the active pane when GTK is open.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Close a tab and all of its terminal processes.
    CloseTab {
        /// Tab ID on the selected connection.
        #[arg(long)]
        tab: u64,
    },
    /// Set a tab's shared name.
    Rename {
        /// Tab ID on the selected connection.
        #[arg(long)]
        tab: u64,
        name: String,
    },
    /// Restore the tab's automatic name.
    ResetName {
        /// Tab ID on the selected connection.
        #[arg(long)]
        tab: u64,
    },
    /// Save a PNG of the application window.
    ///
    /// Requires a running GTK app. Omit --connection.
    Screenshot { path: PathBuf },
    /// Quit the graphical app and stop its connected local daemon.
    ///
    /// Remote terminals keep running. Requires a running GTK app. Omit --connection.
    Quit,
    /// Send a JSON request to the app, or directly to a daemon with --connection.
    Rpc { request: String },
}
#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Add a workspace.
    Add {
        #[arg(long)]
        name: String,
        /// Starting directory on the local connection.
        #[arg(long)]
        directory: Option<PathBuf>,
        /// Starting directory for a connection; repeat for multiple connections.
        #[arg(long, value_name = "CONNECTION=PATH")]
        connection_directory: Vec<String>,
    },
    /// Edit a workspace name and starting directories.
    Edit {
        id: u64,
        #[arg(long)]
        name: Option<String>,
        /// Starting directory on the local connection.
        #[arg(long)]
        directory: Option<PathBuf>,
        /// Starting directory for a connection; use CONNECTION=~ to reset to home.
        #[arg(long, value_name = "CONNECTION=PATH")]
        connection_directory: Vec<String>,
    },
    /// Remove a workspace and keep its tabs open.
    Remove { id: u64 },
}
fn workspace_directories(
    values: &[String],
) -> anyhow::Result<std::collections::BTreeMap<gmux_client::model::DaemonId, PathBuf>> {
    let mut directories = std::collections::BTreeMap::new();
    if values.is_empty() {
        return Ok(directories);
    }
    let connections = call(serde_json::json!({"method":"connection-list"}))?;
    let mut choices = connections["remotes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(mut local) = connections
        .get("local")
        .filter(|v| v["daemon"].is_string())
        .cloned()
    {
        local["local"] = serde_json::json!(true);
        choices.push(local);
    }
    for value in values {
        let (name, path) = value
            .split_once('=')
            .filter(|(_, path)| !path.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Expected CONNECTION=PATH"))?;
        let connection = choices
            .iter()
            .find(|connection| {
                connection["name"] == name || (name == "local" && connection["local"] == true)
            })
            .ok_or_else(|| anyhow::anyhow!("Connection {name} is not configured"))?;
        directories.insert(
            serde_json::from_value(connection["daemon"].clone())?,
            if connection["local"] == true {
                std::path::absolute(gmux_client::model::expand_home(std::path::Path::new(path)))?
            } else {
                PathBuf::from(path)
            },
        );
    }
    Ok(directories)
}
use crate::client::ipc::call;
fn print_response(response: serde_json::Value, plain_text: bool) -> anyhow::Result<()> {
    if plain_text && let Some(text) = response.get("text").and_then(|v| v.as_str()) {
        println!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(&response)?);
    }
    Ok(())
}

fn launch_in_running_app(cli: &Cli) -> anyhow::Result<bool> {
    if !cli.new_window && cli.connection.is_none() && cli.directory.is_none() {
        return Ok(false);
    }
    Ok(ipc::call_if_running(
        &ipc::companion_path(".ui"),
        PublicRequest::Launch {
            connection: cli.connection.clone(),
            directory: cli.directory.clone(),
            directory_base: cli
                .directory
                .as_ref()
                .map(|_| std::env::current_dir())
                .transpose()?,
            new_window: cli.new_window,
        },
        None,
    )?
    .is_some())
}

fn main() -> anyhow::Result<()> {
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches)?;
    if let Some(name) = &cli.connection
        && name != "local"
        && gmux_client::connections::Catalog::load()?.is_local(name)
    {
        cli.connection = Some("local".into());
    }
    // IDs inherited from a shell belong to this computer, independently of
    // the active window's default connection.
    if cli.connection.is_none()
        && let Some((name, args)) = matches.subcommand()
        && match name {
            name if pane_method(name) => {
                args.value_source("pane") == Some(clap::parser::ValueSource::EnvVariable)
            }
            "move" => args.value_source("tab") == Some(clap::parser::ValueSource::EnvVariable),
            _ => false,
        }
    {
        cli.connection = Some("local".into());
    }
    if let Some(command) = cli.command {
        let plain_text = matches!(&command, Command::Read { .. });
        let rpc = matches!(&command, Command::Rpc { .. });
        anyhow::ensure!(
            cli.connection.is_none()
                || !matches!(
                    command,
                    Command::Quit
                        | Command::Screenshot { .. }
                        | Command::Workspace { .. }
                        | Command::Focus {
                            workspace: Some(_),
                            ..
                        }
                ),
            "This command controls the GTK app; omit --connection"
        );
        let mut request = match command {
            Command::Quit => serde_json::to_value(PublicRequest::Quit)?,
            Command::List => serde_json::to_value(PublicRequest::List)?,
            Command::New {
                name,
                directory,
                workspace,
            } => serde_json::to_value(LayoutCommand::New {
                directory_base: directory
                    .as_ref()
                    .map(|_| std::env::current_dir())
                    .transpose()?,
                name,
                directory,
                workspace: workspace.map(Some),
            })?,
            Command::Workspace { command } => match command {
                WorkspaceCommand::Add {
                    name,
                    directory,
                    connection_directory,
                } => serde_json::to_value(LayoutCommand::WorkspaceAdd {
                    name,
                    directory: directory
                        .map(|path| std::path::absolute(gmux_client::model::expand_home(&path)))
                        .transpose()?,
                    directories: workspace_directories(&connection_directory)?,
                    tab: None,
                })?,
                WorkspaceCommand::Edit {
                    id,
                    name,
                    directory,
                    connection_directory,
                } => {
                    let list = call(PublicRequest::List)?;
                    let definition = list["workspaces"]
                        .as_array()
                        .and_then(|workspaces| {
                            workspaces.iter().find(|workspace| workspace["id"] == id)
                        })
                        .ok_or_else(|| anyhow::anyhow!("Workspace {id} does not exist"))?;
                    let mut directories: std::collections::BTreeMap<
                        gmux_client::model::DaemonId,
                        PathBuf,
                    > = serde_json::from_value(definition["directories"].clone())?;
                    directories.extend(workspace_directories(&connection_directory)?);
                    serde_json::to_value(LayoutCommand::WorkspaceEdit {
                        workspace: id,
                        name: name
                            .unwrap_or_else(|| definition["name"].as_str().unwrap().to_owned()),
                        directory: directory
                            .map(|path| std::path::absolute(gmux_client::model::expand_home(&path)))
                            .transpose()?,
                        directories: Some(directories),
                    })?
                }
                WorkspaceCommand::Remove { id } => {
                    serde_json::to_value(LayoutCommand::WorkspaceRemove { workspace: id })?
                }
            },
            Command::Move { tab, workspace } => {
                serde_json::to_value(LayoutCommand::Move { tab, workspace })?
            }
            Command::Split { pane, down } => {
                serde_json::to_value(TabCommand::Split { pane, down })?
            }
            Command::Send { text, pane, enter } => serde_json::to_value(PaneRequest::Send {
                text,
                pane,
                paste: true,
                enter,
            })?,
            Command::Links { pane } => serde_json::to_value(PaneRequest::Links { pane })?,
            Command::Read { pane } => serde_json::to_value(PaneRequest::Read { pane })?,
            Command::Notify { message, pane } => {
                serde_json::to_value(PaneRequest::Notify { message, pane })?
            }
            Command::Focus {
                tab,
                workspace,
                pane,
            } => serde_json::to_value(PublicRequest::Focus {
                connection: None,
                tab,
                workspace,
                pane,
            })?,
            Command::Close { pane } => serde_json::to_value(TabCommand::Close { pane })?,
            Command::CloseTab { tab } => serde_json::to_value(TabCommand::CloseTab { tab })?,
            Command::Rename { tab, name } => {
                serde_json::to_value(TabCommand::Rename { tab, name })?
            }
            Command::ResetName { tab } => serde_json::to_value(TabCommand::ResetName { tab })?,
            Command::Screenshot { path } => serde_json::to_value(PublicRequest::Screenshot {
                path: std::path::absolute(path)?,
            })?,
            Command::Rpc { request } => serde_json::from_str(&request)?,
        };
        let response = if let Some(connection) = cli.connection {
            if connection != "local" {
                // A local shell's numeric pane environment must not target a remote pane.
                if let Some((name, args)) = matches.subcommand()
                    && pane_method(name)
                    && args.value_source("pane") != Some(clap::parser::ValueSource::CommandLine)
                {
                    request["pane"] = serde_json::Value::Null;
                }
                anyhow::ensure!(
                    !request["method"].as_str().is_some_and(pane_method)
                        || request["pane"].as_u64().is_some(),
                    if rpc {
                        "Remote pane requests require an explicit pane field"
                    } else {
                        "Remote pane commands require an explicit --pane"
                    }
                );
                {
                    if let Some(("move", args)) = matches.subcommand()
                        && args.value_source("tab") != Some(clap::parser::ValueSource::CommandLine)
                    {
                        request["tab"] = serde_json::Value::Null;
                    }
                    anyhow::ensure!(
                        request["method"] != "move" || request["tab"].as_u64().is_some(),
                        if rpc {
                            "Moving a remote tab requires an explicit tab field"
                        } else {
                            "Moving a remote tab requires an explicit --tab"
                        }
                    );
                    anyhow::ensure!(
                        !request["method"]
                            .as_str()
                            .is_some_and(LayoutCommand::workspace_method),
                        "Workspace commands control the GTK app; omit --connection"
                    );
                }
            }
            if request["method"] == "focus"
                || LayoutCommand::supports(request["method"].as_str().unwrap_or(""))
            {
                request["connection"] = serde_json::json!(connection);
                let response = call(request)?;
                print_response(response, plain_text)?;
                return Ok(());
            }
            if connection == "local" {
                ipc::Endpoint::local().call(request)?
            } else {
                ipc::Endpoint::remote(connection, None).call(request)?
            }
        } else {
            call(request)?
        };
        print_response(response, plain_text)?;
        return Ok(());
    }
    if launch_in_running_app(&cli)? {
        return Ok(());
    }
    app::run(cli.directory, cli.new_window, cli.connection)
}
