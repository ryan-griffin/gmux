//! Check terminal rendering against GTK's independent Pango layout drawing.
#[allow(dead_code)]
#[path = "../src/render.rs"]
mod render;
#[allow(dead_code)]
#[path = "../src/renderer.rs"]
mod renderer;

use clap::{Parser, ValueEnum};
use gmux::{Cell, Frame, theme::Theme};
use gtk::{gdk, graphene, gsk, pango, prelude::*};
use renderer::{Renderer, Scene};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Parser)]
struct Options {
    #[arg(long, default_value = "artifacts/renderer-check")]
    output: PathBuf,
    /// Use the selected GTK backend for cache pixels instead of the Cairo oracle.
    #[arg(long, value_enum, default_value_t = CacheBackend::Cairo)]
    cache_renderer: CacheBackend,
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
        let flags = [0, 1, 2, 3, 4, 8, 16, 8 | 16][y];
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
// GTK draws one complete Pango layout per terminal cluster.
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
        for (flag, line_y) in [(8, ch - 2.0), (16, ch / 2.0)] {
            if cell.flags & flag != 0 {
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
        "{name}: retained/fresh mismatch on {}: nodes_equal={nodes_equal}, \
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
        ("unfocused", 1),
        ("bar", 0),
        ("underline", 0),
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
            "unfocused" => scene.focused = false,
            "bar" => scene.frame.cursor_style = 0,
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
            stats.cached_glyphs <= 4096,
            "Glyph cache exceeded its limit"
        );
        if iteration > 12 {
            assert_eq!(stats.cached_glyphs, 4096);
        }
    }
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
                    cells[y * 40] = cell(&format!("{step}"), theme, 8 | 16);
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
        *target = cell(" ", theme, 8 | 16);
    }
    cells[84].flags |= 64;
    cells[85] = cell("H", theme, 32 | 8 | 16);
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
    validate_cache(cache_gpu, &font, &context, &options.output);
    validate_scroll_cache(cache_gpu, &font, &context, &options.output);
    validate_geometry(&gpu, &font, &context);
    std::fs::write(
        options.output.join("checks.json"),
        serde_json::to_string_pretty(&json!({
            "gtk_renderer": gpu.type_().name(), "cache_renderer": cache_gpu.type_().name(),
            "glyph_cases": cases,
            "checks": ["exact cache pixels", "independent layout agreement", "row invalidation", "scroll cache reuse", "bounded glyph cache",
                       "font and context changes", "fractional blocks", "selection and search",
                       "concealed text", "blank and wide decorations", "wide cursor", "background batching", "overlapping fallback order"]
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
