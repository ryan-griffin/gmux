//! This daemon's TCP listener configuration. Outgoing destinations belong to clients.
use super::network as server;
use gmux_client::{
    ipc,
    model::{self, DaemonId},
    network, storage,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct RemoteAccess {
    config: network::RemoteAccess,
    server: Option<server::Server>,
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
enum Request {
    RemoteAccessGet,
    RemoteAccessSet {
        remote_access: network::RemoteAccess,
    },
}
impl RemoteAccess {
    pub fn start(daemon: DaemonId) -> anyhow::Result<Self> {
        let mut remote_access = Self {
            config: storage::load("remote-access.json")?,
            ..Default::default()
        };
        if remote_access.config.enabled {
            match server::Server::start(
                server::Listen {
                    address: remote_access.config.listen_address,
                    authentication: remote_access.config.authentication.clone(),
                },
                ipc::socket_path(),
                daemon,
            ) {
                Ok(server) => remote_access.server = Some(server),
                Err(error) => remote_access.error = Some(error.to_string()),
            }
        }
        Ok(remote_access)
    }
    pub fn bound_address(&self) -> Option<std::net::SocketAddr> {
        self.server.as_ref().map(|s| s.address)
    }
    pub fn handles(method: &str) -> bool {
        matches!(method, "remote-access-get" | "remote-access-set")
    }
    pub fn call(&mut self, daemon: DaemonId, body: Value) -> anyhow::Result<Value> {
        match serde_json::from_value(body)? {
            Request::RemoteAccessGet => Ok(
                json!({"local":{"name":model::hostname(),"daemon":daemon,"machine_kind":model::MachineKind::local()},"hostname":model::hostname(),"remote_access":self.config,"remote_access_error":self.error,"bound_address":self.bound_address()}),
            ),
            Request::RemoteAccessSet { remote_access } => {
                let token = if remote_access.enabled {
                    remote_access.authentication.token()?
                } else {
                    None
                };
                let prepared = if remote_access.enabled
                    && (self.server.is_none()
                        || (self.config.listen_address != remote_access.listen_address
                            && self.bound_address() != Some(remote_access.listen_address)))
                {
                    Some(server::Server::prepare(server::Listen {
                        address: remote_access.listen_address,
                        authentication: remote_access.authentication.clone(),
                    })?)
                } else {
                    None
                };
                storage::save("remote-access.json", &remote_access)?;
                if let Some(prepared) = prepared {
                    self.server = Some(prepared.start(ipc::socket_path(), daemon));
                } else if !remote_access.enabled {
                    self.server = None;
                } else if let Some(server) = &self.server
                    && !server.token_matches(&token)
                {
                    server.set_token(token);
                }
                self.config = remote_access;
                self.error = None;
                Ok(json!({"ok":true,"daemon":daemon}))
            }
        }
    }
}
