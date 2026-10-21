//! Check terminal rendering against GTK's independent Pango layout drawing.
mod cursor {
    include!("../src/terminal/cursor.rs");

    // Native-only observation, without counters or extra state in the client.
    pub(super) fn source_id(cursor: &Cursor) -> Option<u32> {
        cursor.source.borrow().as_ref().map(glib::SourceId::as_raw)
    }

    pub(super) fn timing(cursor: &Cursor) -> (Instant, i64) {
        let source = cursor.source.borrow();
        let source = glib::MainContext::default()
            .find_source_by_id(source.as_ref().unwrap())
            .unwrap();
        (cursor.deadline.get(), source.ready_time())
    }
}
#[allow(dead_code)]
#[path = "../src/render.rs"]
mod render;
#[allow(dead_code)]
#[path = "../src/renderer.rs"]
mod renderer;

use clap::{Parser, ValueEnum};
use gmux_client::{
    Cell, Frame,
    cells::{decoration, mouse_shape, underline},
    theme::Theme,
};
use gtk::{gdk, graphene, gsk, pango, prelude::*};
use renderer::{Renderer, Scene};
use serde_json::json;
use std::{
    ops::Range,
    path::{Path, PathBuf},
};

#[derive(Parser)]
struct Options {
    #[arg(long, default_value = "artifacts/renderer-check")]
    output: PathBuf,
    /// Use the selected GTK backend for cache pixels instead of the Cairo oracle.
    #[arg(long, value_enum, default_value_t = CacheBackend::Cairo)]
    cache_renderer: CacheBackend,
    /// Run the native focus checks driven by renderer-smoke-test.py.
    #[arg(long)]
    cursor_lifecycle: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum CacheBackend {
    Cairo,
    Selected,
}

struct OffscreenRenderer(gsk::Renderer);

impl Drop for OffscreenRenderer {
    fn drop(&mut self) {
        self.0.unrealize();
    }
}

fn cell(text: &str, theme: Theme, flags: u8) -> Cell {
    let mut cell = Cell {
        fg: theme.foreground,
        bg: theme.background,
        flags,
        ..Cell::default()
    };
    cell.text[..text.len()].copy_from_slice(text.as_bytes());
    cell
}

fn scene(cells: &[Cell], theme: Theme) -> Scene<'_> {
    Scene {
        frame: Frame {
            cols: 40,
            rows: 8,
            background: theme.background,
            foreground: theme.foreground,
            cursor: theme.cursor,
            ..Frame::default()
        },
        cells,
        metrics: (10.0, 24.0),
        scale: 1.0,
        origin: (0.0, 0.0),
        width: 400,
        height: 192,
        theme,
        selection: None,
        hovered_link: None,
        matches: &[],
        focused: true,
        exited: false,
    }
}

fn fixture(theme: Theme) -> Vec<Cell> {
    let mut cells = vec![cell(" ", theme, 0); 40 * 8];
    let clusters = [
        "A",
        "m",
        "e\u{301}",
        "界",
        "👩‍💻",
        "🙂",
        "א",
        "م",
        "\u{e0b0}",
        "j",
    ];
    for y in 0..8 {
        let flags = [0, 1, 2, 3, 4, 0, 16, 16][y];
        let mut x = 0;
        while x < 40 {
            let cluster = clusters[(x + y) % clusters.len()];
            cells[y * 40 + x] = cell(cluster, theme, flags);
            if matches!(cluster, "界" | "👩‍💻" | "🙂") && x + 1 < 40 {
                x += 1;
                cells[y * 40 + x] = cell("", theme, flags | 64);
            }
            x += 1;
        }
    }
    for y in [5, 7] {
        for cell in &mut cells[y * 40..(y + 1) * 40] {
            cell.decoration = underline::SINGLE;
        }
    }
    cells
}

fn rgba(rgb: [u8; 3], alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
        alpha,
    )
}

// The reference deliberately does not extract, reposition, or batch glyphs.
// GTK draws one complete Pango layout per supplied text span.
fn reference(
    scene: &Scene<'_>,
    font: &pango::FontDescription,
    context: &pango::Context,
) -> gsk::RenderNode {
    let snapshot = gtk::Snapshot::new();
    snapshot.append_color(
        &rgba(scene.frame.background, 1.0),
        &graphene::Rect::new(0.0, 0.0, scene.width as f32, scene.height as f32),
    );
    for (index, cell) in scene.cells.iter().enumerate() {
        if cell.bg == scene.frame.background {
            continue;
        }
        let (cw, ch) = scene.metrics;
        let x = (index % scene.frame.cols as usize) as f64 * cw;
        let y = (index / scene.frame.cols as usize) as f64 * ch;
        let snap_x =
            |x: f64| ((x + scene.origin.0) * scene.scale).round() / scene.scale - scene.origin.0;
        let snap_y =
            |y: f64| ((y + scene.origin.1) * scene.scale).round() / scene.scale - scene.origin.1;
        snapshot.append_color(
            &rgba(cell.bg, 1.0),
            &graphene::Rect::new(
                snap_x(x) as f32,
                snap_y(y) as f32,
                (snap_x(x + cw) - snap_x(x)) as f32,
                (snap_y(y + ch) - snap_y(y)) as f32,
            ),
        );
    }
    for (index, cell) in scene.cells.iter().enumerate() {
        if cell.flags & 32 != 0 {
            continue;
        }
        let (cw, ch) = scene.metrics;
        let x = (index % scene.frame.cols as usize) as f64 * cw;
        let y = (index / scene.frame.cols as usize) as f64 * ch;
        let color = rgba(cell.fg, if cell.flags & 4 != 0 { 0.5 } else { 1.0 });
        if !cell.text().is_empty() && cell.text() != " " {
            let layout = pango::Layout::new(context);
            let mut font = font.clone();
            if cell.flags & 1 != 0 {
                font.set_weight(pango::Weight::Bold);
            }
            if cell.flags & 2 != 0 {
                font.set_style(pango::Style::Italic);
            }
            layout.set_font_description(Some(&font));
            layout.set_text(cell.text());
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x as f32, (y + 1.0) as f32));
            snapshot.append_layout(&layout, &color);
            snapshot.restore();
        }
        for (present, line_y) in [
            (cell.underline() != underline::NONE, ch - 2.0),
            (cell.flags & 16 != 0, ch / 2.0),
        ] {
            if present {
                // Reference snapping is expressed in global device coordinates.
                let left =
                    ((x + scene.origin.0) * scene.scale).round() / scene.scale - scene.origin.0;
                let right = ((x + cw + scene.origin.0) * scene.scale).round() / scene.scale
                    - scene.origin.0;
                let top = ((y + line_y + scene.origin.1) * scene.scale).round() / scene.scale
                    - scene.origin.1;
                let bottom = ((y + line_y + 1.0 + scene.origin.1) * scene.scale).round()
                    / scene.scale
                    - scene.origin.1;
                snapshot.append_color(
                    &color,
                    &graphene::Rect::new(
                        left as f32,
                        top as f32,
                        (right - left) as f32,
                        (bottom - top) as f32,
                    ),
                );
            }
        }
    }
    snapshot.to_node().unwrap()
}

fn render(gpu: &gsk::Renderer, node: &gsk::RenderNode, scene: &Scene<'_>) -> gdk::Texture {
    let snapshot = gtk::Snapshot::new();
    snapshot.scale(scene.scale as f32, scene.scale as f32);
    snapshot.translate(&graphene::Point::new(
        scene.origin.0 as f32,
        scene.origin.1 as f32,
    ));
    snapshot.append_node(node);
    gpu.render_texture(
        snapshot.to_node().unwrap(),
        Some(&graphene::Rect::new(
            0.0,
            0.0,
            (scene.width as f64 * scene.scale).ceil() as f32,
            (scene.height as f64 * scene.scale).ceil() as f32,
        )),
    )
}

fn pixels(texture: &gdk::Texture) -> Vec<u8> {
    let mut pixels = vec![0; texture.width() as usize * texture.height() as usize * 4];
    texture.download(&mut pixels, texture.width() as usize * 4);
    pixels
}

