//! Isolated accessible-text construction, excluding GTK, transport and rendering.
//! Run with `cargo run --locked --release -p gmux-gtk --example accessible-terminal-benchmark`.

#[allow(dead_code)]
#[path = "../src/accessible_terminal.rs"]
mod accessible_terminal;

use accessible_terminal::Text;
use gmux_client::{Cell, Frame};
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
        cell.set_text(glyph).unwrap();
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

fn retention() -> Vec<Value> {
    let mut results = Vec::new();
    for unicode in [false, true] {
        let mut text = Text::default();
        let mut spare: Option<Text> = None;
        for (cols, rows) in [(80, 24), (240, 80), (512, 256), (80, 24)] {
            let frame = Frame {
                cols,
                rows,
                ..Frame::default()
            };
            let cells = fixture(cols, rows, unicode);
            // Follow the canvas's build, replace and spare-retention sequence.
            let mut updates = Vec::new();
            for _ in 0..3 {
                let mut next = spare.take().unwrap_or_default();
                next.rebuild(frame, &cells, None);
                let used_bytes = next.used_bytes();
                let old = std::mem::replace(&mut text, next);
                spare = (!old.has_excess_capacity(used_bytes)).then_some(old);
                let active_bytes = text.capacity_bytes();
                let spare_bytes = spare.as_ref().map_or(0, Text::capacity_bytes);
                updates.push(json!({
                    "active_capacity_bytes": active_bytes,
                    "active_used_bytes": text.used_bytes(),
                    "spare_capacity_bytes": spare_bytes,
                    "combined_capacity_bytes": active_bytes + spare_bytes,
                }));
            }
            results.push(json!({
                "cols": cols,
                "rows": rows,
                "fixture": if unicode { "unicode" } else { "ascii" },
                "updates": updates,
            }));
        }
    }
    results
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
                text.update_positions(
                    black_box(frame),
                    black_box(Some(gmux_client::selection::Selection::linear(
                        0,
                        cells.len() as u64 - 1,
                    ))),
                );
                black_box(&text);
            });
            let mut column = 1u64;
            let rectangle_positions = measure(|| {
                column = 1 + column % (u64::from(cols) - 2);
                text.update_positions(
                    black_box(frame),
                    black_box(Some(gmux_client::selection::Selection::rectangle(
                        1,
                        u64::from(rows - 1) * u64::from(cols) + column,
                        cols,
                    ))),
                );
                black_box(&text);
            });
            let range_capacity_bytes = text.selection.capacity() * size_of::<(u32, u32)>();
            results.push(json!({
                "cols": cols,
                "rows": rows,
                "fixture": if unicode { "unicode" } else { "ascii" },
                "rectangle_positions": rectangle_positions,
                "range_capacity_bytes": range_capacity_bytes,
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
            "retention": retention(),
        }))
        .unwrap()
    );
}
