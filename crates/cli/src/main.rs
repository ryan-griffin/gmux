use clap::{
    Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, parser::ValueSource,
};
use gmux_client::ipc;
use gmux_protocol::control::{
    Command as TerminalCommand, PaneRequest, PublicRequest, TabCommand, pane_method,
};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "gmux",
    version,
    about = "Persistent terminal daemon and management CLI"
)]
struct Cli {
    /// Select a connection by name, or local for this computer.
    #[arg(long, global = true)]
    connection: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the local daemon using saved settings. Fails if already running.
    Serve(ServeOptions),
    /// Inspect the daemon's identity, connections, and execution statistics.
    Status,
    /// Inspect or change this daemon's saved per-terminal history retention.
    Scrollback {
        /// Maximum retained allocation in bytes: integer, default, or unlimited. Zero disables history.
        #[arg(long)]
        max_bytes: Option<gmux_protocol::retention::Limit>,
        /// Maximum physical history lines: integer, default, or unlimited. Zero still permits a native page.
        #[arg(long)]
        max_lines: Option<gmux_protocol::retention::Limit>,
    },
    /// List tabs, panes, working directories, program status, progress, and unread indicators.
    List,
    /// Create a terminal tab.
    New {
        #[arg(long)]
        name: Option<String>,
        /// Starting directory; remote paths must be absolute or start with ~/.
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    /// Split a pane. Locally defaults to the pane in the calling shell.
    Split {
        /// Pane ID; defaults to GMUX_PANE or the only pane locally.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
        #[arg(long)]
        down: bool,
    },
    /// Send literal text to a pane. Add --enter to run a command.
    Send {
        text: String,
        /// Pane ID; defaults to GMUX_PANE or the only pane locally.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
        #[arg(long)]
        enter: bool,
    },
    /// Read a pane's visible terminal contents.
    Read {
        /// Pane ID; defaults to GMUX_PANE or the only pane locally.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Read OSC 8 targets and cell ranges in the default daemon viewport.
    Links {
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Inspect program status records, command lifecycle, and process metadata.
    Metadata {
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Print the latest command's output, or output containing a screen cell.
    #[command(name = "command-output")]
    Output {
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
        /// Zero-based column. Both coordinates are required together.
        #[arg(long, requires = "row")]
        column: Option<u16>,
        /// Zero-based row in retained history, including the active screen.
        #[arg(long, requires = "column")]
        row: Option<u32>,
        /// Return JSON with plain text and optional styled HTML.
        #[arg(long)]
        html: bool,
    },
    /// Deliver a notification to clients viewing a pane.
    Notify {
        message: String,
        /// Pane ID; defaults to GMUX_PANE or the only pane locally.
        #[arg(long, env = "GMUX_PANE")]
        pane: Option<u64>,
    },
    /// Close a pane and terminate its process.
    Close {
        /// Pane ID; defaults to GMUX_PANE or the only pane locally.
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
    /// Stop the daemon and its terminal processes, saving history.
    Stop,
    /// Manage local and saved remote connections.
    Connection {
        #[command(subcommand)]
        command: ConnectionCommand,
    },
    /// Send a JSON request through the selected daemon's public control API.
    Rpc { request: String },
}

#[derive(Subcommand)]
enum ConnectionCommand {
    /// List this computer, incoming connection settings, and saved remotes. No daemon is required.
    List,
    /// Add a remote connection, verifying its identity and authentication.
    Add {
        /// Daemon hostname or IP address. The port defaults to 7331.
        address: String,
        /// Connection name; defaults to the daemon's hostname.
        #[arg(long)]
        name: Option<String>,
        /// Authentication required by the remote daemon.
        #[arg(long, value_enum)]
        auth: Auth,
        /// File containing the shared authentication token.
        #[arg(long, requires = "auth", required_if_eq("auth", "token"))]
        token_file: Option<PathBuf>,
    },
    /// Edit a saved connection, or use local for this computer's name and incoming access.
    #[command(group(clap::ArgGroup::new("changes").required(true).multiple(true)
        .args(["new_name", "use_hostname", "address", "auth", "listen_address", "remote_access"])))]
    Edit {
        /// Current connection name, or local for this computer.
        #[arg(value_name = "CONNECTION")]
        target: String,
        /// Change the connection's display name.
        #[arg(long = "name", value_name = "NAME", conflicts_with = "use_hostname")]
        new_name: Option<String>,
        /// Use this computer's hostname for the local connection.
        #[arg(long, conflicts_with = "new_name")]
        use_hostname: bool,
        /// Change a remote address, preserving its daemon identity. The port defaults to 7331.
        #[arg(long)]
        address: Option<String>,
        /// Allow incoming TCP connections locally. Disabling leaves terminals running.
        #[arg(long, value_name = "BOOL")]
        remote_access: Option<bool>,
        /// Incoming listening IP address. The port defaults to 7331.
        #[arg(long, value_parser = gmux_protocol::network::parse_listen_address)]
        listen_address: Option<SocketAddr>,
        /// Authentication for the remote daemon, or incoming clients when editing local.
        #[arg(long, value_enum, required_if_eq("remote_access", "true"))]
        auth: Option<Auth>,
        /// File containing the shared authentication token.
        #[arg(long, requires = "auth", required_if_eq("auth", "token"))]
        token_file: Option<PathBuf>,
    },
    /// Remove a saved remote connection. Its terminals keep running.
    Remove {
        /// Name of the saved remote connection.
        #[arg(value_name = "CONNECTION")]
        target: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Auth {
    /// Allow anyone who can reach the address to connect.
    None,
    /// Require matching token contents on clients and the daemon.
    Token,
}

#[derive(Args, Default)]
struct ServeOptions {
    /// Start a detached daemon and return once it is ready. Log errors to daemon.log.
    #[arg(long)]
    background: bool,
}

impl ServeOptions {
    fn run(self) -> anyhow::Result<()> {
        if self.background {
            gmux_client::local::start_daemon()
        } else {
            gmux_daemon::daemon::run()
        }
    }
}

#[derive(Parser)]
#[command(
    name = "gmux serve",
    bin_name = "gmux serve",
    about = "Start the local daemon using saved connection settings. Fails if already running."
)]
struct ServeCli {
    #[command(flatten)]
    options: ServeOptions,
}
impl Auth {
    fn authentication(
        self,
        file: Option<PathBuf>,
    ) -> anyhow::Result<gmux_protocol::network::Authentication> {
        anyhow::ensure!(
            self == Self::Token || file.is_none(),
            "--token-file requires --auth token"
        );
        Ok(match self {
            Self::None => gmux_protocol::network::Authentication::None,
            Self::Token => gmux_protocol::network::Authentication::Token {
                file: std::path::absolute(
                    file.ok_or_else(|| anyhow::anyhow!("Token file is required"))?,
                )?,
            },
        })
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            if error.is::<gmux_protocol::control::DaemonAlreadyRunning>() {
                std::process::ExitCode::from(
                    gmux_protocol::control::DaemonAlreadyRunning::EXIT_CODE,
                )
            } else {
                std::process::ExitCode::FAILURE
            }
        }
    }
}

fn run() -> anyhow::Result<()> {
    if std::env::args_os().len() == 1 {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }
    // Serving only needs launch options. Avoid constructing the management
    // command parser and touching its code pages in the resident process.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("serve")) {
        let options = if std::env::args_os().len() == 2 {
            ServeOptions::default()
        } else {
            ServeCli::parse_from(std::env::args_os().skip(1)).options
        };
        return options.run();
    }
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    if let Command::Serve(options) = cli.command {
        anyhow::ensure!(
            cli.connection.is_none(),
            "serve runs locally; connection selectors require a management command"
        );
        drop(matches);
        return options.run();
    }
    let connection = match cli.connection {
        Some(name)
            if name == "local" || gmux_client::connections::Catalog::load()?.is_local(&name) =>
        {
            None
        }
        connection => connection,
    };
    let remote = connection.is_some();
    if remote && matches!(cli.command, Command::Connection { .. }) {
        anyhow::bail!("Connection configuration belongs to this client; omit --connection");
    }
    if remote
        && let Some((name, args)) = matches.subcommand()
        && pane_method(name)
    {
        anyhow::ensure!(
            args.value_source("pane") == Some(ValueSource::CommandLine),
            "Remote pane commands require an explicit --pane"
        );
    }
    let stop = matches!(&cli.command, Command::Stop);
    let plain_text = matches!(
        &cli.command,
        Command::Read { .. } | Command::Output { html: false, .. }
    );
    let request = match cli.command {
        Command::Serve(_) => unreachable!(),
        Command::Scrollback {
            max_bytes,
            max_lines,
        } => serde_json::to_value(gmux_protocol::control::ExecutionRequest::Scrollback {
            max_bytes,
            max_lines,
        })?,
        Command::Status => serde_json::to_value(PublicRequest::Status)?,
        Command::List => serde_json::to_value(PublicRequest::List)?,
        Command::New { name, directory } => serde_json::to_value(TerminalCommand::New {
            name,
            directory: if remote {
                directory
            } else {
                directory
                    .map(|path| std::path::absolute(gmux_platform::unix::paths::expand_home(&path)))
                    .transpose()?
            },
        })?,
        Command::Split { pane, down } => serde_json::to_value(TabCommand::Split { pane, down })?,
        Command::Close { pane } => serde_json::to_value(TabCommand::Close { pane })?,
        Command::CloseTab { tab } => serde_json::to_value(TabCommand::CloseTab { tab })?,
        Command::Rename { tab, name } => serde_json::to_value(TabCommand::Rename { tab, name })?,
        Command::ResetName { tab } => serde_json::to_value(TabCommand::ResetName { tab })?,
        Command::Send { text, pane, enter } => serde_json::to_value(PaneRequest::Send {
            pane,
            text,
            paste: true,
            enter,
        })?,
        Command::Links { pane } => serde_json::to_value(PaneRequest::Links { pane })?,
        Command::Metadata { pane } => serde_json::to_value(PaneRequest::Metadata { pane })?,
        Command::Output {
            pane,
            column,
            row,
            html,
        } => serde_json::to_value(PaneRequest::CommandOutput {
            pane,
            point: column.zip(row),
            formats: if html {
                gmux_protocol::clipboard::Formats::TextHtml
            } else {
                gmux_protocol::clipboard::Formats::Text
            },
        })?,
        Command::Read { pane } => serde_json::to_value(PaneRequest::Read { pane })?,
        Command::Notify { message, pane } => {
            serde_json::to_value(PaneRequest::Notify { pane, message })?
        }
        Command::Stop => serde_json::to_value(PublicRequest::Stop)?,
        Command::Rpc { request } => serde_json::from_str(&request)?,
        Command::Connection { command } => match command {
            ConnectionCommand::List => json!({"method":"connection-list"}),
            ConnectionCommand::Add {
                address,
                name,
                auth,
                token_file,
            } => json!({
                "method":"connection-add","address":address,"name":name,
                "authentication":auth.authentication(token_file)?
            }),
            ConnectionCommand::Remove { target } => {
                json!({"method":"connection-remove","connection":target})
            }
            ConnectionCommand::Edit {
                target,
                new_name,
                use_hostname,
                address,
                remote_access,
                listen_address,
                auth,
                token_file,
            } => {
                let local = target == "local"
                    || gmux_client::connections::Catalog::load()?.is_local(&target);
                let authentication = auth
                    .map(|auth| auth.authentication(token_file))
                    .transpose()?;
                let mut request = json!({"method":"connection-edit","connection":target});
                if local {
                    anyhow::ensure!(
                        address.is_none(),
                        "Use --listen-address for the local daemon's incoming address"
                    );
                    request["connection"] = json!("local");
                    if remote_access.is_some()
                        || listen_address.is_some()
                        || authentication.is_some()
                    {
                        let current =
                            gmux_client::connections::call(json!({"method":"remote-access-get"}))?;
                        let mut config: gmux_protocol::network::RemoteAccess =
                            serde_json::from_value(current["remote_access"].clone())?;
                        if let Some(enabled) = remote_access {
                            config.enabled = enabled;
                        }
                        if let Some(address) = listen_address {
                            config.listen_address = address;
                        }
                        if let Some(authentication) = authentication {
                            config.authentication = authentication;
                        }
                        request["remote_access"] = serde_json::to_value(config)?;
                    }
                } else {
                    anyhow::ensure!(
                        remote_access.is_none() && listen_address.is_none(),
                        "--remote-access and --listen-address apply only to the local connection"
                    );
                    request["address"] = json!(address);
                    request["authentication"] = serde_json::to_value(authentication)?;
                }
                if use_hostname {
                    request["name"] = Value::Null;
                } else if let Some(name) = new_name {
                    request["name"] = json!(name);
                }
                request
            }
        },
    };
    let response = if stop {
        let endpoint = connection.map_or_else(ipc::Endpoint::local, |name| {
            ipc::Endpoint::remote(name, None)
        });
        endpoint.stop(None)?
    } else if TerminalCommand::supports(request["method"].as_str().unwrap_or("")) {
        let endpoint = connection.map_or_else(ipc::Endpoint::local, |name| {
            ipc::Endpoint::remote(name, None)
        });
        serde_json::to_value(endpoint.command_result(serde_json::from_value(request)?, None)?)?
    } else if let Some(connection) = connection {
        ipc::Endpoint::remote(connection, None).call(request)?
    } else {
        gmux_client::connections::call(request)?
    };
    if plain_text && let Some(text) = response.get("text").and_then(Value::as_str) {
        println!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(&response)?);
    }
    Ok(())
}
