//! Measure terminal scene preparation and completed rendering work.
//! Run on a native display with `cargo run --release --example renderer-benchmark`.

#[allow(dead_code)]
#[path = "../src/render.rs"]
mod render;
#[allow(dead_code)]
#[path = "../src/renderer.rs"]
mod renderer;

use clap::Parser;
use gmux::{Cell, Frame, theme::Theme};
use gtk::{graphene, pango, prelude::*};
use renderer::{Renderer, Scene};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Instant};

#[derive(Parser)]
struct Options {
    #[arg(long, default_value_t = 60)]
    frames: usize,
    #[arg(long, default_value_t = 120)]
    cols: u16,
    #[arg(long, default_value_t = 40)]
    rows: u16,
    #[arg(long, default_value = "artifacts/renderer-benchmark")]
    output: PathBuf,
}

fn summary(mut samples: Vec<f64>) -> Value {
    samples.sort_by(f64::total_cmp);
    json!({"median_ms": samples[samples.len() / 2], "p95_ms": samples[(samples.len() * 95 / 100).min(samples.len() - 1)]})
}

fn cell(text: &str, fg: [u8; 3], bg: [u8; 3], flags: u8) -> Cell {
    let mut cell = Cell {
        fg,
        bg,
        flags,
        ..Cell::default()
    };
    cell.text[..text.len()].copy_from_slice(text.as_bytes());
    cell
}

fn fixture(cols: u16, rows: u16, theme: Theme, mixed: bool) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(cols as usize * rows as usize);
    let ascii =
        b"2026-10-02 12:34:56 INFO worker: received request and completed task successfully | ";
    let unicode = [
        "界",
        "e\u{301}",
        "👩‍💻",
        "┃",
        "▀",
        "▄",
        "▚",
        "░",
        "▒",
        "▓",
        "█",
        "┼",
    ];
    for y in 0..rows as usize {
        let mut x = 0;
        while x < cols as usize {
            let fg = theme.palette[1 + (x / 20 + y / 4) % 7];
            let flags = if mixed { [0, 1, 2, 4, 8, 16][y % 6] } else { 0 };
            if mixed && x % 18 < 2 && x + 1 < cols as usize {
                let text = unicode[(x / 18 + y) % unicode.len()];
                cells.push(cell(text, fg, theme.background, flags));
                x += 1;
                if text == "界" || text == "👩‍💻" {
                    cells.push(cell("", fg, theme.background, flags | 64));
                    x += 1;
                }
            } else {
                let text = (ascii[(x + y) % ascii.len()] as char).to_string();
                cells.push(cell(&text, fg, theme.background, flags));
                x += 1;
            }
        }
    }
    cells
}

