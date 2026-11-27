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
mod text_blink {
    include!("../src/terminal/text_blink.rs");

    pub(super) fn source_id(blink: &TextBlink) -> Option<u32> {
        blink.source.borrow().as_ref().map(glib::SourceId::as_raw)
    }
    pub(super) fn ready_time(blink: &TextBlink) -> i64 {
        let source = blink.source.borrow();
        glib::MainContext::default()
            .find_source_by_id(source.as_ref().unwrap())
            .unwrap()
            .ready_time()
    }
}
#[allow(dead_code)]
#[path = "../src/bidi.rs"]
mod bidi;
#[allow(dead_code)]
#[path = "../src/render.rs"]
mod render;
#[allow(dead_code)]
#[path = "../src/renderer.rs"]
mod renderer;

use clap::{Parser, ValueEnum};
use gmux_protocol::{
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
    cell.set_text(text).unwrap();
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
        graphics: None,
        bidi: None,
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
        text_blink_visible: true,
    }
}

fn fixture(theme: Theme) -> Vec<Cell> {
    let mut cells = vec![cell(" ", theme, 0); 40 * 8];
    let long = format!("a{}", "\u{301}".repeat(64));
    let clusters = [
        long.as_str(),
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
        if index % 3 == 0 {
            target.flags |= gmux_protocol::cells::flags::BLINK;
        }
    }
    let mut scene = scene(&cells, theme);
    // Force ink across neighboring cells to exercise paint-order checks, with
    // alternating fallback fonts, colors, italics, and node opacity.
    scene.metrics.0 = 3.0;
    scene.width = 120;
    for shown in [true, false] {
        scene.text_blink_visible = shown;
        let actual = node(&mut Renderer::new(), &scene, font, context).0;
        let mut expected_cells = cells.clone();
        for cell in &mut expected_cells {
            if cell.blink() && !shown {
                cell.flags |= 32;
            }
        }
        let expected = reference(
            &Scene {
                cells: &expected_cells,
                ..scene
            },
            font,
            context,
        );
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
            "Overlapping blink/fallback glyphs changed paint order: {delta}"
        );
    }
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
            "blink",
            "blink-hidden",
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
                "blink" | "blink-hidden" => cells[1].flags = 8,
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
            scene.text_blink_visible = case != "blink-hidden";
            if case == "selection" {
                scene.selection = Some(gmux_protocol::selection::Selection::linear(0, 0));
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
                "blink-hidden" => expected_cells[1].flags |= 32,
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
                    expected.set_text("").unwrap();
                }
                expected_cells[span.start].set_text(&text).unwrap();
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
    let text_blink = text_blink::TextBlink::new(&area);
    assert!(!text_blink.running() && text_blink.visible());
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
    assert!(
        !text_blink.running(),
        "Ordinary text must not schedule animation"
    );
    let animations = settings.is_gtk_enable_animations();
    settings.set_gtk_enable_animations(true);
    text_blink.update(true);
    wait(|| !text_blink.visible());
    let source = text_blink::source_id(&text_blink);
    let ready = text_blink::ready_time(&text_blink);
    for _ in 0..1000 {
        text_blink.update(true);
        cursor.restart();
    }
    assert_eq!(text_blink::source_id(&text_blink), source);
    assert_eq!(text_blink::ready_time(&text_blink), ready);
    settings.set_gtk_enable_animations(false);
    assert!(!text_blink.running() && text_blink.visible());
    assert!(
        cursor.running(),
        "Text animation preferences must preserve cursor behavior"
    );
    settings.set_gtk_enable_animations(true);
    assert!(text_blink.running());
    wait(|| !text_blink.visible());
    text_blink.update(false);
    assert!(!text_blink.running() && text_blink.visible());
    text_blink.update(true);
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
    frame.mouse_shape = gmux_protocol::cells::mouse_shape::CROSSHAIR;
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
        assert!(
            (before.1 + micros..=after.1 + micros + 1000)
                .contains(&text_blink::ready_time(&text_blink)),
            "Timing preference did not reschedule text blinking"
        );
        assert!(text_blink.visible());
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
    assert!(!text_blink.running() && text_blink.visible());
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
    assert!(
        text_blink.running(),
        "Visible text is independent of pane focus"
    );
    area.grab_focus();
    wait(|| cursor.running());
    window.set_visible(false);
    assert!(!cursor.running());
    assert!(!text_blink.running() && text_blink.visible());
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
    assert!(!text_blink.running() && text_blink.visible());
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
    assert!(
        text_blink.running(),
        "Cursor inactivity timeout applies only to the cursor"
    );
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
    let source = text_blink::source_id(&text_blink).unwrap();
    let weak = std::rc::Rc::downgrade(&text_blink);
    drop(text_blink);
    assert!(weak.upgrade().is_none());
    // Source IDs cannot be reconstructed, so inspect the context through GLib.
    use glib::translate::ToGlibPtr;
    assert!(
        unsafe {
            glib::ffi::g_main_context_find_source_by_id(
                glib::MainContext::default().to_glib_none().0,
                source,
            )
        }
        .is_null(),
        "Dropping the controller must remove its sleeping source"
    );
    let blink = text_blink::TextBlink::new(&area);
    blink.update(true);
    assert!(blink.running());
    content.remove(&area);
    assert!(!blink.running() && blink.visible());
    content.append(&area);
    wait(|| blink.running());
    window.destroy();
    assert!(!blink.running() && blink.visible());
    settings.set_gtk_enable_animations(animations);
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
        s.selection = Some(gmux_protocol::selection::Selection::linear(1, 19));
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
        scene.selection = Some(gmux_protocol::selection::Selection::linear(36, 44));
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
                c.set_text("").unwrap();
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
        ("password", 1),
        ("password-end", 1),
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
            "selection" => {
                scene.selection = Some(gmux_protocol::selection::Selection::linear(42, 45))
            }
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
            "password" => scene.frame.cursor_password_input = true,
            "password-end" => scene.frame.cursor_password_input = false,
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