// Cache correctness needs a deterministic pixel oracle. Some Vulkan drivers
// produce transient outliers even for identical nodes. Compare exact Cairo
// pixels; selected-backend drawing stays covered by the independent Pango and
// geometry checks below. Node serialization belongs in failure diagnostics:
// serializing every pair is much slower than rasterizing these scenes.
fn assert_render_eq(
    gpu: &gsk::Renderer,
    actual_node: &gsk::RenderNode,
    expected_node: &gsk::RenderNode,
    scene: &Scene<'_>,
    output: &Path,
    name: &str,
) {
    let actual = render(gpu, actual_node, scene);
    let actual_pixels = pixels(&actual);
    let expected = render(gpu, expected_node, scene);
    let expected_pixels = pixels(&expected);
    let actual_size = (actual.width(), actual.height());
    let expected_size = (expected.width(), expected.height());
    if actual_size == expected_size && actual_pixels == expected_pixels {
        return;
    }
    let nodes_equal = actual_node.serialize() == expected_node.serialize();
    let width = actual.width() as usize;
    let mut count = 0;
    let mut bounds = None;
    let mut first = Vec::new();
    let mut max_channel_delta = 0;
    for (index, (a, b)) in actual_pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(expected_pixels.as_chunks::<4>().0)
        .enumerate()
    {
        if a == b {
            continue;
        }
        count += 1;
        let (x, y) = (index % width, index / width);
        bounds = Some(match bounds {
            None => (x, y, x, y),
            Some((left, top, right, bottom)) => {
                (x.min(left), y.min(top), x.max(right), y.max(bottom))
            }
        });
        for (&a, &b) in a.iter().zip(b) {
            max_channel_delta = max_channel_delta.max(a.abs_diff(b));
        }
        if first.len() < 8 {
            first.push(json!({"x": x, "y": y, "actual_bgra": a, "expected_bgra": b}));
        }
    }
    let report = json!({
        "case": name, "gtk_renderer": gpu.type_().name(),
        "actual_size": actual_size, "expected_size": expected_size,
        "scale": scene.scale, "origin": scene.origin, "metrics": scene.metrics,
        "nodes_equal": nodes_equal, "differing_pixels": count,
        "bounds": bounds, "max_channel_delta": max_channel_delta, "first": first,
    });
    std::fs::write(
        output.join(format!("{name}-mismatch.json")),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .unwrap();
    for (suffix, texture, node) in [
        ("actual", &actual, actual_node),
        ("expected", &expected, expected_node),
    ] {
        texture
            .save_to_png(output.join(format!("{name}-{suffix}.png")))
            .unwrap();
        node.write_to_file(output.join(format!("{name}-{suffix}.node")))
            .unwrap();
    }
    panic!(
        "{name}: pixel mismatch on {}: nodes_equal={nodes_equal}, \
        sizes={actual_size:?}/{expected_size:?}, {count} differing pixels, \
        bounds={bounds:?}, max channel delta={max_channel_delta}; see {}",
        gpu.type_().name(),
        output.join(format!("{name}-mismatch.json")).display()
    );
}

fn node(
    renderer: &mut Renderer,
    scene: &Scene<'_>,
    font: &pango::FontDescription,
    context: &pango::Context,
) -> (gsk::RenderNode, renderer::RenderStats) {
    let snapshot = gtk::Snapshot::new();
    let stats = renderer.snapshot(&snapshot, scene, font, context);
    (snapshot.to_node().unwrap(), stats)
}

fn assert_direct_text(node: &gsk::RenderNode) {
    assert_ne!(node.node_type(), gsk::RenderNodeType::CairoNode);
    for child in node.children() {
        assert_direct_text(&child);
    }
}

fn validate_overlapping_fallbacks(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
) {
    let theme = Theme::adwaita(true);
    let mut cells = vec![cell(" ", theme, 0); 40 * 8];
    for (index, target) in cells.iter_mut().enumerate() {
        *target = cell(
            ["f", "界", "j", "🙂"][index % 4],
            theme,
            [0, 2, 4, 6][index / 40 % 4],
        );
        target.fg = theme.palette[1 + index % 3];
    }
    let mut scene = scene(&cells, theme);
    // Force ink across neighboring cells to exercise paint-order checks, with
    // alternating fallback fonts, colors, italics, and node opacity.
    scene.metrics.0 = 3.0;
    scene.width = 120;
    let actual = node(&mut Renderer::new(), &scene, font, context).0;
    let expected = reference(&scene, font, context);
    let actual = pixels(&render(gpu, &actual, &scene));
    let expected = pixels(&render(gpu, &expected, &scene));
    let delta = actual
        .iter()
        .zip(&expected)
        .map(|(&a, &b)| (a as f64 - b as f64).abs())
        .sum::<f64>()
        / actual.len() as f64;
    assert!(
        delta < 1.0,
        "Overlapping fallback glyphs changed paint order: {delta}"
    );
}

fn assert_ligature_layout(
    gpu: &gsk::Renderer,
    actual: &gsk::RenderNode,
    expected: &gsk::RenderNode,
    scene: &Scene<'_>,
    spans: &[Range<usize>],
    output: &Path,
    name: &str,
) {
    let actual_texture = render(gpu, actual, scene);
    let expected_texture = render(gpu, expected, scene);
    let width = actual_texture.width() as usize;
    let actual_pixels = pixels(&actual_texture);
    let expected_pixels = pixels(&expected_texture);
    for span in spans {
        let col = span.start % scene.frame.cols as usize;
        let row = span.start / scene.frame.cols as usize;
        let left = (col as f64 * scene.metrics.0 * scene.scale).floor() as usize;
        let right = (((col + span.len()) as f64 * scene.metrics.0 * scene.scale).ceil() as usize)
            .min(width);
        let top = (row as f64 * scene.metrics.1 * scene.scale).floor() as usize;
        let bottom = (((row + 1) as f64 * scene.metrics.1 * scene.scale).ceil() as usize)
            .min(actual_texture.height() as usize);
        let mut delta = 0.0;
        for y in top..bottom {
            let range = (y * width + left) * 4..(y * width + right) * 4;
            delta += actual_pixels[range.clone()]
                .iter()
                .zip(&expected_pixels[range])
                .map(|(&a, &b)| a.abs_diff(b) as f64)
                .sum::<f64>();
        }
        delta /= ((right - left) * (bottom - top) * 4) as f64;
        // Check each operator, rather than diluting a missing glyph in the
        // mostly empty viewport. GPU rasterization allows small edge differences.
        if delta >= 1.0 {
            actual_texture
                .save_to_png(output.join(format!("{name}-actual.png")))
                .unwrap();
            expected_texture
                .save_to_png(output.join(format!("{name}-expected.png")))
                .unwrap();
            panic!(
                "{name}: layout mismatch on {} in {span:?}: mean channel delta {delta}; see {}",
                gpu.type_().name(),
                output.display()
            );
        }
    }
}

fn validate_ligatures(
    gpu: &gsk::Renderer,
    cache_gpu: &gsk::Renderer,
    context: &pango::Context,
    output: &Path,
) {
    context
        .font_map()
        .unwrap()
        .add_font_file(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fonts/GmuxLigatureTest.ttf"),
        )
        .unwrap();
    let font = pango::FontDescription::from_string("Gmux Ligature Test 15px");
    let measure = pango::Layout::new(context);
    measure.set_font_description(Some(&font));
    measure.set_text("M");
    let (cw, ch) = measure.pixel_size();
    let signature = |text: &str| {
        measure.set_text(text);
        let mut glyphs = Vec::new();
        for run in measure.line_readonly(0).unwrap().runs() {
            glyphs.extend(
                run.glyph_string()
                    .glyph_info()
                    .iter()
                    .map(|info| info.glyph()),
            );
        }
        glyphs
    };
    assert_eq!(
        signature("->").len(),
        1,
        "Fixture must form a two-cell ligature"
    );
    assert_eq!(
        signature("ffi").len(),
        1,
        "Fixture must form a three-cell ligature"
    );
    assert_ne!(
        signature("!="),
        [signature("!"), signature("=")].concat(),
        "Fixture must apply contextual substitutions"
    );
    let theme = Theme::adwaita(true);
    for scale in [1.0, 1.5] {
        let mut renderer = Renderer::new();
        for case in [
            "joined",
            "foreground",
            "background",
            "bold",
            "selection",
            "search",
            "cursor",
            "restored",
            "cell-width",
        ] {
            let mut cells = vec![cell(" ", theme, 0); 16 * 2];
            for (index, ch) in "->X !=X ffiX".chars().enumerate() {
                cells[index] = cell(&ch.to_string(), theme, 0);
            }
            // Shaping must stop at a row boundary even when the cells form an operator.
            cells[15] = cell("-", theme, 0);
            cells[16] = cell(">", theme, 0);
            if case == "cell-width" {
                // Complete layouts can draw merged ligatures at their natural width
                // while placing the following glyphs on the wider terminal grid.
                cells[4..7].fill(cell(" ", theme, 0));
            }
            let mut matches = vec![0; cells.len()];
            if case == "search" {
                matches[1] = 2;
            }
            match case {
                "foreground" => cells[1].fg = theme.palette[1],
                "background" => cells[1].bg = theme.palette[4],
                "bold" => cells[1].flags = 1,
                // Boundaries must split shaping even when the effective
                // foreground matches the neighboring text.
                "selection" => cells[1].fg = theme.selection_foreground,
                "cursor" | "restored" => {
                    cells[0].fg = theme.background;
                    cells[1].fg = theme.background;
                    cells[0].bg = theme.palette[4];
                }
                _ => {}
            }
            let mut scene = self::scene(&cells, theme);
            scene.frame.cols = 16;
            scene.frame.rows = 2;
            scene.scale = scale;
            scene.metrics = (cw as f64, (ch + 2) as f64);
            if case == "cell-width" {
                scene.metrics.0 += 1.0;
            }
            scene.width = (scene.metrics.0 * 16.0).ceil() as i32;
            scene.height = (ch + 2) * 2;
            scene.matches = &matches;
            if case == "selection" {
                scene.selection = Some((0, 0));
            }
            if case == "cursor" {
                scene.frame.cursor_x = 1;
                scene.frame.cursor_visible = 1;
                scene.frame.cursor_style = 1;
            }
            let mut spans = vec![2..3, 4..6, 6..7, 8..11, 11..12, 15..16, 16..17];
            if matches!(case, "joined" | "background" | "cell-width" | "restored") {
                spans.insert(0, 0..2);
            } else {
                spans.splice(0..0, [0..1, 1..2]);
            }
            // Declare the expected text spans and colors explicitly. The reference
            // draws these layouts without interpreting selection or cursor metadata.
            let mut expected_cells = cells.clone();
            match case {
                "selection" | "search" => {
                    let index = if case == "selection" { 0 } else { 1 };
                    expected_cells[index].fg = theme.selection_foreground;
                    expected_cells[index].bg = theme.selection_background;
                }
                "cursor" => {
                    expected_cells[1].fg = theme.background;
                    expected_cells[1].bg = theme.cursor;
                }
                _ => {}
            }
            for span in &spans {
                let text: String = cells[span.clone()].iter().map(Cell::text).collect();
                for expected in &mut expected_cells[span.clone()] {
                    expected.text.fill(0);
                }
                expected_cells[span.start].text[..text.len()].copy_from_slice(text.as_bytes());
            }
            let (actual, _) = node(&mut renderer, &scene, &font, context);
            let expected = reference(
                &Scene {
                    cells: &expected_cells,
                    ..scene
                },
                &font,
                context,
            );
            assert_render_eq(
                cache_gpu,
                &actual,
                &expected,
                &scene,
                output,
                &format!("ligature-{case}-{scale}"),
            );
            if case == "background" {
                assert_ligature_layout(
                    gpu,
                    &actual,
                    &expected,
                    &scene,
                    &spans,
                    output,
                    &format!("ligature-gpu-background-{scale}"),
                );
            }
        }
    }
}

fn validate_cursor_lifecycle(output: &Path) {
    fn paint(area: &gtk::DrawingArea, rgb: [u8; 3]) {
        area.set_draw_func(move |_, cr, _, _| {
            cr.set_source_rgb(
                rgb[0] as f64 / 255.0,
                rgb[1] as f64 / 255.0,
                rgb[2] as f64 / 255.0,
            );
            cr.paint().unwrap();
        });
    }
    let mut serial = 0;
    let mut activate = |window: &gtk::Window, rgb: [u8; 3]| {
        window.present();
        serial += 1;
        let temporary = output.join("cursor-activation.tmp");
        std::fs::write(&temporary, json!({"serial":serial,"color":rgb}).to_string()).unwrap();
        std::fs::rename(temporary, output.join("cursor-activation.json")).unwrap();
        wait(|| {
            std::fs::read_to_string(output.join("cursor-activated"))
                .ok()
                .as_deref()
                == Some(&serial.to_string())
        });
    };
    use std::time::{Duration, Instant};
    #[track_caller]
    fn wait(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !predicate() {
            if Instant::now() >= deadline {
                fn dump(widget: &gtk::Widget) {
                    eprintln!(
                        "{} mapped={} visible={} allocation={}x{} focus={} active={:?}",
                        widget.type_().name(),
                        widget.is_mapped(),
                        widget.is_visible(),
                        widget.width(),
                        widget.height(),
                        widget.has_focus(),
                        widget.downcast_ref::<gtk::Window>().map(|w| w.is_active())
                    );
                    let mut child = widget.first_child();
                    while let Some(widget) = child {
                        dump(&widget);
                        child = widget.next_sibling();
                    }
                }
                for window in gtk::Window::list_toplevels() {
                    dump(&window);
                }
                panic!("Cursor lifecycle transition timed out");
            }
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let area = gtk::DrawingArea::builder()
        .focusable(true)
        .content_width(200)
        .content_height(50)
        .vexpand(true)
        .build();
    let click = gtk::GestureClick::new();
    let weak = area.downgrade();
    click.connect_pressed(move |_, _, _, _| {
        if let Some(area) = weak.upgrade() {
            area.grab_focus();
        }
    });
    area.add_controller(click);
    paint(&area, [60, 120, 180]);
    area.set_cursor_from_name(Some("text"));
    let settings = area.settings();
    let old = (
        settings.is_gtk_cursor_blink(),
        settings.gtk_cursor_blink_time(),
        settings.gtk_cursor_blink_timeout(),
    );
    settings.set_gtk_cursor_blink(true);
    settings.set_gtk_cursor_blink_time(200);
    settings.set_gtk_cursor_blink_timeout(10);
    let defaults = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = defaults.clone();
    let cursor = cursor::Cursor::new(&area, move |blink| observed.borrow_mut().push(blink));
    let mut frame = Frame {
        cols: 80,
        rows: 24,
        cursor_visible: 1,
        cursor_blink: 1,
        mouse_shape: mouse_shape::DEFAULT,
        ..Frame::default()
    };
    cursor.update(frame, false);
    assert_eq!(
        area.cursor().unwrap().name().as_deref(),
        Some("default"),
        "First viewport must apply explicit OSC 22 default"
    );
    assert!(!cursor.running());
    let other = gtk::DrawingArea::builder()
        .focusable(true)
        .content_width(200)
        .content_height(50)
        .vexpand(true)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&area);
    content.append(&other);
    let window = gtk::Window::builder()
        .child(&content)
        .default_width(200)
        .default_height(100)
        .build();
    area.grab_focus();
    activate(&window, [60, 120, 180]);
    wait(|| cursor.running());
    wait(|| !cursor.visible());
    // Text input must restart blinking even with identical terminal coordinates.
    cursor.restart();
    assert!(cursor.visible() && cursor.running());
    // Leave the original source pending while activity resets the deadline.
    // Holding the main loop also covers an overdue source after a scheduling
    // delay. Observe the first hidden phase instead of sampling a narrow window.
    cursor.restart();
    std::thread::sleep(Duration::from_millis(80));
    let pending = cursor::source_id(&cursor);
    let activity = Instant::now();
    cursor.restart();
    assert_eq!(cursor::source_id(&cursor), pending);
    wait(|| !cursor.visible());
    assert!(
        activity.elapsed() >= Duration::from_millis(100),
        "Blink hid before the full new half-cycle"
    );

    // Key input and its echoed cursor update reuse the same pending source.
    cursor.restart();
    let pending = cursor::source_id(&cursor);
    for index in 0..1000 {
        cursor.restart();
        frame.cursor_x = (index % 70) as u16;
        cursor.update(frame, false);
        assert_eq!(
            cursor::source_id(&cursor),
            pending,
            "Input replaced a pending source"
        );
    }

    // Colors, scrollback and pointer changes must not reset the blink phase.
    let timing = cursor::timing(&cursor);
    frame.background = [1, 2, 3];
    frame.foreground = [4, 5, 6];
    frame.cursor = [7, 8, 9];
    frame.scrollback.total = 100;
    frame.mouse_shape = mouse_shape::POINTER;
    cursor.update(frame, false);
    assert_eq!(cursor::source_id(&cursor), pending);
    assert_eq!(cursor::timing(&cursor), timing);
    assert_eq!(area.cursor().unwrap().name().as_deref(), Some("pointer"));
    cursor.set_link_hover(true);
    frame.mouse_shape = gmux_client::cells::mouse_shape::CROSSHAIR;
    cursor.update(frame, false);
    assert_eq!(area.cursor().unwrap().name().as_deref(), Some("pointer"));
    cursor.set_link_hover(false);
    assert_eq!(area.cursor().unwrap().name().as_deref(), Some("crosshair"));
    cursor.update(frame, false);
    assert_eq!(area.cursor().unwrap().name().as_deref(), Some("crosshair"));

    // Timing changes take effect with a source pending, and never send daemon
    // defaults while the effective enablement stays true. Check the scheduled
    // times directly so late callback delivery cannot fail an upper time bound.
    for cycle in [800, 200, 800] {
        let before = (Instant::now(), glib::monotonic_time());
        settings.set_gtk_cursor_blink_time(cycle);
        let after = (Instant::now(), glib::monotonic_time());
        let (deadline, ready_time) = cursor::timing(&cursor);
        let half_cycle = Duration::from_millis(cycle as u64 / 2);
        assert!(
            (before.0 + half_cycle..=after.0 + half_cycle).contains(&deadline),
            "Timing preference did not reset the blink deadline"
        );
        // The whole-millisecond GLib timeout rounds up by less than 1 ms.
        let micros = half_cycle.as_micros() as i64;
        assert!(
            (before.1 + micros..=after.1 + micros + 1000).contains(&ready_time),
            "Timing preference did not reschedule the pending source"
        );
        assert!(cursor.visible());
    }
    let lengthened_deadline = cursor::timing(&cursor).0;
    wait(|| !cursor.visible());
    assert!(Instant::now() >= lengthened_deadline);
    settings.set_gtk_cursor_blink_timeout(20);
    settings.set_gtk_cursor_blink_timeout(10);
    assert!(
        defaults.borrow().is_empty(),
        "Timing-only settings sent daemon defaults"
    );
    let timeout_spec = settings.find_property("gtk-cursor-blink-timeout").unwrap();
    let minimum = timeout_spec
        .downcast_ref::<glib::ParamSpecInt>()
        .unwrap()
        .minimum();
    std::fs::write(
        output.join("cursor-settings.json"),
        json!({"timeout_minimum": minimum}).to_string(),
    )
    .unwrap();
    if minimum <= 0 {
        settings.set_gtk_cursor_blink_timeout(0);
        assert!(!cursor.running());
        assert_eq!(*defaults.borrow(), [false]);
    }
    settings.set_gtk_cursor_blink(false);
    assert!(!cursor.running());
    settings.set_gtk_cursor_blink_time(400);
    assert_eq!(*defaults.borrow(), [false]);
    settings.set_gtk_cursor_blink_timeout(10);
    assert_eq!(*defaults.borrow(), [false]);
    settings.set_gtk_cursor_blink(true);
    assert_eq!(*defaults.borrow(), [false, true]);
    settings.set_gtk_cursor_blink_time(200);

    frame.cursor_blink = 0;
    cursor.update(frame, false);
    assert!(cursor.visible() && !cursor.running());
    frame.cursor_blink = 1;
    frame.cursor_visible = 0;
    cursor.update(frame, false);
    assert!(!cursor.running());
    frame.cursor_visible = 1;
    cursor.update(frame, false);
    assert!(cursor.running());
    other.grab_focus();
    wait(|| !cursor.running());
    assert!(cursor.visible());
    area.grab_focus();
    wait(|| cursor.running());
    window.set_visible(false);
    assert!(!cursor.running());
    area.grab_focus();
    activate(&window, [60, 120, 180]);
    wait(|| cursor.running());
    let cover = gtk::Window::builder()
        .default_width(100)
        .default_height(100)
        .build();
    let cover_area = gtk::DrawingArea::new();
    paint(&cover_area, [180, 100, 60]);
    cover.set_child(Some(&cover_area));
    activate(&cover, [180, 100, 60]);
    wait(|| !window.is_active());
    assert!(
        !cursor.running(),
        "An inactive window must stop its blink source"
    );
    cover.destroy();
    area.grab_focus();
    activate(&window, [60, 120, 180]);
    wait(|| cursor.running());
    settings.set_gtk_cursor_blink(false);
    assert!(!cursor.running() && cursor.visible());
    settings.set_gtk_cursor_blink(true);
    assert!(cursor.running());
    assert_eq!(*defaults.borrow(), [false, true, false, true]);

    settings.set_gtk_cursor_blink_timeout(1);
    assert_eq!(*defaults.borrow(), [false, true, false, true]);
    wait(|| !cursor.running());
    assert!(cursor.visible());
    cursor.restart();
    assert!(cursor.running());
    cursor.update(frame, true);
    assert!(!cursor.running());
    cursor.update(frame, false);
    assert!(cursor.running());
    frame.mouse_shape = 255;
    cursor.update(frame, false);
    assert_eq!(area.cursor().unwrap().name().as_deref(), Some("text"));
    let weak = std::rc::Rc::downgrade(&cursor);
    drop(cursor);
    assert!(
        weak.upgrade().is_none(),
        "Blink source must not retain its owner"
    );
    window.destroy();
    settings.set_gtk_cursor_blink(old.0);
    settings.set_gtk_cursor_blink_time(old.1);
    settings.set_gtk_cursor_blink_timeout(old.2);
}

fn validate_decoration_patterns(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
) {
    let theme = Theme::adwaita(true);
    let grid = |decoration, flags| {
        vec![
            Cell {
                fg: [255; 3],
                bg: [0; 3],
                decoration,
                flags,
                ..Cell::default()
            };
            6
        ]
    };
    let check = |cells: &[Cell]| {
        let mut s = scene(cells, theme);
        s.frame.cols = 6;
        s.frame.rows = 1;
        s.frame.background = [0; 3];
        s.width = 60;
        s.height = 24;
        let texture = render(gpu, &node(&mut Renderer::new(), &s, font, context).0, &s);
        pixels(&texture)
    };
    for style in [
        underline::SINGLE,
        underline::DOUBLE,
        underline::DOTTED,
        underline::DASHED,
        decoration::OVERLINE,
    ] {
        let data = check(&grid(style, 0));
        for y in 0..24 {
            for x in 0..60 {
                let expected = match style {
                    underline::SINGLE => y == 22,
                    underline::DOUBLE => y == 20 || y == 22,
                    underline::DOTTED => y == 22 && x % 2 == 0,
                    underline::DASHED => y == 22 && x % 6 < 3,
                    _ => y == 0, // Overline is at the top, with no underline.
                };
                assert_eq!(
                    data[(y * 60 + x) * 4],
                    if expected { 255 } else { 0 },
                    "Decoration {style}, {x},{y}"
                );
            }
        }
    }
    let data = check(&grid(underline::CURLY, 0));
    let mut extents = Vec::new();
    for x in 1..59 {
        let ink: Vec<_> = (0..24).filter(|&y| data[(y * 60 + x) * 4] > 20).collect();
        assert!(!ink.is_empty(), "Curly underline has a gap at column {x}");
        assert!(
            ink.iter().all(|&y| (19..24).contains(&y)),
            "Wave must stay in the underline band"
        );
        extents.push((*ink.first().unwrap(), *ink.last().unwrap()));
    }
    assert!(
        extents.windows(2).any(|pair| pair[0] != pair[1]),
        "Curly underline must change vertical position"
    );
    assert!(extents.iter().any(|&(top, _)| top == 19));
    assert!(extents.iter().any(|&(_, bottom)| bottom == 23));
    let faint = check(&grid(underline::SINGLE | decoration::OVERLINE, 4));
    assert!((127..=128).contains(&faint[22 * 60 * 4]));
    assert!((127..=128).contains(&faint[0]));
    let invisible = check(&grid(underline::CURLY | decoration::OVERLINE, 32));
    assert!(
        invisible
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[..3] == [0; 3])
    );
}

// Compare batched patterns with independent, individually snapped rectangles.
// Blank cells isolate coverage, color, opacity, phase and clipped run edges.
fn validate_batched_patterns(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
    output: &Path,
) {
    let theme = Theme::adwaita(true);
    let mut cells = vec![cell("", theme, 0); 12 * 4];
    for (index, cell) in cells.iter_mut().enumerate() {
        let x = index % 12;
        cell.decoration = if index / 12 % 2 == 0 {
            underline::DOTTED
        } else {
            underline::DASHED
        };
        if (2..7).contains(&x) {
            cell.decoration |= decoration::UNDERLINE_COLOR;
            cell.underline_color = if x == 6 { [0; 3] } else { [240, 60, 180] };
        }
        if (4..10).contains(&x) {
            cell.flags |= 4;
        }
        if x == 10 {
            cell.flags |= 64;
        }
        if x == 11 {
            cell.flags |= 32;
        }
    }
    for (scale, origin) in [
        (1.0, (0.25, 0.5)),
        (1.25, (0.25, 0.5)),
        (1.5, (0.2, 0.4)),
        (2.0, (0.25, 0.5)),
        (1.375, (0.2, 0.4)),
        (1.3, (0.25, 0.5)), // Non-binary scale retains individual rectangles.
        (1.0, (-0.5, 0.5)), // Rounding across zero cannot use a periodic tile.
        (1.0, (0.599609375, 0.0)), // Saved glyph ink bounds require this translation when autosized.
    ] {
        let mut s = scene(&cells, theme);
        s.frame.cols = 12;
        s.frame.rows = 4;
        s.metrics = (17.3, 21.3);
        s.width = 208;
        s.height = 86;
        s.scale = scale;
        s.origin = origin;
        s.selection = Some((1, 19));
        let expected = gtk::Snapshot::new();
        expected.append_color(
            &rgba(s.frame.background, 1.0),
            &graphene::Rect::new(0.0, 0.0, 208.0, 86.0),
        );
        // Draw selection backgrounds independently of decoration color.
        for y in 0..4 {
            let row = gtk::Snapshot::new();
            let row_y = y as f64 * s.metrics.1;
            let snap_x = |x: f64| ((x + s.origin.0) * scale).round() / scale - s.origin.0;
            let snap_y =
                |y: f64| ((y + row_y + s.origin.1) * scale).round() / scale - row_y - s.origin.1;
            for x in 0..12 {
                let index = y * 12 + x;
                let selected = (1..=19).contains(&index);
                if selected {
                    row.append_color(
                        &rgba(theme.selection_background, 1.0),
                        &graphene::Rect::new(
                            snap_x(x as f64 * 17.3) as f32,
                            snap_y(0.0) as f32,
                            (snap_x((x + 1) as f64 * 17.3) - snap_x(x as f64 * 17.3)) as f32,
                            (snap_y(21.3) - snap_y(0.0)) as f32,
                        ),
                    );
                }
            }
            // Clip at appearance boundaries, not every cell edge. Splitting a
            // single dot into adjacent rectangles adds an unrelated blend seam.
            let mut runs: Vec<(usize, usize, [u8; 3], f32)> = Vec::new();
            for x in 0..12 {
                let index = y * 12 + x;
                let cell = &cells[index];
                if cell.flags & 32 != 0 {
                    continue;
                }
                let color = if cell.has_underline_color() {
                    cell.underline_color
                } else if (1..=19).contains(&index) {
                    theme.selection_foreground
                } else {
                    cell.fg
                };
                let alpha = if cell.flags & 4 != 0 { 0.5 } else { 1.0 };
                if let Some((_, end, previous_color, previous_alpha)) = runs.last_mut()
                    && *end == x
                    && *previous_color == color
                    && *previous_alpha == alpha
                {
                    *end += 1;
                } else {
                    runs.push((x, x + 1, color, alpha));
                }
            }
            for (start, end, color, alpha) in runs {
                let unit = if y % 2 == 0 { 1.0 } else { 3.0 };
                let start = start as f64 * 17.3;
                let end = end as f64 * 17.3;
                let mut left = (start / (2.0 * unit)).floor() * 2.0 * unit;
                while left < end {
                    let a = left.max(start);
                    let b = (left + unit).min(end);
                    if b > a {
                        row.append_color(
                            &rgba(color, alpha),
                            &graphene::Rect::new(
                                snap_x(a) as f32,
                                snap_y(19.3) as f32,
                                (snap_x(b) - snap_x(a)) as f32,
                                (snap_y(20.3) - snap_y(19.3)) as f32,
                            ),
                        );
                    }
                    left += 2.0 * unit;
                }
            }
            expected.save();
            expected.translate(&graphene::Point::new(0.0, row_y as f32));
            expected.append_node(row.to_node().unwrap());
            expected.restore();
        }
        let actual = node(&mut Renderer::new(), &s, font, context).0;
        let expected = expected.to_node().unwrap();
        assert_render_eq(
            gpu,
            &actual,
            &expected,
            &s,
            output,
            &format!("batched-patterns-{scale}"),
        );
    }
}

fn validate_link_hover(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
    output: &Path,
) {
    let theme = Theme::adwaita(true);
    let mut cells = fixture(theme);
    for (index, cell) in cells.iter_mut().enumerate() {
        cell.decoration = (index % 6) as u8 | decoration::OVERLINE;
        if index % 3 != 0 {
            cell.decoration |= decoration::UNDERLINE_COLOR;
            cell.underline_color = if index % 3 == 1 {
                [240, 77, 151]
            } else {
                [0; 3]
            };
        }
        // Decorations include linked blanks, concealed cells and faint text.
        cell.flags = [0, 4, 32][index / 6 % 3];
    }
    cells[38] = cell("界", theme, 0);
    cells[39] = cell("", theme, 64);
    for scale in [1.0, 1.25, 1.5, 2.0] {
        let mut renderer = Renderer::new();
        let mut scene = scene(&cells, theme);
        scene.scale = scale;
        scene.origin = (0.25, 0.5);
        scene.metrics.1 = 21.3;
        scene.selection = Some((36, 44));
        scene.frame.cursor_visible = 1;
        scene.frame.cursor_style = 1;
        scene.frame.cursor_x = 4;
        let (_, warm) = node(&mut renderer, &scene, font, context);
        let mut previous = None;
        // Enter, stay within one span, cross wrapped rows, change span, leave.
        for (step, range) in [
            Some((1, 22)),
            Some((1, 22)),
            Some((35, 47)),
            Some((95, 100)),
            None,
        ]
        .into_iter()
        .enumerate()
        {
            scene.hovered_link = range;
            let (actual, stats) = node(&mut renderer, &scene, font, context);
            assert_eq!(stats.rebuilt_rows, 0, "Hover must retain glyph rows");
            assert_eq!(stats.text_nodes, 0);
            assert_eq!(stats.cached_shapes, warm.cached_shapes);
            assert_eq!(stats.cached_shape_bytes, warm.cached_shape_bytes);
            let rows = |span: Option<(usize, usize)>, y: usize| {
                span.and_then(|(start, end)| {
                    (start < (y + 1) * 40 && end > y * 40).then(|| {
                        (
                            start.saturating_sub(y * 40),
                            end.saturating_sub(y * 40).min(40),
                        )
                    })
                })
            };
            assert_eq!(
                stats.background_rows,
                (0..8)
                    .filter(|&y| rows(previous, y) != rows(range, y))
                    .count()
            );
            let mut expected_cells = cells.clone();
            if let Some((start, end)) = range {
                // Declare the pinned Ghostty mapping for NONE/SINGLE/DOUBLE/
                // CURLY/DOTTED/DASHED; normal SGR rendering is the pixel oracle.
                for cell in &mut expected_cells[start..end] {
                    cell.decoration = (cell.decoration & !decoration::UNDERLINE_MASK)
                        | [
                            underline::SINGLE,
                            underline::DOUBLE,
                            underline::SINGLE,
                            underline::SINGLE,
                            underline::SINGLE,
                            underline::SINGLE,
                        ][cell.underline() as usize];
                }
            }
            let expected_scene = Scene {
                cells: &expected_cells,
                hovered_link: None,
                ..scene
            };
            let expected = node(&mut Renderer::new(), &expected_scene, font, context).0;
            assert_render_eq(
                gpu,
                &actual,
                &expected,
                &scene,
                output,
                &format!("link-hover-{scale}-{step}"),
            );
            previous = range;
        }
    }
}

fn validate_decorations(gpu: &gsk::Renderer, context: &pango::Context, output: &Path) {
    let theme = Theme::adwaita(true);
    fn configured(cells: &[Cell], theme: Theme, scale: f64) -> Scene<'_> {
        let mut s = scene(cells, theme);
        s.scale = scale;
        s.origin = (0.25, 0.5);
        s.metrics = (12.0, 20.0);
        s.width = 480;
        s
    }
    let font = pango::FontDescription::from_string("Monospace 16");
    for scale in [1.0, 1.25, 1.5, 2.0] {
        let mut glyphs = vec![cell(" ", theme, 0); 40 * 8];
        for (index, ch) in "gypqjgypqj".chars().enumerate() {
            glyphs[index] = Cell {
                fg: [255; 3],
                ..cell(&ch.to_string(), theme, 0)
            };
        }
        let s = configured(&glyphs, theme, scale);
        let plain = render(gpu, &node(&mut Renderer::new(), &s, &font, context).0, &s);
        let plain_pixels = pixels(&plain);
        for style in underline::STYLES {
            let mut decorated = glyphs.clone();
            for c in &mut decorated[..10] {
                c.decoration = style | decoration::UNDERLINE_COLOR;
                c.underline_color = [255, 0, 0];
            }
            let s = configured(&decorated, theme, scale);
            let actual_texture = render(gpu, &node(&mut Renderer::new(), &s, &font, context).0, &s);
            let actual_pixels = pixels(&actual_texture);
            let mut blanks = decorated.clone();
            for c in &mut blanks {
                c.text = [0; 64];
            }
            let blank_scene = configured(&blanks, theme, scale);
            let mask = render(
                gpu,
                &node(&mut Renderer::new(), &blank_scene, &font, context).0,
                &blank_scene,
            );
            let mask_pixels = pixels(&mask);
            let mut intersections = 0;
            for ((glyph, line), actual) in plain_pixels
                .as_chunks::<4>()
                .0
                .iter()
                .zip(mask_pixels.as_chunks::<4>().0)
                .zip(actual_pixels.as_chunks::<4>().0)
            {
                // Opaque descender ink must cover the contrasting underline.
                // The separate blank scene proves these are actual intersections.
                if *glyph == [255; 4] && line[2] > 180 && line[1] < 80 {
                    intersections += 1;
                    if actual != glyph {
                        actual_texture
                            .save_to_png(output.join(format!("descender-{style}-{scale}.png")))
                            .unwrap();
                    }
                    assert_eq!(
                        actual, glyph,
                        "Underline {style} covered descender at scale {scale}"
                    );
                }
            }
            assert!(
                intersections > 0,
                "Fixture must overlap underline {style} and descenders at {scale}"
            );
            let s = configured(&decorated, theme, scale);
            let mut renderer = Renderer::new();
            node(&mut renderer, &s, &font, context);
            let (_, stats) = node(&mut renderer, &s, &font, context);
            assert_eq!(stats.rebuilt_rows, 0);
            let mut changed = decorated.clone();
            changed[0].underline_color = [0, 255, 0];
            let s = configured(&changed, theme, scale);
            let (cached, stats) = node(&mut renderer, &s, &font, context);
            assert_eq!(stats.rebuilt_rows, 1);
            let fresh = node(&mut Renderer::new(), &s, &font, context).0;
            assert_render_eq(gpu, &cached, &fresh, &s, output, "decoration-color-update");
        }
    }
}