fn main() -> anyhow::Result<()> {
    let options = Options::parse();
    anyhow::ensure!((10..=300).contains(&options.frames), "Use 10 to 300 frames");
    anyhow::ensure!(
        (20..=240).contains(&options.cols) && (8..=100).contains(&options.rows),
        "Grid is outside benchmark limits"
    );
    std::fs::create_dir_all(&options.output)?;
    gtk::init()?;
    let window = gtk::Window::builder()
        .title("Gmux renderer benchmark")
        .build();
    window.present();
    let gpu = window
        .renderer()
        .ok_or_else(|| anyhow::anyhow!("Window has no renderer"))?;
    let context = window.pango_context();
    let font = pango::FontDescription::from_string("Monospace 12");
    let measure = pango::Layout::new(&context);
    measure.set_font_description(Some(&font));
    measure.set_text("M");
    let (cw, ch) = measure.pixel_size();
    let metrics = (cw.max(1) as f64, (ch + 2).max(1) as f64);
    let width = (options.cols as f64 * metrics.0) as i32;
    let height = (options.rows as f64 * metrics.1) as i32;
    let viewport = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let theme = Theme::adwaita(true);
    let frame = Frame {
        cols: options.cols,
        rows: options.rows,
        background: theme.background,
        foreground: theme.foreground,
        cursor: theme.cursor,
        ..Frame::default()
    };
    let mut results = Vec::new();
    for fixture_name in ["ascii", "mixed", "tui", "fallback"] {
        for workload in ["static", "single-row", "full-screen", "all-rows"] {
            let mut renderer = Renderer::new();
            let mut cells = fixture(options.cols, options.rows, theme, fixture_name != "ascii");
            if fixture_name == "fallback" {
                for (index, cell) in cells.iter_mut().enumerate() {
                    let x = index % options.cols as usize;
                    let y = index / options.cols as usize;
                    *cell = self::cell(
                        ["a", "b", "c", "界", "", "🙂", "", " "][x % 8],
                        theme.palette[1 + y % 7],
                        theme.background,
                        if matches!(x % 8, 4 | 6) { 64 } else { 0 },
                    );
                }
            }
            if fixture_name == "tui" {
                for (index, cell) in cells.iter_mut().enumerate() {
                    let x = index % options.cols as usize;
                    let y = index / options.cols as usize;
                    cell.bg = if y == 0 || y == options.rows as usize - 1 {
                        theme.palette[4]
                    } else if y.is_multiple_of(10) {
                        theme.palette[8]
                    } else if x < options.cols as usize / 3 {
                        [30, 45, 55]
                    } else {
                        theme.background
                    };
                }
            }
            let highlights = vec![0; cells.len()];
            let mut prepare = Vec::new();
            let mut complete = Vec::new();
            let mut total = Vec::new();
            let mut rebuilt_rows = 0;
            let mut new_text_nodes = 0;
            let mut cached_glyphs = 0;
            let mut pixels = vec![0; width as usize * height as usize * 4];
            for iteration in 0..options.frames + 5 {
                if workload != "static" {
                    if workload == "full-screen" {
                        cells.rotate_left(options.cols as usize);
                    }
                    let y = if workload == "single-row" {
                        0
                    } else {
                        options.rows as usize - 1
                    };
                    {
                        let index = y * options.cols as usize + options.cols as usize - 1;
                        cells[index] = cell(
                            &((b'a' + (iteration % 26) as u8) as char).to_string(),
                            theme.foreground,
                            theme.background,
                            0,
                        );
                    }
                    if workload == "all-rows" {
                        for y in 0..options.rows as usize {
                            let index = y * options.cols as usize;
                            cells[index] = cell(
                                &((b'a' + (iteration % 26) as u8) as char).to_string(),
                                theme.foreground,
                                theme.background,
                                0,
                            );
                        }
                    }
                }
                let scene = Scene {
                    frame,
                    cells: &cells,
                    metrics,
                    scale: 1.0,
                    origin: (0.0, 0.0),
                    width,
                    height,
                    theme,
                    selection: None,
                    matches: &highlights,
                    focused: false,
                    exited: false,
                };
                let started = Instant::now();
                let snapshot = gtk::Snapshot::new();
                let stats = renderer.snapshot(&snapshot, &scene, &font, &context);
                let node = snapshot.to_node().unwrap();
                let prepared = started.elapsed().as_secs_f64() * 1000.0;
                let gpu_started = Instant::now();
                let texture = gpu.render_texture(&node, Some(&viewport));
                // Download forces GPU completion. Command submission alone is
                // not a valid comparison with Cairo's CPU painting time.
                texture.download(&mut pixels, width as usize * 4);
                let rendered = gpu_started.elapsed().as_secs_f64() * 1000.0;
                if iteration >= 5 {
                    let expected = match workload {
                        "static" => 0,
                        "single-row" => 1,
                        "all-rows" => options.rows as usize,
                        _ => 1,
                    };
                    assert_eq!(
                        stats.rebuilt_rows, expected,
                        "Row cache invalidation in {workload}"
                    );
                    prepare.push(prepared);
                    complete.push(rendered);
                    total.push(prepared + rendered);
                    rebuilt_rows += stats.rebuilt_rows;
                    new_text_nodes += stats.text_nodes;
                    cached_glyphs = stats.cached_glyphs;
                }
                if iteration == options.frames + 4 {
                    let name = format!("{fixture_name}-{workload}-gsk");
                    texture.save_to_png(options.output.join(format!("{name}.png")))?;
                    node.write_to_file(options.output.join(format!("{name}.node")))?;
                }
            }
            let result = json!({"fixture": fixture_name, "workload": workload, "terminal_renderer": "gsk",
                    "prepare": summary(prepare), "render_and_download": summary(complete), "total": summary(total),
                    "rebuilt_rows": rebuilt_rows, "new_text_nodes": new_text_nodes, "cached_glyphs": cached_glyphs});
            println!("{result}");
            results.push(result);
        }
    }
    let report = json!({"gtk_renderer": gpu.type_().name(),
        "gtk_version": format!("{}.{}.{}", gtk::major_version(), gtk::minor_version(), gtk::micro_version()),
        "font": font.to_string(), "cols": options.cols, "rows": options.rows, "metrics": metrics,
        "width": width, "height": height, "frames": options.frames, "warmup_frames": 5, "results": results});
    std::fs::write(
        options.output.join("results.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    window.close();
    gpu.unrealize();
    Ok(())
}