fn validate_text_blink(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
    output: &Path,
) {
    let theme = Theme::adwaita(true);
    let mut cells = fixture(theme);
    // Alternate inside ASCII words, with wide/combining/fallback glyphs and
    // graphics in the same row. Backgrounds remain visible in both phases.
    for (index, cell) in cells.iter_mut().enumerate() {
        if index % 4 < 2 {
            cell.flags |= gmux_protocol::cells::flags::BLINK;
        }
        cell.bg = [30, 45, 60];
        cell.decoration = underline::STYLES[(index / 40) % 5]
            | decoration::OVERLINE
            | decoration::UNDERLINE_COLOR;
        cell.underline_color = [210, 40, 80];
    }
    cells[41] = cell("█", theme, 8 | 16);
    cells[3].flags |= 8;
    cells[4].flags |= 8;
    cells[42] = cell("e\u{301}", theme, 8 | 4 | 16);
    cells[43] = cell("hidden", theme, 8 | 32 | 16);
    let mut highlights = vec![0; cells.len()];
    highlights[81..90].fill(2);
    highlights[123..132].fill(1);
    for scale in [1.0, 1.5] {
        for cursor_style in [0, 1, 2] {
            for selection in [
                None,
                Some(gmux_protocol::selection::Selection::linear(1, 87)),
                Some(gmux_protocol::selection::Selection::rectangle(1, 87, 40)),
            ] {
                let mut renderer = Renderer::new();
                let mut scene = scene(&cells, theme);
                scene.scale = scale;
                scene.origin = (0.25, 0.5);
                scene.matches = &highlights;
                scene.selection = selection;
                scene.hovered_link = Some((0, 83));
                scene.frame.cursor_style = cursor_style;
                scene.frame.cursor_visible = 1;
                scene.frame.cursor_x = 3; // Wide cluster head in this fixture.
                for (step, shown) in [true, false, true, false].into_iter().enumerate() {
                    scene.text_blink_visible = shown;
                    let (actual, stats) = node(&mut renderer, &scene, font, context);
                    assert_eq!(stats.blinking_rows, 8);
                    if step > 0 {
                        assert_eq!(
                            (stats.rebuilt_rows, stats.background_rows, stats.text_nodes),
                            (0, 0, 0)
                        );
                    }
                    // Independent cell transformation is the oracle: conceal
                    // only blink ink, preserving original cell/background/cursor
                    // geometry, selection, search and hyperlink ownership.
                    let mut expected = cells.clone();
                    for cell in &mut expected {
                        if !shown && cell.blink() {
                            cell.flags |= 32;
                        }
                        if !shown {
                            cell.flags &= !gmux_protocol::cells::flags::BLINK;
                        }
                    }
                    let expected_scene = Scene {
                        cells: &expected,
                        text_blink_visible: true,
                        ..scene
                    };
                    let expected = node(&mut Renderer::new(), &expected_scene, font, context).0;
                    if shown {
                        assert_render_eq(
                            gpu,
                            &actual,
                            &expected,
                            &scene,
                            output,
                            "text-blink-shown-cache",
                        );
                    } else {
                        let actual_pixels = pixels(&render(gpu, &actual, &scene));
                        let expected_pixels = pixels(&render(gpu, &expected, &scene));
                        let width = (scene.width as f64 * scale).ceil() as usize;
                        // Different text-node partitions can change synthetic
                        // italic edge clipping in GSK. Apply the independent
                        // Pango oracle's tolerance to each cell, so one missing
                        // glyph cannot be diluted in a mostly blank viewport.
                        for y in 0..8 {
                            for x in 0..40 {
                                let left = (x as f64 * scene.metrics.0 * scale) as usize;
                                let right = ((x + 1) as f64 * scene.metrics.0 * scale) as usize;
                                let top = (y as f64 * scene.metrics.1 * scale) as usize;
                                let bottom = ((y + 1) as f64 * scene.metrics.1 * scale) as usize;
                                let mut delta = 0;
                                for py in top..bottom {
                                    for channel in (py * width + left) * 4..(py * width + right) * 4
                                    {
                                        delta += actual_pixels[channel]
                                            .abs_diff(expected_pixels[channel])
                                            as usize;
                                    }
                                }
                                let mean =
                                    delta as f64 / ((right - left) * (bottom - top) * 4) as f64;
                                if mean >= 1.0 {
                                    assert_render_eq(
                                        gpu,
                                        &actual,
                                        &expected,
                                        &scene,
                                        output,
                                        &format!(
                                            "text-blink-hidden-{scale}-{cursor_style}-{x}-{y}"
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
                // Hyperlink hover must retain the glyph nodes even in the
                // hidden phase, including blank blinking cells gaining ink.
                scene.hovered_link = None;
                let (cached, stats) = node(&mut renderer, &scene, font, context);
                assert_eq!((stats.rebuilt_rows, stats.text_nodes), (0, 0));
                let fresh = node(&mut Renderer::new(), &scene, font, context).0;
                assert_render_eq(gpu, &cached, &fresh, &scene, output, "text-blink-hover");
                for (visible, focused) in [(0, true), (1, true), (1, false)] {
                    scene.frame.cursor_visible = visible;
                    scene.focused = focused;
                    let (cached, stats) = node(&mut renderer, &scene, font, context);
                    assert_eq!(stats.rebuilt_rows, usize::from(cursor_style == 1));
                    let fresh = node(&mut Renderer::new(), &scene, font, context).0;
                    assert_render_eq(
                        gpu,
                        &cached,
                        &fresh,
                        &scene,
                        output,
                        "text-blink-cursor-phase",
                    );
                }
            }
        }
    }
    // Blank or concealed cells and blinking ink outside the allocation must
    // not keep a source alive. A hovered blank link is visible decoration.
    let mut blanks = vec![cell(" ", theme, 8); 320];
    blanks[0] = cell("X", theme, 8 | 32);
    let mut renderer = Renderer::new();
    let mut s = scene(&blanks, theme);
    assert_eq!(node(&mut renderer, &s, font, context).1.blinking_rows, 0);
    s.hovered_link = Some((1, 4));
    assert_eq!(node(&mut renderer, &s, font, context).1.blinking_rows, 1);
    s.text_blink_visible = false;
    let cached = node(&mut renderer, &s, font, context).0;
    let fresh = node(&mut Renderer::new(), &s, font, context).0;
    assert_render_eq(gpu, &cached, &fresh, &s, output, "text-blink-blank-hover");
    s.width = 10;
    assert_eq!(node(&mut renderer, &s, font, context).1.blinking_rows, 0);
    s.width = 400;
    s.height = 0;
    assert_eq!(
        renderer
            .snapshot(&gtk::Snapshot::new(), &s, font, context)
            .blinking_rows,
        0
    );
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
            for cell in cells.iter_mut().step_by(3) {
                cell.flags |= gmux_protocol::cells::flags::BLINK;
            }
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
                scene.text_blink_visible = step % 2 == 0;
                scene.height = 168;
                scene.scale = scale;
                scene.origin = origin;
                if step >= 2 {
                    scene.selection = Some(gmux_protocol::selection::Selection::linear(42, 85));
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
            .clone_from_slice(&cells[source * 40..(source + 1) * 40]);
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

fn validate_rtl(gpu: &gsk::Renderer, font: &pango::FontDescription, context: &pango::Context) {
    fn glyphs(node: &gsk::RenderNode, result: &mut Vec<u32>) {
        if let Some(text) = node.downcast_ref::<gsk::TextNode>() {
            result.extend(text.glyphs().into_iter().map(|g| g.glyph()));
        }
        for child in node.children() {
            glyphs(&child, result);
        }
    }
    let theme = Theme::adwaita(true);
    let mut cells = vec![cell(" ", theme, 0); 320];
    for (i, text) in ["سَ", "ل", "ا", "م"].into_iter().enumerate() {
        cells[i] = cell(text, theme, 0);
    }
    let bidi = bidi::Grid::update(40, &cells, None).unwrap();
    let mut scene = scene(&cells, theme);
    scene.bidi = Some(&bidi);
    scene.scale = 2.0;
    let mut renderer = Renderer::new();
    let (joined, _) = node(&mut renderer, &scene, font, context);
    // Independent Pango drawing supplies the expected contextual glyphs,
    // including the vowel mark and lam-alef ligature.
    let layout = pango::Layout::new(context);
    layout.set_font_description(Some(font));
    layout.set_text("سَلام");
    let expected = gtk::Snapshot::new();
    expected.append_layout(&layout, &rgba(theme.foreground, 1.0));
    let mut want = Vec::new();
    glyphs(&expected.to_node().unwrap(), &mut want);
    let mut actual = Vec::new();
    glyphs(&joined, &mut actual);
    assert_eq!(
        actual, want,
        "Arabic must retain contextual shaping in visual order"
    );
    let (cached, stats) = node(&mut renderer, &scene, font, context);
    assert_eq!(stats.rebuilt_rows, 0);
    assert_eq!(joined.serialize(), cached.serialize());

    scene.selection = Some(gmux_protocol::selection::Selection::linear(0, 0));
    scene.frame.cursor_x = 1;
    scene.frame.cursor_style = 1;
    scene.frame.cursor_visible = 1;
    let (chosen, _) = node(&mut renderer, &scene, font, context);
    let mut with_selection = Vec::new();
    glyphs(&chosen, &mut with_selection);
    with_selection.dedup();
    want.dedup();
    assert_eq!(
        with_selection, want,
        "Selection and cursor must not change joining forms"
    );
    let texture = render(gpu, &chosen, &scene);
    let pixels = pixels(&texture);
    let stripe = |col: usize| &pixels[(col * 20 + 1) * 4..][..4];
    let bgra = |[r, g, b]: [u8; 3]| [b, g, r, 255];
    assert_eq!(stripe(0), bgra(theme.background));
    assert_eq!(stripe(2), bgra(scene.frame.cursor));
    assert_eq!(stripe(3), bgra(theme.selection_background));

    if layout.unknown_glyphs_count() == 0 {
        scene.selection = None;
        scene.frame.cursor_visible = 0;
        let mut letters = cells.clone();
        for target in &mut letters[..3] {
            *target = cell("ب", theme, 0);
        }
        letters[3] = cell(" ", theme, 0);
        let order = bidi::Grid::update(40, &letters, None).unwrap();
        let scene = Scene {
            cells: &letters,
            bidi: Some(&order),
            ..scene
        };
        let (joined, _) = node(&mut renderer, &scene, font, context);
        let texture = render(gpu, &joined, &scene);
        let data = self::pixels(&texture);
        let ink = |x, y| data[(y * texture.width() as usize + x) * 4] > theme.background[2] + 40;
        for seam in [20, 40] {
            assert!(
                (0..48).any(|y| ink(seam - 1, y) && ink(seam, y)),
                "Cursive strokes should reach across the cell edge at {seam}"
            );
        }
    }
}

fn validate_box_drawing(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
) {
    let theme = Theme {
        foreground: [255; 3],
        background: [0; 3],
        ..Theme::adwaita(true)
    };
    let cells: Vec<_> = [
        "         ",
        " ╭─╮ ┏━┓ ",
        " │ │ ┃ ┃ ",
        " ╰─╯ ┗━┛ ",
        "         ",
    ]
    .iter()
    .flat_map(|row| {
        row.chars()
            .map(|ch| cell(ch.encode_utf8(&mut [0; 4]), theme, 4))
    })
    .collect();
    let mut scene = scene(&cells, theme);
    scene.frame.cols = 9;
    scene.frame.rows = 5;
    scene.metrics = (9.0, 21.0);
    scene.width = 81;
    scene.height = 105;
    let mut renderer = Renderer::new();
    for (scale, light, heavy) in [(1.0, 1, 2), (1.25, 1, 3), (1.5, 2, 3), (2.0, 2, 5)] {
        for origin in [(0.0, 0.0), (0.25, 0.5), (-0.25, -0.5)] {
            scene.scale = scale;
            scene.origin = origin;
            let (node, stats) = node(&mut renderer, &scene, font, context);
            assert_eq!(stats.text_nodes, 0, "Box drawing must not use font glyphs");
            let texture = render(gpu, &node, &scene);
            let data = pixels(&texture);
            assert!(
                data.as_chunks::<4>().0.iter().all(|pixel| pixel[0] <= 129),
                "Faint strokes overlap"
            );
            let sample = |x, y| data[(y * texture.width() as usize + x) * 4];
            let x = |col: usize| ((col as f64 * 9.0 + origin.0) * scale).round() as usize;
            let y = |row: usize| ((row as f64 * 21.0 + origin.1) * scale).round() as usize;
            let horizontal = |px, row| {
                (y(row)..y(row + 1))
                    .map(|py| sample(px, py))
                    .collect::<Vec<_>>()
            };
            let vertical = |py, col| {
                (x(col)..x(col + 1))
                    .map(|px| sample(px, py))
                    .collect::<Vec<_>>()
            };
            let check_width = |stroke: &[u8], thickness| {
                assert_eq!(
                    stroke.iter().filter(|&&a| a >= 126).count(),
                    thickness,
                    "{scale}× at {origin:?}: {stroke:?}"
                );
                assert!(
                    stroke.iter().all(|&a| a <= 2 || a >= 126),
                    "Stroke not pixel-aligned: {stroke:?}"
                );
            };
            let check_join = |stroke: &[u8], join: &[u8]| {
                assert!(
                    stroke.iter().zip(join).all(|(&a, &b)| a.abs_diff(b) <= 8),
                    "Broken box join at {scale}×, {origin:?}: {stroke:?} vs {join:?}",
                );
            };
            for (left, thickness) in [(1, light), (5, heavy)] {
                let right = left + 2;
                for row in [1, 3] {
                    let h = horizontal((x(left + 1) + x(right)) / 2, row);
                    check_width(&h, thickness);
                    for seam in [x(left + 1), x(right)] {
                        for px in [seam - 1, seam] {
                            check_join(&h, &horizontal(px, row));
                        }
                    }
                    for col in [left, right] {
                        let v = vertical((y(2) + y(3)) / 2, col);
                        check_width(&v, thickness);
                        let seam = y(if row == 1 { 2 } else { 3 });
                        for py in [seam - 1, seam] {
                            check_join(&v, &vertical(py, col));
                        }
                        if left == 1 && scale == 2.0 {
                            // The bend cuts off the outside of a square elbow,
                            // but must still cross its inward diagonal.
                            let outside = |stroke: &[u8], reverse| {
                                if reverse {
                                    stroke.iter().rposition(|&a| a > 64)
                                } else {
                                    stroke.iter().position(|&a| a > 64)
                                }
                                .unwrap()
                            };
                            let px = x(col) + outside(&v, col == right);
                            let py = y(row) + outside(&h, row == 3);
                            assert!(sample(px, py) < 32, "Rounded corner is square");
                            assert!(
                                (1..=5).any(|d| sample(
                                    if col == right { px - d } else { px + d },
                                    if row == 3 { py - d } else { py + d },
                                ) > 64),
                                "Rounded corner has no bend"
                            );
                        }
                    }
                }
            }
        }
    }
}

fn validate_extended_boxes(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
) {
    const COLS: usize = 16;
    let theme = Theme {
        foreground: [255; 3],
        background: [0; 3],
        ..Theme::adwaita(true)
    };
    let lines = [
        " ─╼━ ━╾─ ",
        " ╔═╦═╗   ",
        " ║ ║ ║   ",
        " ╠═╬═╣   ",
        " ║ ║ ║   ",
        " ╚═╩═╝   ",
        " ╌┄┈ ╱╲╳ ",
    ];
    let cells: Vec<_> = lines
        .iter()
        .flat_map(|line| line.chars().chain(std::iter::repeat(' ')).take(COLS))
        .chain((0x2500..=0x257f).map(|cp| char::from_u32(cp).unwrap()))
        .map(|ch| cell(ch.encode_utf8(&mut [0; 4]), theme, 4))
        .collect();
    let mut scene = scene(&cells, theme);
    scene.frame.cols = COLS as u16;
    scene.frame.rows = (cells.len() / COLS) as u16;
    scene.metrics = (9.0, 21.0);
    scene.width = 9 * i32::from(scene.frame.cols);
    scene.height = 21 * i32::from(scene.frame.rows);
    scene.origin = (0.25, 0.5);
    let mut renderer = Renderer::new();
    for scale in [1.0, 1.25, 2.0] {
        scene.scale = scale;
        let (node, stats) = node(&mut renderer, &scene, font, context);
        assert_eq!(
            stats.text_nodes, 0,
            "The full box drawing block must be font-independent"
        );
        let texture = render(gpu, &node, &scene);
        let data = pixels(&texture);
        assert!(
            data.as_chunks::<4>().0.iter().all(|p| p[0] <= 129),
            "Faint junctions must not be painted twice"
        );
        let sample = |x, y| data[(y * texture.width() as usize + x) * 4];
        let x = |col: usize| ((col as f64 * 9.0 + scene.origin.0) * scale).round() as usize;
        let y = |row: usize| ((row as f64 * 21.0 + scene.origin.1) * scale).round() as usize;
        let same = |a: u8, b: u8| a.abs_diff(b) <= 2;
        for seam in [2, 3, 6, 7] {
            for py in y(0)..y(1) {
                assert!(
                    same(sample(x(seam) - 1, py), sample(x(seam), py)),
                    "Mixed-weight seam at column {seam}, scale {scale}"
                );
            }
        }
        for col in [1, 3, 5] {
            let profile = |py| (x(col)..x(col + 1)).map(move |px| sample(px, py));
            let middle = (y(2) + y(3)) / 2;
            let mut last = false;
            let mut rails = 0;
            for sample in profile(middle) {
                let lit = sample > 64;
                rails += usize::from(lit && !last);
                last = lit;
            }
            assert_eq!(rails, 2, "The double border needs two distinct rails");
            for row in 2..=5 {
                for py in [y(row) - 1, y(row)] {
                    assert!(
                        profile(py).zip(profile(middle)).all(|(a, b)| same(a, b)),
                        "Double vertical seam on {} in column {col}, row {row}, scale {scale}: {:?} vs {:?}",
                        gpu.type_().name(),
                        profile(py).collect::<Vec<_>>(),
                        profile(middle).collect::<Vec<_>>()
                    );
                }
            }
        }
        for row in [1, 3, 5] {
            for col in 2..=5 {
                assert!(
                    (y(row)..y(row + 1)).all(|py| same(sample(x(col) - 1, py), sample(x(col), py))),
                    "Double horizontal seam in column {col}, row {row}, scale {scale}"
                );
            }
        }
        for col in 1..=3 {
            let py = (y(6)..y(7))
                .find(|&py| (x(col)..x(col + 1)).any(|px| sample(px, py) > 64))
                .unwrap();
            let strokes = (x(col)..x(col + 1))
                .filter(|&px| sample(px, py) > 64 && (px == x(col) || sample(px - 1, py) <= 64))
                .count();
            assert_eq!(strokes, col + 1, "Wrong dash count at scale {scale}");
        }
        if scale == 2.0 {
            let seam = x(6);
            assert!(
                sample(seam - 1, y(6)) > 64 && sample(seam, y(6)) > 64,
                "Diagonals do not reach their shared corner"
            );
        }
        for offset in 0..128 {
            let col = offset % COLS;
            let row = lines.len() + offset / COLS;
            assert!(
                (y(row)..y(row + 1).min(texture.height() as usize)).any(|py| {
                    (x(col)..x(col + 1).min(texture.width() as usize)).any(|px| sample(px, py) > 32)
                }),
                "Empty box glyph U+{:04X} at scale {scale}",
                0x2500 + offset
            );
        }
    }
}

fn validate_kitty_graphics(
    gpu: &gsk::Renderer,
    font: &pango::FontDescription,
    context: &pango::Context,
) {
    use gmux_protocol::graphics::{Graphics, Image, ImageSizing, Layer, PixelFormat, Placement};
    use std::sync::Arc;

    let theme = Theme {
        background: [0; 3],
        foreground: [0, 0, 255],
        ..Theme::adwaita(true)
    };
    let mut cells = vec![cell(" ", theme, 0); 40 * 8];
    cells[1].bg = [0, 255, 0];
    cells[2] = cell("█", theme, 0);
    cells[3] = cell("█", theme, 0);
    cells[5] = cell("\u{10eeee}\u{0305}\u{0305}", theme, 0);
    cells[7].decoration |= decoration::EXPLICIT_BACKGROUND;
    cells[8].decoration = decoration::OVERLINE | gmux_protocol::cells::underline::SINGLE;
    let image = |key, width, format, pixels: &[u8]| {
        Arc::new(Image {
            key,
            width,
            height: 1,
            format,
            pixels: pixels.to_vec(),
        })
    };
    let placement = |image, layer, col, width, source| Placement {
        image,
        layer,
        cell: [col, 0],
        offset: [0, 0],
        size: [width, 24],
        source,
        source_subpixels: None,
        fit_rows: None,
        sizing: None,
    };
    let graphics = Graphics {
        cell_pixels: [10, 24],
        unicode_cell_pixels: None,
        frozen_cell_pixels: None,
        images: vec![
            image(1, 1, PixelFormat::Rgb, &[255, 0, 0]),
            image(2, 1, PixelFormat::Rgba, &[255, 0, 0, 128]),
            image(3, 3, PixelFormat::Rgb, &[0, 0, 255, 0, 255, 0, 0, 0, 255]),
        ],
        placements: vec![
            placement(1, Layer::BelowBackground, -1, 30, [0, 0, 1, 1]),
            placement(1, Layer::BelowBackground, 6, 20, [0, 0, 1, 1]),
            placement(1, Layer::BelowText, 2, 10, [0, 0, 1, 1]),
            placement(1, Layer::BelowText, 5, 10, [0, 0, 1, 1]),
            placement(1, Layer::BelowText, 8, 20, [0, 0, 1, 1]),
            placement(2, Layer::AboveText, 3, 10, [0, 0, 1, 1]),
            placement(3, Layer::AboveText, 4, 10, [1, 0, 1, 1]),
        ],
    };
    let mut renderer = Renderer::new();
    let mut scene = scene(&cells, theme);
    scene.graphics = Some(&graphics);
    scene.focused = true;
    scene.frame.cursor_visible = 1;
    scene.frame.cursor_style = 1;
    scene.frame.cursor_x = 9;
    scene.frame.cursor = [0, 255, 0];
    let painted = node(&mut renderer, &scene, font, context).0;
    let data = pixels(&render(gpu, &painted, &scene));
    for (x, expected) in [
        (5, [0, 0, 255, 255]),
        (15, [0, 255, 0, 255]),
        (25, [255, 0, 0, 255]),
        (35, [127, 0, 128, 255]),
        (45, [0, 255, 0, 255]),
        (55, [0, 0, 255, 255]),
        (65, [0, 0, 255, 255]),
        (75, [0, 0, 0, 255]),
        (95, [0, 255, 0, 255]),
    ] {
        let start = (12 * scene.width as usize + x) * 4;
        assert!(
            data[start..start + 4]
                .iter()
                .zip(expected)
                .all(|(&a, b)| a.abs_diff(b) <= 1),
            "Unexpected image color at x={x}: {:?}",
            &data[start..start + 4]
        );
    }
    for half in [0..12, 12..24] {
        assert!(
            half.into_iter().any(|y| {
                let start = (y * scene.width as usize + 85) * 4;
                data[start..start + 4] == [255, 0, 0, 255]
            }),
            "An image below text covered an underline or overline"
        );
    }
    let mut expected = cells.clone();
    expected[5] = cell(" ", theme, 0);
    let empty_placeholder = node(
        &mut renderer,
        &Scene {
            cells: &expected,
            ..scene
        },
        font,
        context,
    )
    .0;
    assert_eq!(data, pixels(&render(gpu, &empty_placeholder, &scene)));
    let without_images = Scene {
        graphics: None,
        ..scene
    };
    let _ = node(&mut renderer, &without_images, font, context);
    let restored = node(&mut renderer, &scene, font, context).0;
    assert_eq!(data, pixels(&render(gpu, &restored, &scene)));

    let pixels_across_rows = std::sync::Arc::new(Image {
        key: 1,
        width: 3,
        height: 3,
        format: PixelFormat::Rgb,
        pixels: [[255, 0, 0], [0, 255, 0], [0, 0, 255]]
            .into_iter()
            .flat_map(|row| row.repeat(3))
            .collect(),
    });
    let blank = vec![cell(" ", theme, 0); 40 * 8];
    let mut tiled = crate::scene(&blank, theme);
    tiled.frame.cursor_visible = 0;
    tiled.metrics = (8.0, 16.0);
    let unit = gmux_protocol::graphics::SOURCE_SUBPIXELS;
    let graphics = Graphics {
        cell_pixels: [8, 16],
        unicode_cell_pixels: None,
        frozen_cell_pixels: None,
        images: vec![pixels_across_rows],
        placements: vec![
            Placement {
                offset: [0, 8],
                size: [16, 8],
                source_subpixels: Some([0, 0, 3 * unit, 3 * unit / 2]),
                ..placement(1, Layer::BelowText, 0, 16, [0, 0, 3, 2])
            },
            Placement {
                cell: [0, 1],
                size: [16, 8],
                source_subpixels: Some([0, 3 * unit / 2, 3 * unit, 3 * unit / 2]),
                ..placement(1, Layer::BelowText, 0, 16, [0, 2, 3, 1])
            },
            Placement {
                offset: [0, 8],
                size: [16, 16],
                ..placement(1, Layer::BelowText, 2, 16, [0, 0, 3, 3])
            },
        ],
    };
    tiled.graphics = Some(&graphics);
    let drawn = node(&mut renderer, &tiled, font, context).0;
    let data = pixels(&render(gpu, &drawn, &tiled));
    for y in 8..24 {
        let start = y * tiled.width as usize * 4;
        let (tiles, reference) = data[start..start + 128].split_at(64);
        assert_eq!(tiles, reference, "Unicode image seam on pixel row {y}");
    }

    let mut bidi_cells = blank.clone();
    for (x, text) in ["\u{202e}", "س", "م", "ا", "\u{10eeee}", "\u{202c}"]
        .into_iter()
        .enumerate()
    {
        bidi_cells[x] = cell(text, theme, 0);
    }
    let bidi = bidi::Grid::update(40, &bidi_cells, None).unwrap();
    let logical = 4;
    let visual = bidi.visual(logical);
    assert!(visual < logical);
    tiled.cells = &bidi_cells;
    tiled.bidi = Some(&bidi);
    let graphics = Graphics {
        cell_pixels: [8, 16],
        unicode_cell_pixels: None,
        frozen_cell_pixels: None,
        images: vec![
            image(1, 1, PixelFormat::Rgb, &[0, 255, 0]),
            image(2, 1, PixelFormat::Rgb, &[255, 0, 0]),
        ],
        placements: vec![
            Placement {
                size: [8, 16],
                source_subpixels: Some([0, 0, unit, unit]),
                ..placement(1, Layer::BelowText, logical as i32, 8, [0, 0, 1, 1])
            },
            Placement {
                cell: [visual as i32, 1],
                size: [8, 16],
                ..placement(2, Layer::AboveText, logical as i32, 8, [0, 0, 1, 1])
            },
        ],
    };
    tiled.graphics = Some(&graphics);
    let drawn = node(&mut renderer, &tiled, font, context).0;
    let data = pixels(&render(gpu, &drawn, &tiled));
    let sample = |column: usize, row: usize| {
        let byte = ((row * 16 + 8) * tiled.width as usize + column * 8 + 4) * 4;
        &data[byte..byte + 4]
    };
    assert_eq!(sample(visual, 0), [0, 255, 0, 255]);
    assert_ne!(sample(logical, 0), [0, 255, 0, 255]);
    assert_eq!(sample(visual, 1), [0, 0, 255, 255]);
    assert_eq!(sample(logical, 1), [0, 0, 0, 255]);
    let wide = tiled.width;
    tiled.width = ((visual + 1) * 8) as i32;
    let drawn = node(&mut renderer, &tiled, font, context).0;
    let data = pixels(&render(gpu, &drawn, &tiled));
    let sample = (8 * tiled.width as usize + visual * 8 + 4) * 4;
    assert_eq!(data[sample..sample + 4], [0, 255, 0, 255]);
    tiled.width = wide;

    // The size owner's cell aspect ratio can differ from this viewer's.
    tiled.metrics = (10.0, 24.0);
    tiled.cells = &blank;
    tiled.bidi = None;
    let graphics = Graphics {
        cell_pixels: [8, 16],
        unicode_cell_pixels: None,
        frozen_cell_pixels: None,
        images: vec![
            image(1, 1, PixelFormat::Rgb, &[255, 0, 0]),
            Arc::new(Image {
                key: 2,
                width: 8,
                height: 8,
                format: PixelFormat::Rgb,
                pixels: [255, 0, 0].repeat(64),
            }),
        ],
        placements: vec![
            Placement {
                cell: [4, 1],
                size: [8, 48],
                fit_rows: Some([1, 2]),
                ..placement(1, Layer::AboveText, 4, 8, [0, 0, 1, 1])
            },
            Placement {
                cell: [6, 1],
                size: [8, 8],
                sizing: Some(ImageSizing::Columns),
                ..placement(2, Layer::AboveText, 6, 8, [0, 0, 8, 8])
            },
            Placement {
                cell: [8, 1],
                size: [16, 16],
                sizing: Some(ImageSizing::Rows),
                ..placement(2, Layer::AboveText, 8, 16, [0, 0, 8, 8])
            },
            Placement {
                cell: [11, 1],
                size: [8, 8],
                sizing: Some(ImageSizing::Native),
                ..placement(2, Layer::AboveText, 11, 8, [0, 0, 8, 8])
            },
        ],
    };
    tiled.graphics = Some(&graphics);
    let drawn = node(&mut renderer, &tiled, font, context).0;
    let data = pixels(&render(gpu, &drawn, &tiled));
    for y in 24..48 {
        for x in 37..122 {
            let byte = (y * tiled.width as usize + x) * 4;
            let red = ((40..50).contains(&x) && (31..41).contains(&y))
                || ((60..70).contains(&x) && y < 34)
                || (80..104).contains(&x)
                || ((110..118).contains(&x) && y < 32);
            assert_eq!(data[byte..byte + 4], [0, 0, if red { 255 } else { 0 }, 255]);
        }
    }

    // Two overlapping crops can consume the cache using pixels from one image.
    let mut source = vec![255; gmux_protocol::graphics::MAX_PIXEL_BYTES];
    for y in 0..3 {
        for x in 0..3 {
            let start = ((y + 11) * 2000 + x + 11) * 4;
            source[start..start + 4].copy_from_slice(&[x as u8 * 120, y as u8 * 100, 0, 150]);
        }
    }
    let graphics = Graphics {
        cell_pixels: [8, 16],
        unicode_cell_pixels: None,
        frozen_cell_pixels: None,
        images: vec![Arc::new(Image {
            key: 4,
            width: 2000,
            height: 1250,
            format: PixelFormat::Rgba,
            pixels: source,
        })],
        placements: [
            [11, 11, 3, 3],
            [12, 11, 1, 3],
            [11, 12, 3, 1],
            [12, 12, 1, 1],
            [11, 11, 3, 3],
        ]
        .into_iter()
        .zip([23, 23, 23, 23, 2])
        .enumerate()
        .map(|(i, (crop, width))| placement(4, Layer::AboveText, 4 + i as i32 * 4, width, crop))
        .collect(),
    };
    tiled.metrics = (8.25, 16.25);
    let full_cache = Graphics {
        images: graphics.images.clone(),
        placements: [
            placement(4, Layer::AboveText, 0, 8, [0, 0, 1000, 1250]),
            placement(4, Layer::AboveText, 1, 8, [500, 0, 1000, 1250]),
        ]
        .into_iter()
        .chain(graphics.placements.iter().copied())
        .collect(),
        cell_pixels: graphics.cell_pixels,
        ..Graphics::default()
    };
    let mut regular_renderer = Renderer::new();
    let mut overflow_renderer = Renderer::new();
    for (scale, origin) in [(1.0, (0.0, 0.0)), (1.25, (0.2, 0.4))] {
        let paint = |renderer: &mut Renderer, graphics| {
            let scene = Scene {
                graphics: Some(graphics),
                scale,
                origin,
                ..tiled
            };
            let drawn = node(renderer, &scene, font, context).0;
            render(gpu, &drawn, &scene)
        };
        let reference = paint(&mut regular_renderer, &graphics);
        let expected = pixels(&reference);
        let actual = pixels(&paint(&mut overflow_renderer, &full_cache));
        for y in 0..(26.0 * scale) as usize {
            for x in (32.0 * scale) as usize..(170.0 * scale) as usize {
                let offset = (y * reference.width() as usize + x) * 4;
                let actual = &actual[offset..offset + 4];
                let expected = &expected[offset..offset + 4];
                assert!(
                    actual
                        .iter()
                        .zip(expected)
                        .all(|(&a, &b)| a.abs_diff(b) <= 1),
                    "Crop filtering changed at scale {scale} at ({x}, {y}): {expected:?} vs {actual:?}"
                );
            }
        }
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
    scene.selection = Some(gmux_protocol::selection::Selection::linear(2, 5));
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

    // Rectangle geometry and glyph colors must equal independently recolored
    // cells, including wide glyphs intersected at either edge.
    let mut rectangle_cells = cells.clone();
    rectangle_cells[1] = cell("界", theme, 0);
    rectangle_cells[2] = cell("", theme, 64);
    rectangle_cells[3] = cell("e\u{301}", theme, 0);
    rectangle_cells[45] = cell("界", theme, 0);
    rectangle_cells[46] = cell("", theme, 64);
    scene.cells = &rectangle_cells;
    scene.matches = &[];
    scene.selection = Some(gmux_protocol::selection::Selection::rectangle(2, 85, 40));
    let actual = node(&mut Renderer::new(), &scene, font, context).0;
    let mut expected_cells = rectangle_cells.clone();
    for (row, left, right) in [(0, 1, 5), (1, 2, 6), (2, 2, 5)] {
        for cell in &mut expected_cells[row * 40 + left..=row * 40 + right] {
            cell.fg = theme.selection_foreground;
            cell.bg = theme.selection_background;
        }
    }
    scene.cells = &expected_cells;
    scene.selection = None;
    let expected = node(&mut Renderer::new(), &scene, font, context).0;
    assert_eq!(
        pixels(&render(gpu, &actual, &scene)),
        pixels(&render(gpu, &expected, &scene)),
        "Rectangle selection must paint only the selected row columns and complete glyphs"
    );

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

fn registered_glyph(gpu: &gsk::Renderer, context: &pango::Context, output: &Path) {
    use gmux_protocol::glyphs::{
        Extent, Glyph, Glyphs, Horizontal, Outline, Point, Size, Vertical,
    };
    use std::sync::Arc;

    let theme = Theme::adwaita(true);
    let font = pango::FontDescription::from_string("Monospace 12");
    let cp = '\u{100000}';
    let mut cells = vec![cell(" ", theme, 0); 40 * 8];
    cells[0] = cell("\u{100000}", theme, gmux_protocol::cells::flags::BLINK);
    cells[40] = cell("A", theme, 0);
    let mut scene = scene(&cells, theme);
    let mut renderer = Renderer::new();
    renderer.set_cairo_glyphs(gpu.is::<gsk::CairoRenderer>());
    let capture = |renderer: &mut Renderer, scene: &Scene<'_>| {
        let snapshot = gtk::Snapshot::new();
        let stats = renderer.snapshot(&snapshot, scene, &font, context);
        (snapshot.to_node().unwrap(), stats.rebuilt_rows)
    };
    let (fallback, _) = capture(&mut renderer, &scene);
    let glossary = |left, right| {
        let points = [(left, 250), (right, 250), (right, 750), (left, 750)];
        Glyphs::new(vec![Arc::new(Glyph {
            codepoint: cp,
            upm: 1000,
            extent: Some(Extent {
                advance: 1000,
                line_height: 1000,
                ymin: 0,
            }),
            width: 2,
            size: Size::Stretch,
            horizontal: Horizontal::Center,
            vertical: Vertical::Center,
            pad: [0.0; 4],
            outline: Outline::new(points.into_iter().enumerate().map(|(i, (x, y))| Point {
                x,
                y,
                on_curve: true,
                contour_end: i == 3,
            })),
            revision: 0,
        })])
    };
    let original = glossary(200, 800);
    renderer.set_glyphs(&original);
    let (first, rebuilt) = capture(&mut renderer, &scene);
    assert_eq!(rebuilt, 1);

    let mut repeated: Glyphs =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    repeated.reuse_entries(&original);
    assert!(repeated.same(&original));
    renderer.set_glyphs(&repeated);
    scene.text_blink_visible = false;
    let (hidden, rebuilt) = capture(&mut renderer, &scene);
    assert_eq!(rebuilt, 0);
    scene.text_blink_visible = true;
    let expected = |x: f32, width: f32| {
        let snapshot = gtk::Snapshot::new();
        snapshot.append_node(&hidden);
        snapshot.append_color(
            &gdk::RGBA::new(
                f32::from(theme.foreground[0]) / 255.0,
                f32::from(theme.foreground[1]) / 255.0,
                f32::from(theme.foreground[2]) / 255.0,
                1.0,
            ),
            &graphene::Rect::new(x, 6.0, width, 12.0),
        );
        snapshot.to_node().unwrap()
    };
    assert_render_eq(
        gpu,
        &first,
        &expected(4.0, 12.0),
        &scene,
        output,
        "registered-glyph",
    );
    renderer.set_glyphs(&glossary(500, 1500));
    let (replacement, rebuilt) = capture(&mut renderer, &scene);
    assert_eq!(rebuilt, 1);
    assert_render_eq(
        gpu,
        &replacement,
        &expected(10.0, 10.0),
        &scene,
        output,
        "replaced-glyph",
    );

    renderer.set_glyphs(&Glyphs::default());
    let (cleared, rebuilt) = capture(&mut renderer, &scene);
    assert_eq!(rebuilt, 1);
    assert_render_eq(gpu, &cleared, &fallback, &scene, output, "cleared-glyph");

    if gpu.is::<gsk::CairoRenderer>() {
        scene.scale = 1.5;
        scene.origin = (1.0 / 3.0, 1.0 / 3.0);
        renderer.set_glyphs(&original);
        let (cached, _) = capture(&mut renderer, &scene);
        renderer.set_cairo_glyphs(false);
        let (vector, _) = capture(&mut renderer, &scene);
        assert_render_eq(gpu, &cached, &vector, &scene, output, "fractional-glyph");
    }
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
    registered_glyph(cache_gpu, &context, &options.output);
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
    validate_kitty_graphics(&cairo.0, &font, &context);
    validate_rtl(&cairo.0, &font, &context);
    validate_box_drawing(&cairo.0, &font, &context);
    validate_extended_boxes(&cairo.0, &font, &context);
    if gpu.type_() != cairo.0.type_() {
        validate_kitty_graphics(&gpu, &font, &context);
        validate_rtl(&gpu, &font, &context);
        validate_box_drawing(&gpu, &font, &context);
        validate_extended_boxes(&gpu, &font, &context);
    }
    validate_decorations(cache_gpu, &context, &options.output);
    validate_decoration_patterns(cache_gpu, &font, &context);
    validate_batched_patterns(cache_gpu, &font, &context, &options.output);
    validate_batched_patterns(&gpu, &font, &context, &options.output);
    validate_link_hover(cache_gpu, &font, &context, &options.output);
    validate_text_blink(cache_gpu, &font, &context, &options.output);
    std::fs::write(
        options.output.join("checks.json"),
        serde_json::to_string_pretty(&json!({
            "gtk_renderer": gpu.type_().name(), "cache_renderer": cache_gpu.type_().name(),
            "cursor_lifecycle": options.cursor_lifecycle,
            "glyph_cases": cases,
            "checks": ["exact cache pixels", "independent layout agreement", "row invalidation", "scroll cache reuse", "bounded shape cache",
                       "font and context changes", "fractional blocks", "all 128 box glyphs, stroke widths and joins", "linear and rectangular selection and search", "link hover SGR pixels and glyph cache retention",
                       "distinct underline patterns and overline geometry", "batched pattern rectangle equivalence", "cursor blink row reuse", "colored underlines beneath descenders", "decoration color invalidation", "concealed text", "blank and wide decorations", "wide cursor", "background batching", "overlapping fallback order",
                       "ligatures and contextual substitutions", "ligature style and cell boundaries", "GPU ligature layouts", "Arabic joining and visual selection geometry",
                       "text blink phases, decorations, cursor and selection pixels", "text blink retains row and glyph nodes", "Kitty image clipping, layering, cropping and alpha"]
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