fn validate_cache(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
    output: &Path,
) {
    let theme = Theme::adwaita(true);
    let cells = fixture(theme);
    let mut highlights = vec![0; cells.len()];
    highlights[3] = 2;
    let mut renderer = Renderer::new();
    let mut font = font.clone();
    let empty_highlights = vec![0; cells.len()];
    let mut scene = scene(&cells, theme);
    scene.matches = &empty_highlights;
    for (step, rows) in [
        ("initial", 8),
        ("unchanged", 0),
        ("search", 1),
        ("selection", 1),
        ("block", 1),
        ("move", 2),
        ("cursor-color", 1),
        ("block-blink-off", 1),
        ("block-blink-on", 1),
        ("unfocused", 1),
        ("bar", 0),
        ("bar-blink-off", 0),
        ("bar-blink-on", 0),
        ("underline", 0),
        ("underline-blink-off", 0),
        ("underline-blink-on", 0),
        ("hidden", 0),
        ("font", 8),
        ("context", 8),
        ("scale", 8),
        ("origin", 8),
        ("theme", 8),
        ("smaller", 0),
        ("larger", 4),
    ] {
        match step {
            "search" => {
                scene.matches = &highlights;
            }
            "selection" => scene.selection = Some((42, 45)),
            "block" => {
                scene.frame.cursor_visible = 1;
                scene.frame.cursor_style = 1;
                scene.frame.cursor_y = 2;
            }
            "move" => scene.frame.cursor_y = 3,
            "cursor-color" => scene.frame.cursor = [255, 0, 0],
            "block-blink-off" | "bar-blink-off" | "underline-blink-off" => {
                scene.frame.cursor_visible = 0
            }
            "block-blink-on" | "bar-blink-on" | "underline-blink-on" => {
                scene.frame.cursor_visible = 1
            }
            "unfocused" => scene.focused = false,
            "bar" => {
                scene.focused = true;
                scene.frame.cursor_style = 0;
            }
            "underline" => scene.frame.cursor_style = 2,
            "hidden" => scene.frame.cursor_visible = 0,
            "font" => font.set_size(16 * pango::SCALE),
            "context" => context.changed(),
            "scale" => scene.scale = 1.5,
            "origin" => scene.origin = (0.25, 0.5),
            "theme" => scene.frame.background = [35, 35, 35],
            "smaller" => {
                scene.frame.rows = 4;
                scene.cells = &cells[..160];
            }
            "larger" => {
                scene.frame.rows = 8;
                scene.cells = &cells;
            }
            _ => {}
        }
        let (cached, stats) = node(&mut renderer, &scene, &font, context);
        assert_eq!(stats.rebuilt_rows, rows, "Cache invalidation after {step}");
        let fresh = node(&mut Renderer::new(), &scene, &font, context).0;
        assert_render_eq(
            gpu,
            &cached,
            &fresh,
            &scene,
            output,
            &format!("cache-{step}"),
        );
    }
    // Output changing one row must not discard the other rows.
    let mut changed = cells.clone();
    changed[43] = cell("Z", theme, 0);
    scene.cells = &changed;
    let (cached, stats) = node(&mut renderer, &scene, &font, context);
    assert_eq!(stats.rebuilt_rows, 1);
    let fresh = node(&mut Renderer::new(), &scene, &font, context).0;
    assert_render_eq(gpu, &cached, &fresh, &scene, output, "cache-output");
    assert_eq!(
        node(&mut renderer, &scene, &font, context).1.rebuilt_rows,
        0
    );

    let mut cells = vec![cell(" ", theme, 0); 40 * 8];
    for iteration in 0..16 {
        for (index, target) in cells.iter_mut().enumerate() {
            *target = cell(
                &char::from_u32(0x4e00 + (iteration * 320 + index) as u32)
                    .unwrap()
                    .to_string(),
                theme,
                0,
            );
        }
        let (_, stats) = node(&mut renderer, &self::scene(&cells, theme), &font, context);
        assert!(
            stats.cached_shapes <= 4096 && stats.cached_shape_bytes <= 1024 * 1024,
            "Shape cache exceeded its limits"
        );
        if iteration > 12 {
            assert_eq!(stats.cached_shapes, 4096);
        }
    }
    // Long, unique ASCII runs must reach the byte budget before the entry limit.
    renderer.clear();
    let mut evicted_cells = Vec::new();
    for iteration in 0..160 {
        for row in 0..8 {
            let mut word = format!("{:08X}", iteration * 8 + row);
            word.push_str(&"X".repeat(32));
            for (target, ch) in cells[row * 40..(row + 1) * 40].iter_mut().zip(word.chars()) {
                *target = cell(&ch.to_string(), theme, 0);
            }
        }
        let (_, stats) = node(&mut renderer, &self::scene(&cells, theme), &font, context);
        assert!(
            stats.cached_shape_bytes <= 1024 * 1024,
            "Long runs exceeded the shape cache budget"
        );
        if iteration == 0 {
            evicted_cells = cells.clone();
        }
        if iteration == 159 {
            assert!(
                stats.cached_shapes < 1280,
                "Byte budget did not evict long runs"
            );
        }
    }
    let scene = self::scene(&evicted_cells, theme);
    let cached = node(&mut renderer, &scene, &font, context).0;
    let fresh = node(&mut Renderer::new(), &scene, &font, context).0;
    assert_render_eq(gpu, &cached, &fresh, &scene, output, "cache-evicted-runs");

    // A single run can exceed the entire cache budget. It must still produce
    // every glyph without being retained in the cache.
    renderer.clear();
    let oversized = vec![cell("X", theme, 0); u16::MAX as usize];
    let mut oversized_scene = self::scene(&oversized, theme);
    oversized_scene.frame.cols = u16::MAX;
    oversized_scene.frame.rows = 1;
    let (large, stats) = node(&mut renderer, &oversized_scene, &font, context);
    fn glyphs(node: &gsk::RenderNode) -> usize {
        node.downcast_ref::<gsk::TextNode>()
            .map_or(0, |text| text.num_glyphs() as usize)
            + node.children().iter().map(glyphs).sum::<usize>()
    }
    assert_eq!(glyphs(&large), oversized.len(), "Oversized run lost text");
    assert_eq!((stats.cached_shapes, stats.cached_shape_bytes), (0, 0));
}

