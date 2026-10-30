//! Random daemon identities from the local operating system.
use gmux_protocol::model::DaemonId;
use std::io::Read;

pub fn daemon_id() -> anyhow::Result<DaemonId> {
    let mut bytes = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(DaemonId::from_bytes(bytes))
}
