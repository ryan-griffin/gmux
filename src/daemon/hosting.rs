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
pub(super) struct Hosting {
    config: network::Hosting,
    server: Option<server::Server>,
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
enum Request {
    HostingGet,
    HostingSet { hosting: network::Hosting },
}
impl Hosting {
    pub fn start(daemon: DaemonId) -> anyhow::Result<Self> {
        let mut hosting = Self {
            config: storage::load("hosting.json")?,
            ..Default::default()
        };
        if hosting.config.enabled {
            match server::Server::start(
                server::Listen {
                    address: hosting.config.address,
                    authentication: hosting.config.authentication.clone(),
                },
                ipc::socket_path(),
                daemon,
            ) {
                Ok(server) => hosting.server = Some(server),
                Err(error) => hosting.error = Some(error.to_string()),
            }
        }
        Ok(hosting)
    }
    pub fn address(&self) -> Option<std::net::SocketAddr> {
        self.server.as_ref().map(|s| s.address)
    }
    pub fn handles(method: &str) -> bool {
        matches!(method, "hosting-get" | "hosting-set")
    }
    pub fn call(&mut self, daemon: DaemonId, body: Value) -> anyhow::Result<Value> {
        match serde_json::from_value(body)? {
            Request::HostingGet => Ok(
                json!({"local":{"name":model::hostname(),"daemon":daemon,"kind":model::MachineKind::local()},"hostname":model::hostname(),"hosting":self.config,"hosting_error":self.error,"listen":self.address()}),
            ),
            Request::HostingSet { hosting } => {
                let token = if hosting.enabled {
                    hosting.authentication.token()?
                } else {
                    None
                };
                let prepared = if hosting.enabled
                    && (self.server.is_none()
                        || (self.config.address != hosting.address
                            && self.address() != Some(hosting.address)))
                {
                    Some(server::Server::prepare(server::Listen {
                        address: hosting.address,
                        authentication: hosting.authentication.clone(),
                    })?)
                } else {
                    None
                };
                storage::save("hosting.json", &hosting)?;
                if let Some(prepared) = prepared {
                    self.server = Some(prepared.start(ipc::socket_path(), daemon));
                } else if !hosting.enabled {
                    self.server = None;
                } else if let Some(server) = &self.server
                    && !server.token_matches(&token)
                {
                    server.set_token(token);
                }
                self.config = hosting;
                self.error = None;
                Ok(json!({"ok":true,"daemon":daemon}))
            }
        }
    }
}