fn validate_scroll_cache(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
    output: &Path,
) {
    let theme = Theme::adwaita(true);
    for scale in [1.0, 1.25, 1.5, 2.0] {
        for origin in [(0.0, 0.0), (0.25, 0.5)] {
            let mut renderer = Renderer::new();
            let mut cells = fixture(theme);
            let mut highlights = vec![0; cells.len()];
            for (step, delta) in [0_isize, 1, -1, 3, -2, 7, -7].into_iter().enumerate() {
                if delta > 0 {
                    cells.rotate_left(delta as usize * 40);
                } else if delta < 0 {
                    cells.rotate_right(delta.unsigned_abs() * 40);
                }
                // Incoming rows contain new content rather than wrapping old rows.
                for row in 0..delta.unsigned_abs() {
                    let y = if delta > 0 {
                        8 - delta as usize + row
                    } else {
                        row
                    };
                    cells[y * 40] = Cell {
                        decoration: underline::SINGLE,
                        ..cell(&format!("{step}"), theme, 16)
                    };
                    cells[y * 40].bg = [35 + step as u8, 45, 55];
                }
                let mut scene = scene(&cells, theme);
                scene.metrics.1 = 21.0;
                scene.height = 168;
                scene.scale = scale;
                scene.origin = origin;
                if step >= 2 {
                    scene.selection = Some((42, 85));
                    highlights.fill(0);
                    highlights[step * 40 + 3] = 1;
                    highlights[step * 40 + 4] = 2;
                    scene.matches = &highlights;
                    scene.frame.cursor_visible = 1;
                    scene.frame.cursor_style = 1;
                    scene.frame.cursor_x = 6;
                    scene.frame.cursor_y = step as u16;
                }
                let (cached, stats) = node(&mut renderer, &scene, font, context);
                if step == 1 && (scale == 1.0 || scale == 2.0) {
                    assert_eq!(stats.rebuilt_rows, 1, "Scrolling should reuse seven rows");
                }
                let fresh = node(&mut Renderer::new(), &scene, font, context).0;
                assert_render_eq(
                    gpu,
                    &cached,
                    &fresh,
                    &scene,
                    output,
                    &format!("scroll-{step}-{scale}-{}-{}", origin.0, origin.1),
                );
            }
        }
    }
    // Matching one edge row is insufficient evidence that the viewport scrolled.
    for (target, source) in [(0, 1), (7, 6)] {
        let cells = fixture(theme);
        let mut renderer = Renderer::new();
        node(&mut renderer, &scene(&cells, theme), font, context);
        let mut edited = cells.clone();
        edited[target * 40..(target + 1) * 40]
            .copy_from_slice(&cells[source * 40..(source + 1) * 40]);
        let scene = scene(&edited, theme);
        let (cached, stats) = node(&mut renderer, &scene, font, context);
        assert_eq!(
            stats.rebuilt_rows, 1,
            "An edge edit must preserve other cached rows"
        );
        let fresh = node(&mut Renderer::new(), &scene, font, context).0;
        assert_render_eq(
            gpu,
            &cached,
            &fresh,
            &scene,
            output,
            &format!("edge-{target}"),
        );
    }
}

