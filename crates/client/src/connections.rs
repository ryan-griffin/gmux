//! Saved destinations and direct execution calls. Opening this catalog starts no daemon.
use crate::{ipc, network};
use gmux_platform::unix::storage;
use gmux_protocol::model::{DaemonId, Machine, MachineKind};
use gmux_protocol::network::Authentication;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_REMOTES: usize = 32;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub name: String,
    #[serde(deserialize_with = "deserialize_address")]
    pub address: String,
    pub authentication: Authentication,
    pub daemon: DaemonId,
    pub machine_kind: MachineKind,
}

fn deserialize_address<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let address = String::deserialize(deserializer)?;
    Ok(gmux_protocol::network::with_default_port(&address).into_owned())
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_name: Option<String>,
    pub remotes: Vec<Remote>,
}
impl Catalog {
    pub fn load() -> anyhow::Result<Self> {
        storage::load("connections.json")
    }
    pub fn local_name(&self) -> &str {
        self.local_name
            .as_deref()
            .unwrap_or(gmux_platform::unix::machine::hostname())
    }
    pub fn is_local(&self, name: &str) -> bool {
        name == "local" || name == self.local_name()
    }
    pub fn connections(&self) -> Vec<Machine> {
        self.remotes
            .iter()
            .map(|r| Machine {
                name: r.name.clone(),
                daemon: r.daemon,
                machine_kind: r.machine_kind,
            })
            .collect()
    }
    pub fn remote(&self, name: &str, expected: Option<DaemonId>) -> anyhow::Result<&Remote> {
        let remote = self
            .remotes
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| anyhow::anyhow!("Connection {name} does not exist"))?;
        anyhow::ensure!(
            expected.is_none_or(|id| id == remote.daemon),
            "Remote connection identity changed"
        );
        Ok(remote)
    }
    fn name_taken(&self, name: &str) -> bool {
        name == "local" || name == self.local_name() || self.remotes.iter().any(|r| r.name == name)
    }
    fn edit<T>(operation: impl FnOnce(&mut Self) -> anyhow::Result<T>) -> anyhow::Result<T> {
        Self::edit_prepared(operation, || Ok(()))
    }
    fn edit_prepared<T>(
        operation: impl FnOnce(&mut Self) -> anyhow::Result<T>,
        before_publish: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<T> {
        storage::with_state_lock("connections.json", |path| {
            let mut catalog: Self = storage::load_at(path)?;
            let output = operation(&mut catalog)?;
            storage::save_prepared_locked_at(path, &catalog, before_publish)?;
            Ok(output)
        })
    }
}
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
enum Request {
    ConnectionList,
    RemoteAccessGet,
    RemoteAccessSet {
        remote_access: gmux_protocol::network::RemoteAccess,
    },
    ConnectionAdd {
        name: Option<String>,
        address: String,
        authentication: Authentication,
    },
    ConnectionEdit {
        connection: String,
        daemon: Option<DaemonId>,
        #[serde(default, deserialize_with = "optional_name")]
        name: Option<Option<String>>,
        address: Option<String>,
        authentication: Option<Authentication>,
        remote_access: Option<gmux_protocol::network::RemoteAccess>,
    },
    ConnectionRemove {
        connection: String,
        daemon: Option<DaemonId>,
    },
    LocalDaemonStart,
    LocalDaemonStop,
    #[serde(rename = "_connection-call")]
    ConnectionCall {
        connection: String,
        daemon: Option<DaemonId>,
        request: Value,
    },
}

