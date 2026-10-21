//! Isolate client scroll latency with a simulated round trip, in milliseconds.
//! Pass 0 for Unix socket latency; the default is 100 ms.
use gmux_client::{
    Cell, Frame, Scrollback, ipc,
    model::DaemonId,
    remote::SessionManager,
    theme::Theme,
    transport::{CachedRows, Header, Metadata, Packet},
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

fn cells(start: u64, rows: usize) -> Vec<Cell> {
    (start..start + rows as u64)
        .flat_map(|row| {
            let mut cell = Cell::default();
            let text = format!("{row:04}");
            cell.text[..text.len()].copy_from_slice(text.as_bytes());
            std::iter::repeat_n(cell, 80)
        })
        .collect()
}

fn run(cached: bool, rtt: Duration) -> anyhow::Result<serde_json::Value> {
    let (client, mut server) = UnixStream::pair()?;
    let meta = Metadata {
        id: 1,
        size: (80, 24),
        ..Metadata::default()
    };
    let frame = Frame {
        cols: 80,
        rows: 24,
        scrollback: Scrollback {
            total: 1000,
            offset: 80,
            len: 24,
        },
        ..Frame::default()
    };
    let initial_meta = meta.clone();
    let worker = std::thread::spawn(move || -> anyhow::Result<()> {
        let mut reader = BufReader::new(server.try_clone()?);
        let mut initial = cells(80, 24);
        if cached {
            initial.extend(cells(32, 120));
        }
        let packet = Packet {
            header: Header {
                meta: initial_meta.clone(),
                frame: Some(frame),
                rows: (0..24).collect(),
                generation: 1,
                cache: cached.then(|| CachedRows {
                    start: 32,
                    row_flags: vec![0; 120],
                }),
                ..Header::default()
            },
            cells: initial,
        };
        let mut bytes = Vec::new();
        packet.encode_into(&mut bytes)?;
        server.write_all(&bytes)?;
        let mut line = String::new();
        while reader.read_line(&mut line)? != 0 {
            let command: serde_json::Value = serde_json::from_str(&line)?;
            line.clear();
            if command["method"] != "scroll-to" {
                continue;
            }
            std::thread::sleep(rtt);
            let row = command["row"].as_u64().unwrap();
            let packet = Packet {
                header: Header {
                    meta: initial_meta.clone(),
                    frame: Some(Frame {
                        scrollback: Scrollback {
                            offset: row,
                            ..frame.scrollback
                        },
                        ..frame
                    }),
                    generation: 1,
                    scroll_serial: command["serial"].as_u64().unwrap(),
                    rows: (0..24).collect(),
                    ..Header::default()
                },
                cells: cells(row, 24),
            };
            bytes.clear();
            packet.encode_into(&mut bytes)?;
            if server.write_all(&bytes).is_err() {
                break;
            }
        }
        Ok(())
    });
    let mut manager = SessionManager::from_stream(
        ipc::Endpoint::local(),
        ipc::Wake::new()?,
        DaemonId::generate()?,
        client,
    )?;
    manager.install_metadata(&[meta], 0);
    let session = manager.attach(1, &Theme::adwaita(true))?;
    manager.set_visible([1].into())?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while session.borrow().snapshot().frame.scrollback.offset != 80 {
        manager.poll();
        anyhow::ensure!(Instant::now() < deadline, "Initial viewport timed out");
        std::thread::sleep(Duration::from_micros(100));
    }
    let mut timings = Vec::new();
    for index in 1..=40 {
        let target = 80 + (index % 2) as u64;
        let start = Instant::now();
        session.borrow_mut().scroll_to(target);
        while session.borrow().snapshot().frame.scrollback.offset != target {
            manager.poll();
            anyhow::ensure!(start.elapsed() < Duration::from_secs(3), "Scroll timed out");
            std::thread::sleep(Duration::from_micros(100));
        }
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(
            session.borrow().snapshot().cells[0].text(),
            format!("{target:04}")
        );
    }
    timings.sort_by(f64::total_cmp);
    drop(manager);
    worker.join().expect("Scroll responder panicked")?;
    Ok(
        serde_json::json!({ "cached": cached, "samples": timings.len(),
        "median_ms": timings[timings.len()/2], "p95_ms": timings[timings.len()*95/100],
        "cache_budget_bytes": gmux_client::scroll_cache::MAX_BYTES,
        "cache_rows": if cached { gmux_client::scroll_cache::capacity(80).min(24*5) } else { 0 } }),
    )
}

fn main() -> anyhow::Result<()> {
    let rtt = Duration::from_millis(
        std::env::args()
            .nth(1)
            .as_deref()
            .unwrap_or("100")
            .parse()?,
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "simulated_rtt_ms": rtt.as_millis(), "uncached": run(false, rtt)?, "cached": run(true, rtt)?
        }))?
    );
    Ok(())
}