fn validate_geometry(gpu: &gsk::Renderer, font: &pango::FontDescription, context: &pango::Context) {
    let theme = Theme::adwaita(true);
    for scale in [1.0, 1.25, 1.5, 2.0] {
        for origin in [(0.0, 0.0), (0.25, 0.5)] {
            let cells = vec![
                Cell {
                    fg: [255; 3],
                    bg: [0; 3],
                    ..cell("█", theme, 0)
                };
                4
            ];
            let mut scene = scene(&cells, theme);
            scene.frame.cols = 2;
            scene.frame.rows = 2;
            scene.metrics = (9.0, 21.0);
            scene.width = 18;
            scene.height = 42;
            scene.frame.background = [0; 3];
            scene.scale = scale;
            scene.origin = origin;
            let texture = render(
                gpu,
                &node(&mut Renderer::new(), &scene, font, context).0,
                &scene,
            );
            let pixels = pixels(&texture);
            for y in 2..texture.height() as usize - 2 {
                for x in 2..texture.width() as usize - 2 {
                    assert_eq!(
                        &pixels[(y * texture.width() as usize + x) * 4..][..4],
                        &[255; 4],
                        "Block seam at {scale} scale, {origin:?}, {x},{y}"
                    );
                }
            }
        }
    }

    let cells = vec![cell(" ", theme, 0); 320];
    let mut scene = scene(&cells, theme);
    scene.metrics = (10.0, 24.0);
    scene.selection = Some((2, 5));
    let mut highlights = vec![0; 320];
    highlights[42] = 1;
    highlights[43] = 2;
    scene.matches = &highlights;
    let texture = render(
        gpu,
        &node(&mut Renderer::new(), &scene, font, context).0,
        &scene,
    );
    let data = pixels(&texture);
    let sample = |x, y| &data[(y * texture.width() as usize + x) * 4..][..4];
    // Download is native BGRA on this supported Linux target.
    let bgra = |rgb: [u8; 3]| [rgb[2], rgb[1], rgb[0], 255];
    assert_eq!(sample(10, 12), bgra(theme.background));
    assert_eq!(sample(25, 12), bgra(theme.selection_background));
    assert_eq!(sample(35, 36), bgra(theme.selection_background));
    assert_ne!(sample(25, 36), bgra(theme.background));

    // Decorations on blank cells and spacer tails must be visible; concealed
    // cells suppress text and decorations while retaining their background.
    let mut cells = cells.clone();
    for target in &mut cells[80..120] {
        *target = Cell {
            decoration: underline::SINGLE,
            ..cell(" ", theme, 16)
        };
    }
    cells[84].flags |= 64;
    cells[85] = Cell {
        decoration: underline::SINGLE,
        ..cell("H", theme, 32 | 16)
    };
    cells[86] = cell("界", theme, 0);
    cells[87] = cell("", theme, 64);
    scene.cells = &cells;
    scene.selection = None;
    scene.matches = &[];
    scene.frame.cursor_visible = 1;
    scene.frame.cursor_style = 1;
    scene.frame.cursor_x = 6;
    scene.frame.cursor_y = 2;
    let texture = render(
        gpu,
        &node(&mut Renderer::new(), &scene, font, context).0,
        &scene,
    );
    let data = pixels(&texture);
    let sample = |x, y| &data[(y * texture.width() as usize + x) * 4..][..4];
    assert_eq!(
        sample(45, 70),
        bgra(theme.foreground),
        "Wide spacer underline missing"
    );
    assert_eq!(
        sample(45, 60),
        bgra(theme.foreground),
        "Wide spacer strike missing"
    );
    assert_eq!(
        sample(55, 70),
        bgra(theme.background),
        "Concealed decoration visible"
    );
    assert_eq!(
        sample(55, 60),
        bgra(theme.background),
        "Concealed glyph visible"
    );
    assert_eq!(
        sample(75, 50),
        bgra(theme.cursor),
        "Wide block cursor must fill the spacer"
    );

    // A whole solid background should produce a row node, not a node per cell.
    let mut backgrounds = cells.clone();
    for target in &mut backgrounds {
        *target = Cell {
            bg: [0, 0, 255],
            ..cell(" ", theme, 0)
        };
    }
    scene.cells = &backgrounds;
    scene.frame.cursor_visible = 0;
    let (node, _) = node(&mut Renderer::new(), &scene, font, context);
    fn colors(node: &gsk::RenderNode) -> usize {
        usize::from(node.node_type() == gsk::RenderNodeType::ColorNode)
            + node.children().iter().map(colors).sum::<usize>()
    }
    assert_eq!(
        colors(&node),
        9,
        "Adjacent backgrounds should share color nodes"
    );
    scene.frame.cursor_visible = 1;
    scene.frame.cursor_x = u16::MAX;
    scene.frame.cursor_y = u16::MAX;
    // A scrolled viewport can have no valid cursor position. Hidden cursor
    // metadata must never be used to index cells or construct cursor geometry.
    let (hidden, _) = self::node(&mut Renderer::new(), &scene, font, context);
    assert_eq!(colors(&hidden), 9);
}