fn optional_name<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(de).map(Some)
}
pub fn handles(method: &str) -> bool {
    matches!(
        method,
        "connection-list"
            | "connection-add"
            | "connection-edit"
            | "connection-remove"
            | "remote-access-get"
            | "remote-access-set"
            | "local-daemon-start"
            | "local-daemon-stop"
            | "_connection-call"
    )
}
fn validate_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "Connection name must contain 1 to 64 letters, digits, dots, hyphens, or underscores"
    );
    Ok(())
}
fn validate_address(address: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !address.is_empty() && address.len() <= 255,
        "Invalid connection address"
    );
    Ok(())
}
/// Resolve client configuration locally and send execution requests directly to their destination.
pub fn call(body: impl Serialize) -> anyhow::Result<Value> {
    call_at(&ipc::Endpoint::local(), serde_json::to_value(body)?, None)
}
/// Async routing keeps execution I/O off the bounded filesystem workers.
pub async fn call_at_async(
    mut endpoint: ipc::Endpoint,
    mut body: Value,
    cancellation: Option<gmux_platform::unix::ipc::Cancellation>,
) -> anyhow::Result<Value> {
    loop {
        let method = body["method"].as_str().unwrap_or("");
        if method == "_connection-call" {
            let Request::ConnectionCall {
                connection,
                daemon,
                request,
            } = serde_json::from_value(body)?
            else {
                unreachable!()
            };
            endpoint = ipc::Endpoint::remote(connection, daemon);
            body = request;
        } else if handles(method) {
            return crate::runtime::blocking(move || {
                call_at(&endpoint, body, cancellation.as_ref())
            })
            .await;
        } else {
            return endpoint.call_async(body, cancellation.as_ref()).await;
        }
    }
}
pub fn call_at(
    endpoint: &ipc::Endpoint,
    body: Value,
    cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
) -> anyhow::Result<Value> {
    let call = |endpoint: &ipc::Endpoint, request: Value| match cancellation {
        Some(c) => endpoint.call_cancellable(request, c),
        None => endpoint.call(request),
    };
    if !handles(body["method"].as_str().unwrap_or("")) {
        return call(endpoint, body);
    }
    if let Some(c) = cancellation {
        c.check()?;
    }
    let result = match serde_json::from_value::<Request>(body)? {
        Request::ConnectionList => {
            let catalog = Catalog::load()?;
            let mut result = remote_access_status(cancellation)?;
            result["local"]["name"] = json!(catalog.local_name());
            result["hostname"] = json!(gmux_platform::unix::machine::hostname());
            result["remotes"] = json!(catalog.remotes);
            result
        }
        Request::RemoteAccessGet => remote_access_status(cancellation)?,
        Request::RemoteAccessSet { remote_access } => {
            set_remote_access(&remote_access, cancellation)?;
            json!({"remote_access":remote_access})
        }
        Request::LocalDaemonStart => {
            gmux_platform::unix::local::ensure_daemon(cancellation)?;
            let mut state = call(
                &ipc::Endpoint::local(),
                json!({"method":"remote-access-get"}),
            )?;
            state["local"]["name"] = json!(Catalog::load()?.local_name());
            state
        }
        Request::LocalDaemonStop => {
            // Disabling local execution must also work during startup backoff,
            // when there is no daemon to stop. Do not start one to disable it.
            if gmux_platform::unix::ipc::call_if_running(
                &gmux_platform::unix::ipc::socket_path(),
                gmux_protocol::control::InternalRequest::Hello,
                cancellation,
            )?
            .is_some()
            {
                ipc::Endpoint::local().stop(cancellation)?
            } else {
                json!({"ok":true})
            }
        }
        Request::ConnectionAdd {
            name,
            address,
            authentication,
        } => {
            let address = gmux_protocol::network::with_default_port(&address).into_owned();
            validate_address(&address)?;
            if let Some(name) = &name {
                validate_name(name)?;
            }
            let client = network::Client::connect_cancellable(
                &address,
                &network::credentials(&authentication)?,
                None,
                cancellation,
            )?;
            // An optional existing identity detects self without consulting a running
            // local server. Remote-only clients never allocate a local identity.
            if let Ok(Some(local)) = storage::load::<Option<DaemonId>>("daemon-id.json") {
                anyhow::ensure!(
                    local != client.daemon,
                    "This address belongs to the local connection"
                );
            }
            if let Some(c) = cancellation {
                c.check()?;
            }
            Catalog::edit(|catalog| {
                anyhow::ensure!(
                    catalog.remotes.len() < MAX_REMOTES,
                    "Up to {MAX_REMOTES} remote connections can be configured"
                );
                anyhow::ensure!(
                    !catalog.remotes.iter().any(|r| r.daemon == client.daemon),
                    "Connection is already configured"
                );
                let name = if let Some(name) = name {
                    name
                } else {
                    let base = client
                        .hostname
                        .split('.')
                        .next()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("remote");
                    let base: String = base
                        .chars()
                        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                        .take(48)
                        .collect();
                    let base = if base.is_empty() {
                        "remote".into()
                    } else {
                        base
                    };
                    let mut candidate = base.clone();
                    let mut suffix = 2;
                    while catalog.name_taken(&candidate) {
                        candidate = format!("{base}-{suffix}");
                        suffix += 1;
                    }
                    candidate
                };
                validate_name(&name)?;
                anyhow::ensure!(
                    !catalog.name_taken(&name),
                    "Connection name is already in use"
                );
                catalog.remotes.push(Remote {
                    name: name.clone(),
                    address,
                    authentication,
                    daemon: client.daemon,
                    machine_kind: client.machine_kind,
                });
                Ok(json!({"name":name,"daemon":client.daemon}))
            })?
        }
        Request::ConnectionEdit {
            connection,
            daemon,
            name,
            address,
            authentication,
            remote_access,
        } => {
            if connection == "local" || Catalog::load()?.is_local(&connection) {
                anyhow::ensure!(daemon.is_none(), "Remote connection identity changed");
                anyhow::ensure!(
                    address.is_none() && authentication.is_none(),
                    "Use remote access settings to configure incoming connections to the local daemon"
                );
                let mut result = json!({"ok":true});
                if let Some(name) = name {
                    let name = Catalog::edit_prepared(
                        |catalog| {
                            if let Some(n) = &name {
                                validate_name(n)?;
                            }
                            let effective_name = name
                                .as_deref()
                                .unwrap_or(gmux_platform::unix::machine::hostname());
                            anyhow::ensure!(
                                name.as_deref() != Some("local")
                                    && !catalog.remotes.iter().any(|r| r.name == effective_name),
                                "Connection name is already in use"
                            );
                            catalog.local_name = name
                                .filter(|name| name != gmux_platform::unix::machine::hostname());
                            Ok(catalog.local_name().to_owned())
                        },
                        || {
                            // Both inputs and the catalog's file write are prepared
                            // before changing the daemon; publish the name last.
                            if let Some(remote_access) = &remote_access {
                                set_remote_access(remote_access, cancellation)?;
                            }
                            Ok(())
                        },
                    )?;
                    result["name"] = json!(name);
                } else if let Some(remote_access) = &remote_access {
                    set_remote_access(remote_access, cancellation)?;
                }
                if let Some(remote_access) = remote_access {
                    result["remote_access"] = serde_json::to_value(remote_access)?;
                }
                result
            } else {
                anyhow::ensure!(
                    remote_access.is_none(),
                    "Remote access settings belong to the local connection"
                );
                let original = Catalog::load()?.remote(&connection, daemon)?.clone();
                let mut remote = original.clone();
                if let Some(name) = name {
                    remote.name = name.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Only the local connection can use this computer's hostname"
                        )
                    })?;
                }
                if let Some(a) = address {
                    remote.address = gmux_protocol::network::with_default_port(&a).into_owned();
                }
                if let Some(a) = authentication {
                    remote.authentication = a;
                }
                validate_name(&remote.name)?;
                validate_address(&remote.address)?;
                if remote.address != original.address
                    || remote.authentication != original.authentication
                {
                    let client = network::Client::connect_cancellable(
                        &remote.address,
                        &crate::network::credentials(&remote.authentication)?,
                        Some(remote.daemon),
                        cancellation,
                    )?;
                    remote.machine_kind = client.machine_kind;
                }
                if let Some(c) = cancellation {
                    c.check()?;
                }
                Catalog::edit(|catalog| {
                    anyhow::ensure!(
                        remote.name != "local"
                            && remote.name != catalog.local_name()
                            && !catalog
                                .remotes
                                .iter()
                                .any(|r| r.daemon != remote.daemon && r.name == remote.name),
                        "Connection name is already in use"
                    );
                    let saved = catalog
                        .remotes
                        .iter_mut()
                        .find(|r| r.daemon == original.daemon)
                        .ok_or_else(|| anyhow::anyhow!("Connection was removed while editing"))?;
                    anyhow::ensure!(
                        *saved == original,
                        "Connection changed while editing; reopen its editor"
                    );
                    let result = json!({"name":remote.name,"daemon":remote.daemon});
                    *saved = remote;
                    Ok(result)
                })?
            }
        }
        Request::ConnectionRemove { connection, daemon } => Catalog::edit(|catalog| {
            anyhow::ensure!(
                !catalog.is_local(&connection),
                "The local connection cannot be removed"
            );
            let daemon = catalog.remote(&connection, daemon)?.daemon;
            catalog.remotes.retain(|r| r.daemon != daemon);
            Ok(json!({"ok":true,"daemon":daemon}))
        })?,
        Request::ConnectionCall {
            connection,
            daemon,
            request,
        } => call(&ipc::Endpoint::remote(connection, daemon), request)?,
    };
    if let Some(c) = cancellation {
        c.check()?;
    }
    Ok(result)
}

