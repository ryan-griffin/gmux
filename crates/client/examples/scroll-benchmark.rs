//! Isolate client scroll latency with a simulated round trip, in milliseconds.
//! Arguments: reply delay in ms (default 100), unix|tcp, ascii|long.
use gmux_client::{ipc, remote::SessionManager};
use gmux_protocol::{
    Cell, Frame, Scrollback,
    theme::Theme,
    transport::{CachedRows, Header, Metadata, Packet},
};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

fn cells(start: u64, rows: usize, long: bool) -> Vec<Cell> {
    (start..start + rows as u64)
        .flat_map(|row| {
            let mut cell = Cell::default();
            let text = row_text(row, long);
            cell.set_text(&text).unwrap();
            std::iter::repeat_n(cell, 80)
        })
        .collect()
}

fn row_text(row: u64, long: bool) -> String {
    if long {
        format!("{row:04}{}", "\u{1d185}".repeat(64))
    } else {
        format!("{row:04}")
    }
}

fn run(cached: bool, rtt: Duration, tcp: bool, long: bool) -> anyhow::Result<serde_json::Value> {
    let (client, mut server): (ipc::Stream, ipc::Stream) = if tcp {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let client = TcpStream::connect(listener.local_addr()?)?;
        let (server, _) = listener.accept()?;
        client.set_nodelay(true)?;
        server.set_nodelay(true)?;
        (client.into(), server.into())
    } else {
        let (client, server) = UnixStream::pair()?;
        (client.into(), server.into())
    };
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
        let mut initial = cells(80, 24, long);
        if cached {
            initial.extend(cells(32, 120, long));
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
                cells: cells(row, 24, long),
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
        gmux_platform::unix::ipc::Wake::new()?,
        gmux_platform::unix::identity::daemon_id()?,
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
    let cache_retained_bytes = session.borrow().scroll_cache_bytes();
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
            row_text(target, long)
        );
    }
    timings.sort_by(f64::total_cmp);
    drop(manager);
    worker.join().expect("Scroll responder panicked")?;
    Ok(
        serde_json::json!({ "cached": cached, "samples": timings.len(),
        "median_ms": timings[timings.len()/2], "p95_ms": timings[timings.len()*95/100],
        "cache_retained_bytes": cache_retained_bytes,
        "prefetched_rows": if cached { 120 } else { 0 } }),
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
    let tcp = match std::env::args().nth(2).as_deref().unwrap_or("unix") {
        "unix" => false,
        "tcp" => true,
        _ => anyhow::bail!("Expected unix or tcp"),
    };
    let long = match std::env::args().nth(3).as_deref().unwrap_or("ascii") {
        "ascii" => false,
        "long" => true,
        _ => anyhow::bail!("Expected ascii or long"),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "simulated_rtt_ms": rtt.as_millis(), "transport": if tcp { "tcp" } else { "unix" },
            "text": if long { "long" } else { "ascii" },
            "uncached": run(false, rtt, tcp, long)?, "cached": run(true, rtt, tcp, long)?
        }))?
    );
    Ok(())
}