fn main() -> anyhow::Result<()> {
    let options = Options::parse();
    std::fs::create_dir_all(&options.output)?;
    gtk::init()?;
    if options.cursor_lifecycle {
        validate_cursor_lifecycle(&options.output);
    }

    let window = gtk::Window::new();
    window.present();
    let gpu = window.renderer().unwrap();
    let context = window.pango_context();
    let cairo = OffscreenRenderer(gsk::CairoRenderer::new().upcast());
    cairo.0.realize_for_display(&WidgetExt::display(&window))?;
    let cache_gpu = match options.cache_renderer {
        CacheBackend::Cairo => &cairo.0,
        CacheBackend::Selected => &gpu,
    };
    let mut cases = Vec::new();
    for dark in [true, false] {
        for font_name in [
            "Monospace 9",
            "Monospace 12",
            "Monospace Bold Italic 16",
            "Monospace 14px",
        ] {
            let font = pango::FontDescription::from_string(font_name);
            let layout = pango::Layout::new(&context);
            layout.set_font_description(Some(&font));
            layout.set_text("M");
            let (cw, ch) = layout.pixel_size();
            let theme = Theme::adwaita(dark);
            let cells = fixture(theme);
            let mut scene = scene(&cells, theme);
            scene.metrics = (cw as f64, (ch + 2) as f64);
            scene.width = cw * 40;
            scene.height = (ch + 2) * 8;
            for scale in [1.0, 1.5] {
                scene.scale = scale;
                scene.origin = (0.25, 0.5);
                let (node, _) = node(&mut Renderer::new(), &scene, &font, &context);
                assert_direct_text(&node);
                let expected = render(&gpu, &reference(&scene, &font, &context), &scene);
                let actual = render(&gpu, &node, &scene);
                let name = format!(
                    "{}-{}-{scale}",
                    if dark { "dark" } else { "light" },
                    font_name.replace(' ', "-")
                );
                actual.save_to_png(options.output.join(format!("{name}-gsk.png")))?;
                expected.save_to_png(options.output.join(format!("{name}-reference.png")))?;
                let expected_pixels = pixels(&expected);
                let actual_pixels = pixels(&actual);
                let width = actual.width() as usize;
                // Compare each row, so an omitted script or incorrect baseline
                // cannot disappear into a mostly blank image's overall average.
                for row in 0..8 {
                    let top = (row as f64 * scene.metrics.1 * scale).ceil() as usize;
                    let bottom = ((row + 1) as f64 * scene.metrics.1 * scale).floor() as usize;
                    let start = top * width * 4;
                    let end = bottom * width * 4;
                    let delta = expected_pixels[start..end]
                        .iter()
                        .zip(&actual_pixels[start..end])
                        .map(|(&a, &b)| (a as f64 - b as f64).abs())
                        .sum::<f64>()
                        / (end - start) as f64;
                    assert!(delta < 1.0, "Glyph mismatch in {name}, row {row}: {delta}");
                }
                cases.push(name);
            }
        }
    }
    let font = pango::FontDescription::from_string("Monospace 12");
    validate_overlapping_fallbacks(&gpu, &font, &context);
    validate_ligatures(&gpu, cache_gpu, &context, &options.output);
    validate_cache(cache_gpu, &font, &context, &options.output);
    validate_scroll_cache(cache_gpu, &font, &context, &options.output);
    validate_geometry(&gpu, &font, &context);
    validate_decorations(cache_gpu, &context, &options.output);
    validate_decoration_patterns(cache_gpu, &font, &context);
    validate_batched_patterns(cache_gpu, &font, &context, &options.output);
    validate_batched_patterns(&gpu, &font, &context, &options.output);
    validate_link_hover(cache_gpu, &font, &context, &options.output);
    std::fs::write(
        options.output.join("checks.json"),
        serde_json::to_string_pretty(&json!({
            "gtk_renderer": gpu.type_().name(), "cache_renderer": cache_gpu.type_().name(),
            "cursor_lifecycle": options.cursor_lifecycle,
            "glyph_cases": cases,
            "checks": ["exact cache pixels", "independent layout agreement", "row invalidation", "scroll cache reuse", "bounded shape cache",
                       "font and context changes", "fractional blocks", "selection and search", "link hover SGR pixels and glyph cache retention",
                       "distinct underline patterns and overline geometry", "batched pattern rectangle equivalence", "cursor blink row reuse", "colored underlines beneath descenders", "decoration color invalidation", "concealed text", "blank and wide decorations", "wide cursor", "background batching", "overlapping fallback order",
                       "ligatures and contextual substitutions", "ligature style and cell boundaries", "GPU ligature layouts"]
        }))?,
    )?;
    println!(
        "PASS: {}: {} glyph cases and geometry checks; exact cache pixels on {}",
        gpu.type_().name(),
        cases.len(),
        cache_gpu.type_().name()
    );
    window.close();
    gpu.unrealize();
    Ok(())
}
