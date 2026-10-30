//! Network handshake values, limits, and bounded JSON framing.
use crate::model::DaemonId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    borrow::Cow,
    io::{BufRead, Read, Write},
    net::{AddrParseError, Ipv6Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};
pub const DEFAULT_PORT: u16 = 7331;
pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const CONTROL_TIMEOUT: Duration = TIMEOUT.saturating_add(TIMEOUT);
pub const MAX_HANDSHAKE: usize = 4096;
pub const MAX_REQUEST: usize = 1024 * 1024;
pub const MAX_REPLY: usize = 16 * 1024 * 1024;

/// Supply the default port without resolving hostnames on the caller's thread.
pub fn with_default_port(address: &str) -> Cow<'_, str> {
    if !address.contains(':') || address.ends_with(']') {
        format!("{address}:{DEFAULT_PORT}").into()
    } else if address
        .split_once('%')
        .map_or(address, |(ip, _)| ip)
        .parse::<Ipv6Addr>()
        .is_ok()
    {
        format!("[{address}]:{DEFAULT_PORT}").into()
    } else {
        address.into()
    }
}

/// Parse a listening IP address, using the default port when omitted.
pub fn parse_listen_address(address: &str) -> Result<SocketAddr, AddrParseError> {
    with_default_port(address).parse()
}

fn deserialize_listen_address<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SocketAddr, D::Error> {
    let address = String::deserialize(deserializer)?;
    parse_listen_address(&address).map_err(serde::de::Error::custom)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Authentication {
    None,
    Token { file: PathBuf },
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAccess {
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_listen_address")]
    pub listen_address: SocketAddr,
    pub authentication: Authentication,
}
impl Default for RemoteAccess {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_address: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            authentication: Authentication::None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Channel {
    Control,
    Stream,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handshake {
    pub version: u32,
    pub channel: Channel,
    pub token: Option<String>,
    pub daemon: Option<DaemonId>,
}
#[derive(Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub version: u32,
    pub daemon: DaemonId,
    pub home_directory: PathBuf,
    pub machine_kind: crate::model::MachineKind,
    pub hostname: String,
}

pub fn read_json(reader: &mut impl BufRead, limit: usize) -> anyhow::Result<Value> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= limit && bytes.last() == Some(&b'\n'),
        "Invalid or oversized network message"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn write_json(writer: &mut impl Write, value: &impl Serialize) -> anyhow::Result<()> {
    // Serialize before writing so JSON fragments do not each need a socket
    // write and deadline update with TCP_NODELAY enabled.
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    Ok(())
}

pub fn check_error(value: &Value) -> anyhow::Result<()> {
    if let Some(error) = value.get("error") {
        anyhow::bail!("{}", error.as_str().unwrap_or("Connection request failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    #[test]
    fn listener_settings_accept_optional_ports_and_save_the_resolved_port() {
        for (input, expected) in [
            ("127.0.0.1", "127.0.0.1:7331"),
            ("::1", "[::1]:7331"),
            ("[::1]", "[::1]:7331"),
            ("fe80::1%3", "[fe80::1%3]:7331"),
            ("127.0.0.1:0", "127.0.0.1:0"),
            ("127.0.0.1:8400", "127.0.0.1:8400"),
            ("[::1]:8400", "[::1]:8400"),
        ] {
            assert_eq!(parse_listen_address(input).unwrap().to_string(), expected);
            let settings: RemoteAccess = serde_json::from_value(json!({
                "enabled":true,"listen_address":input,"authentication":{"mode":"none"}
            }))
            .unwrap();
            assert_eq!(settings.listen_address.to_string(), expected);
            assert_eq!(
                serde_json::to_value(settings).unwrap()["listen_address"],
                expected
            );
        }
        for input in [
            "",
            "localhost",
            "[::1",
            "127.0.0.1:",
            "[::1]:",
            "127.0.0.1:65536",
        ] {
            assert!(parse_listen_address(input).is_err(), "Accepted {input}");
        }
    }

    #[test]
    fn messages_require_a_newline_and_obey_the_byte_limit() {
        let mut pipelined = Cursor::new(b"{}\n{\"next\":true}\n");
        assert_eq!(read_json(&mut pipelined, 3).unwrap(), json!({}));
        assert_eq!(read_json(&mut pipelined, 14).unwrap(), json!({"next":true}));
        assert!(read_json(&mut Cursor::new(b"{}"), 3).is_err());
        let mut oversized = Cursor::new(vec![b' '; MAX_HANDSHAKE * 2]);
        assert!(read_json(&mut oversized, MAX_HANDSHAKE).is_err());
        assert_eq!(oversized.position(), MAX_HANDSHAKE as u64 + 1);
    }
}
