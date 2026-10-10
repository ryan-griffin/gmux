//! Isolated accessible-text construction, excluding GTK, transport and rendering.
//! Run with `cargo run --locked --release --example accessible-terminal-benchmark`.

#[allow(dead_code)]
#[path = "../src/accessible_terminal.rs"]
mod accessible_terminal;

use accessible_terminal::Text;
use gmux::{Cell, Frame};
use serde_json::{Value, json};
use std::{hint::black_box, time::Instant};

fn fixture(cols: u16, rows: u16, unicode: bool) -> Vec<Cell> {
    let mut cells = vec![Cell::default(); cols as usize * rows as usize];
    for (i, cell) in cells.iter_mut().enumerate() {
        let x = i % cols as usize;
        let glyph = if unicode {
            ["界", "", "e\u{301}", "👩‍💻", "", "X", "", " "][x % 8]
        } else {
            "X"
        };
        cell.text[..glyph.len()].copy_from_slice(glyph.as_bytes());
        if unicode && matches!(x % 8, 1 | 4) {
            cell.flags = 64;
        }
    }
    cells
}

fn measure(mut run: impl FnMut()) -> Value {
    for _ in 0..10 {
        run();
    }
    let start = Instant::now();
    for _ in 0..10 {
        run();
    }
    // Each of the 21 samples averages at least about 10 ms of work.
    let iterations = (0.01 / (start.elapsed().as_secs_f64() / 10.0))
        .ceil()
        .clamp(10.0, 1_000_000.0) as usize;
    let mut samples = Vec::with_capacity(21);
    for _ in 0..21 {
        let start = Instant::now();
        for _ in 0..iterations {
            run();
        }
        samples.push(start.elapsed().as_secs_f64() * 1e6 / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    json!({
        "iterations_per_sample": iterations,
        "samples": samples.len(),
        "median_batch_mean_us": samples[samples.len() / 2],
        "min_batch_mean_us": samples[0],
        "max_batch_mean_us": samples[samples.len() - 1],
    })
}

fn main() {
    let mut results = Vec::new();
    for (cols, rows) in [(80, 24), (240, 80), (512, 256)] {
        let frame = Frame {
            cols,
            rows,
            ..Frame::default()
        };
        for unicode in [false, true] {
            let cells = fixture(cols, rows, unicode);
            let fresh = measure(|| {
                black_box(Text::from_frame(black_box(frame), black_box(&cells), None));
            });
            let mut text = Text::from_frame(frame, &cells, None);
            let mut spare = Text::from_frame(frame, &cells, None);
            let reused = measure(|| {
                spare.rebuild(black_box(frame), black_box(&cells), None);
                std::mem::swap(&mut text, &mut spare);
                black_box(&text);
            });
            let previous_row = &cells[..cols as usize];
            let mut recolored = previous_row.to_vec();
            for cell in &mut recolored {
                cell.fg = [1, 2, 3];
                cell.bg = [4, 5, 6];
                cell.flags |= 63;
            }
            let color_row = measure(|| {
                let text_changed = black_box(previous_row)
                    .iter()
                    .zip(black_box(&recolored))
                    .any(|(a, b)| !a.text_eq(b));
                assert!(!text_changed);
                text.update_positions(black_box(frame), black_box(Some((0, cells.len() - 1))));
                black_box(&text);
            });
            results.push(json!({
                "cols": cols,
                "rows": rows,
                "fixture": if unicode { "unicode" } else { "ascii" },
                "fresh": fresh,
                "reused": reused,
                "color_row": color_row,
            }));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "scope": "isolated accessible text; no GTK, transport or rendering",
            "results": results,
        }))
        .unwrap()
    );
}
