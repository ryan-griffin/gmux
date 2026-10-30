//! Measure client resource use with idle streams and no daemon or GUI.
use gmux_client::{ipc, remote::SessionManager};
use gmux_platform::unix::ipc::Wake;
use std::{
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

fn resources() -> serde_json::Value {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let value = |key: &str| -> u64 {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    serde_json::json!({"threads":value("Threads:"),"rss_kib":value("VmRSS:"),"virtual_kib":value("VmSize:")})
}
fn main() -> anyhow::Result<()> {
    let count = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "16".into())
        .parse::<usize>()?;
    let wake = Wake::new()?;
    let daemon = gmux_platform::unix::identity::daemon_id()?;
    let before = resources();
    let start = Instant::now();
    let mut streams = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..count {
        let (socket, peer) = UnixStream::pair()?;
        streams.push(SessionManager::from_stream(
            ipc::Endpoint::local(),
            wake.clone(),
            daemon,
            socket,
        )?);
        peers.push(peer);
    }
    let setup_us = start.elapsed().as_micros();
    std::thread::sleep(Duration::from_millis(250));
    let connected = resources();
    let start = Instant::now();
    drop(streams);
    let drop_us = start.elapsed().as_micros();
    std::thread::sleep(Duration::from_millis(250));
    println!(
        "{}",
        serde_json::json!({"connections":count,"before":before,"connected":connected,"after":resources(),"setup_us":setup_us,"drop_us":drop_us})
    );
    Ok(())
}