fn remote_access_status(
    cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
) -> anyhow::Result<Value> {
    let current = gmux_platform::unix::ipc::call_if_running(
        &gmux_platform::unix::ipc::socket_path(),
        json!({"method":"remote-access-get"}),
        cancellation,
    )?;
    let running = current.is_some();
    let mut result = match current {
        Some(value) => value,
        None => json!({
            "remote_access":storage::load::<gmux_protocol::network::RemoteAccess>("remote-access.json")?,
            "bound_address":null,"remote_access_error":null,
            "local":{"name":gmux_platform::unix::machine::hostname(),"machine_kind":gmux_platform::unix::machine::machine_kind(),"daemon":null}
        }),
    };
    result["running"] = json!(running);
    Ok(result)
}

fn set_remote_access(
    remote_access: &gmux_protocol::network::RemoteAccess,
    cancellation: Option<&gmux_platform::unix::ipc::Cancellation>,
) -> anyhow::Result<()> {
    if remote_access.enabled {
        gmux_platform::unix::network::token(&remote_access.authentication)?;
    }
    if gmux_platform::unix::ipc::call_if_running(
        &gmux_platform::unix::ipc::socket_path(),
        json!({"method":"remote-access-set","remote_access":remote_access}),
        cancellation,
    )?
    .is_none()
    {
        storage::save("remote-access.json", remote_access)?;
    }
    Ok(())
}

/// Watch atomic catalog changes across CLI and graphical clients without polling the disk.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_remote_addresses_include_ports_and_preserve_hostnames() {
        let daemon = gmux_platform::unix::identity::daemon_id().unwrap();
        for (input, expected) in [
            ("server.example", "server.example:7331"),
            ("127.0.0.1", "127.0.0.1:7331"),
            ("::1", "[::1]:7331"),
            ("[::1]", "[::1]:7331"),
            ("fe80::1%3", "[fe80::1%3]:7331"),
            ("::7331", "[::7331]:7331"),
            ("server.example:8400", "server.example:8400"),
            ("127.0.0.1:8400", "127.0.0.1:8400"),
            ("[::1]:8400", "[::1]:8400"),
        ] {
            let remote: Remote = serde_json::from_value(json!({
                "name":"server", "address":input, "authentication":{"mode":"none"},
                "daemon":daemon, "machine_kind":"unknown"
            }))
            .unwrap();
            assert_eq!(remote.address, expected);
            assert_eq!(serde_json::to_value(remote).unwrap()["address"], expected);
        }
    }
}
