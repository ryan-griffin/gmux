use crate::render::{self, Rect};
use gmux_client::{Cell, Frame, theme::Theme};
use gtk::{gdk, graphene, gsk, pango, prelude::*};
use std::collections::{HashMap, VecDeque};

pub struct Scene<'a> {
    pub frame: Frame,
    pub cells: &'a [Cell],
    pub metrics: (f64, f64),
    pub scale: f64,
    pub origin: (f64, f64),
    pub width: i32,
    pub height: i32,
    pub theme: Theme,
    pub selection: Option<(usize, usize)>,
    pub matches: &'a [u8],
    pub focused: bool,
    pub exited: bool,
}

impl Scene<'_> {
    pub fn cursor_visible(&self) -> bool {
        self.frame.cursor_visible != 0
            && !self.exited
            && self.frame.cursor_x < self.frame.cols
            && self.frame.cursor_y < self.frame.rows
    }

    fn block_cursor(&self) -> bool {
        self.cursor_visible() && self.focused && self.frame.cursor_style == 1
    }

    fn cursor_index(&self) -> usize {
        self.frame.cursor_y as usize * self.frame.cols as usize + self.frame.cursor_x as usize
    }

    fn cursor_width(&self) -> usize {
        // The spacer belongs to the preceding wide cluster. Fill both cells
        // when the cursor is on its head, including their decorations.
        if self.frame.cursor_x + 1 < self.frame.cols
            && self
                .cells
                .get(self.cursor_index() + 1)
                .is_some_and(|cell| cell.flags & 64 != 0)
        {
            2
        } else {
            1
        }
    }

    fn cursor_rect(&self) -> Rect {
        let (cw, ch) = self.metrics;
        (
            self.frame.cursor_x as f64 * cw,
            self.frame.cursor_y as f64 * ch,
            cw * self.cursor_width() as f64,
            ch,
        )
    }

    fn selected(&self, index: usize) -> bool {
        self.selection
            .is_some_and(|(start, end)| index >= start && index <= end)
    }

    fn highlight(&self, index: usize) -> u8 {
        self.matches.get(index).copied().unwrap_or(0)
    }

    fn foreground(&self, index: usize, cell: &Cell) -> ([u8; 3], f32) {
        let under_cursor = self.block_cursor()
            && (self.cursor_index()..self.cursor_index() + self.cursor_width()).contains(&index);
        (
            if under_cursor {
                cell.bg
            } else if self.selected(index) || self.highlight(index) == 2 {
                self.theme.selection_foreground
            } else {
                cell.fg
            },
            if cell.flags & 4 != 0 && !under_cursor {
                0.5
            } else {
                1.0
            },
        )
    }
}

struct GlyphRun {
    font: pango::Font,
    glyphs: Vec<pango::GlyphInfo>,
    ink: (i32, i32),
}

fn shape(cell: &Cell, font: &pango::FontDescription, context: &pango::Context) -> Vec<GlyphRun> {
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
    let mut result = Vec::new();
    let mut iter = layout.iter();
    loop {
        if let Some(run) = iter.run_readonly() {
            let (ink, logical) = iter.run_extents();
            let mut x = logical.x();
            let baseline = iter.run_baseline() + pango::SCALE;
            let glyphs = run
                .glyph_string()
                .glyph_info()
                .iter()
                .map(|info| {
                    let mut info = *info;
                    let geometry = info.geometry_mut();
                    let advance = geometry.width();
                    geometry.set_x_offset(x + geometry.x_offset());
                    geometry.set_y_offset(baseline + geometry.y_offset());
                    // Exact cell offsets prevent fractional advances from
                    // accumulating when several clusters share a text node.
                    geometry.set_width(0);
                    x += advance;
                    info
                })
                .collect();
            result.push(GlyphRun {
                font: run.item().analysis().font(),
                glyphs,
                ink: (ink.x(), ink.x().saturating_add(ink.width())),
            });
        }
        if !iter.next_run() {
            break;
        }
    }
    result
}

const MAX_GLYPHS: usize = 4096;
type GlyphKey = ([u8; 64], u8);

#[derive(Default)]
struct GlyphCache {
    entries: HashMap<GlyphKey, Vec<GlyphRun>>,
    order: VecDeque<GlyphKey>,
}

impl GlyphCache {
    fn get(
        &mut self,
        cell: &Cell,
        font: &pango::FontDescription,
        context: &pango::Context,
    ) -> &[GlyphRun] {
        let key = (cell.text, cell.flags & 3);
        // Evict one cluster at a time instead of clearing the entire cache
        // during Unicode-heavy output. Cached row nodes retain their fonts.
        if self.entries.len() == MAX_GLYPHS && !self.entries.contains_key(&key) {
            self.entries.remove(&self.order.pop_front().unwrap());
        }
        self.entries.entry(key).or_insert_with(|| {
            self.order.push_back(key);
            shape(cell, font, context)
        })
    }
}

#[derive(PartialEq)]
struct FontKey {
    description: pango::FontDescription,
    context: pango::Context,
    serial: u32,
}

#[derive(Default)]
pub struct RenderStats {
    pub rebuilt_rows: usize,
    pub text_nodes: usize,
    pub cached_glyphs: usize,
}

#[derive(PartialEq)]
struct Appearance {
    cols: u16,
    metrics: (f64, f64),
    scale: f64,
    origin: (f64, f64),
    colors: [[u8; 3]; 4],
}

#[derive(Default)]
struct CachedRow {
    cells: Vec<Cell>,
    highlights: Vec<u8>,
    selection: Option<(usize, usize)>,
    cursor: Option<(usize, usize, [u8; 3])>,
    pixel_phase: f64,
    background: Option<gsk::RenderNode>,
    foreground: Option<gsk::RenderNode>,
}

#[derive(Default)]
pub struct Renderer {
    font: Option<FontKey>,
    glyphs: GlyphCache,
    batches: TextBatches,
    appearance: Option<Appearance>,
    rows: Vec<CachedRow>,
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.glyphs = GlyphCache::default();
        self.batches = TextBatches::default();
        self.font = None;
        self.rows.clear();
        self.appearance = None;
    }

    fn reuse_scrolled_rows(&mut self, cells: &[Cell], cols: usize) {
        let rows = self.rows.len();
        if rows < 2 || cols == 0 || cells.len() != rows * cols {
            return;
        }
        let first = &cells[..cols];
        let last = &cells[(rows - 1) * cols..];
        // Verify the overlap before moving caches, so repeated or edited rows
        // cannot turn an ordinary update into a full viewport rebuild.
        if self.rows[0].cells != first
            && let Some(delta) = self.rows[1..]
                .iter()
                .position(|row| row.cells == first)
                .map(|index| index + 1)
            && self.rows[delta..]
                .iter()
                .zip(cells.chunks_exact(cols))
                .all(|(row, cells)| row.cells == cells)
        {
            self.rows.rotate_left(delta);
            return;
        }
        if self.rows[rows - 1].cells != last
            && let Some(index) = self.rows[..rows - 1]
                .iter()
                .rposition(|row| row.cells == last)
        {
            let delta = rows - 1 - index;
            if self.rows[..rows - delta]
                .iter()
                .zip(cells.chunks_exact(cols).skip(delta))
                .all(|(row, cells)| row.cells == cells)
            {
                self.rows.rotate_right(delta);
            }
        }
    }

    pub fn snapshot(
        &mut self,
        snapshot: &gtk::Snapshot,
        scene: &Scene<'_>,
        font: &pango::FontDescription,
        context: &pango::Context,
    ) -> RenderStats {
        if self.font.as_ref().is_none_or(|previous| {
            previous.description != *font
                || previous.context != *context
                || previous.serial != context.serial()
        }) {
            self.clear();
            self.font = Some(FontKey {
                description: font.clone(),
                context: context.clone(),
                serial: context.serial(),
            });
        }
        let appearance = Appearance {
            cols: scene.frame.cols,
            metrics: scene.metrics,
            scale: scene.scale,
            origin: (
                (scene.origin.0 * scene.scale).fract() / scene.scale,
                (scene.origin.1 * scene.scale).fract() / scene.scale,
            ),
            colors: [
                scene.frame.background,
                scene.theme.selection_background,
                scene.theme.selection_foreground,
                scene.theme.palette[11],
            ],
        };
        if self.appearance.as_ref() != Some(&appearance) {
            self.rows.clear();
            self.appearance = Some(appearance);
        }
        self.rows
            .resize_with(scene.frame.rows as usize, CachedRow::default);
        append_rect(
            snapshot,
            scene.frame.background,
            1.0,
            (0.0, 0.0, scene.width as f64, scene.height as f64),
        );
        let cols = scene.frame.cols as usize;
        self.reuse_scrolled_rows(scene.cells, cols);
        let mut stats = RenderStats::default();
        let batch = &mut self.batches;
        for (y, cells) in scene.cells.chunks(cols).enumerate() {
            let base = y * cols;
            let highlights = scene.matches.get(base..base + cells.len()).unwrap_or(&[]);
            let selection = row_selection(scene.selection, base, cells.len());
            let cursor = (scene.block_cursor() && scene.frame.cursor_y as usize == y).then(|| {
                (
                    scene.frame.cursor_x as usize,
                    scene.cursor_width(),
                    scene.frame.cursor,
                )
            });
            let row = &mut self.rows[y];
            // Rectangle snapping depends on the row's position in device pixels.
            let pixel_phase = (y as f64 * scene.metrics.1 * scene.scale).fract();
            if row.cells != cells
                || row.highlights != highlights
                || row.selection != selection
                || row.cursor != cursor
                || row.pixel_phase != pixel_phase
            {
                let background = gtk::Snapshot::new();
                // Adjacent backgrounds share a node, which keeps full-screen
                // applications from creating one color node for every cell.
                let mut x = 0;
                while x < cells.len() {
                    let colors = backgrounds(scene, base + x, &cells[x]);
                    let mut end = x + 1;
                    while end < cells.len() && backgrounds(scene, base + end, &cells[end]) == colors
                    {
                        end += 1;
                    }
                    for (rgb, alpha) in colors.into_iter().flatten() {
                        append_row_rect(
                            &background,
                            scene,
                            y as f64 * scene.metrics.1,
                            rgb,
                            alpha,
                            (
                                x as f64 * scene.metrics.0,
                                0.0,
                                (end - x) as f64 * scene.metrics.0,
                                scene.metrics.1,
                            ),
                        );
                    }
                    x = end;
                }
                if let Some((x, width, rgb)) = cursor {
                    append_row_rect(
                        &background,
                        scene,
                        y as f64 * scene.metrics.1,
                        rgb,
                        1.0,
                        (
                            x as f64 * scene.metrics.0,
                            0.0,
                            width as f64 * scene.metrics.0,
                            scene.metrics.1,
                        ),
                    );
                }
                row.background = background.to_node();
                let row_snapshot = gtk::Snapshot::new();
                for (x, cell) in cells.iter().enumerate() {
                    let text = cell.text();
                    let has_glyph = !text.is_empty() && text != " ";
                    if cell.flags & 32 != 0 || !has_glyph && cell.flags & (8 | 16) == 0 {
                        continue;
                    }
                    let rect = (
                        x as f64 * scene.metrics.0,
                        0.0,
                        scene.metrics.0,
                        scene.metrics.1,
                    );
                    let (rgb, alpha) = scene.foreground(base + x, cell);
                    let mut first_rect = true;
                    let graphic = has_glyph
                        && render::graphic_rects(text, rect, |rect| {
                            // Flush once before graphics to preserve glyph order.
                            if first_rect {
                                stats.text_nodes += batch.flush(&row_snapshot);
                                first_rect = false;
                            }
                            append_row_rect(
                                &row_snapshot,
                                scene,
                                y as f64 * scene.metrics.1,
                                rgb,
                                alpha,
                                rect,
                            )
                        });
                    if has_glyph && !graphic {
                        for run in self.glyphs.get(cell, font, context) {
                            stats.text_nodes += batch.append(
                                &row_snapshot,
                                run,
                                rgba(rgb, alpha),
                                (rect.0 * pango::SCALE as f64) as i32,
                            );
                        }
                    }
                }
                stats.text_nodes += batch.flush(&row_snapshot);
                // Decorations share long runs. Keeping them outside the glyph
                // batch avoids one text node per underlined or struck-out cell.
                for flag in [8, 16] {
                    let mut start = 0;
                    while start < cells.len() {
                        let cell = &cells[start];
                        if cell.flags & flag == 0 || cell.flags & 32 != 0 {
                            start += 1;
                            continue;
                        }
                        let (rgb, alpha) = scene.foreground(base + start, cell);
                        let mut end = start + 1;
                        while end < cells.len()
                            && cells[end].flags & flag != 0
                            && cells[end].flags & 32 == 0
                            && scene.foreground(base + end, &cells[end]) == (rgb, alpha)
                        {
                            end += 1;
                        }
                        let line_y = if flag == 8 {
                            scene.metrics.1 - 2.0
                        } else {
                            scene.metrics.1 / 2.0
                        };
                        append_row_rect(
                            &row_snapshot,
                            scene,
                            y as f64 * scene.metrics.1,
                            rgb,
                            alpha,
                            (
                                start as f64 * scene.metrics.0,
                                line_y,
                                (end - start) as f64 * scene.metrics.0,
                                1.0,
                            ),
                        );
                        start = end;
                    }
                }
                row.foreground = row_snapshot.to_node();
                row.cells.clear();
                row.cells.extend_from_slice(cells);
                row.highlights.clear();
                row.highlights.extend_from_slice(highlights);
                row.selection = selection;
                row.cursor = cursor;
                row.pixel_phase = pixel_phase;
                stats.rebuilt_rows += 1;
            }
            if let Some(node) = &row.background {
                snapshot.save();
                snapshot.translate(&graphene::Point::new(
                    0.0,
                    (y as f64 * scene.metrics.1) as f32,
                ));
                snapshot.append_node(node);
                snapshot.restore();
            }
        }
        // Paint every background before any text, including wide and tall glyphs.
        for (y, row) in self.rows.iter().enumerate() {
            if let Some(node) = &row.foreground {
                snapshot.save();
                snapshot.translate(&graphene::Point::new(
                    0.0,
                    (y as f64 * scene.metrics.1) as f32,
                ));
                snapshot.append_node(node);
                snapshot.restore();
            }
        }
        if scene.cursor_visible() && !scene.block_cursor() {
            let alpha = if scene.focused { 1.0 } else { 0.35 };
            render::cursor_rects(
                scene.frame.cursor_style,
                scene.focused,
                scene.cursor_rect(),
                |rect| {
                    append_row_rect(snapshot, scene, 0.0, scene.frame.cursor, alpha, rect);
                },
            );
        }
        stats.cached_glyphs = self.glyphs.entries.len();
        stats
    }
}

#[derive(Default)]
struct TextBatch {
    font: Option<pango::Font>,
    color: Option<gdk::RGBA>,
    glyphs: Vec<pango::GlyphInfo>,
    right: i32,
}

// Fallback fonts often alternate within a terminal row. Reuse a compatible
// batch only when its new ink cannot overlap any glyph whose order would change.
// Graphics flush all batches, preserving their order relative to text.
#[derive(Default)]
struct TextBatches {
    batches: Vec<TextBatch>,
    used: usize,
}
impl TextBatches {
    fn append(
        &mut self,
        snapshot: &gtk::Snapshot,
        run: &GlyphRun,
        color: gdk::RGBA,
        x: i32,
    ) -> usize {
        let compatible = self.batches[..self.used].iter().rposition(|batch| {
            batch.font.as_ref() == Some(&run.font) && batch.color == Some(color)
        });
        let left = x.saturating_add(run.ink.0);
        let right = x.saturating_add(run.ink.1);
        let mut flushed = 0;
        let index = if let Some(index) = compatible
            && (index + 1 == self.used
                || self.batches[index..self.used]
                    .iter()
                    .all(|batch| batch.right <= left))
        {
            index
        } else {
            // Bound the search even when every cell has a different color.
            if self.used == 8 {
                flushed = self.flush(snapshot);
            }
            if self.used == self.batches.len() {
                self.batches.push(TextBatch::default());
            }
            let batch = &mut self.batches[self.used];
            batch.font = Some(run.font.clone());
            batch.color = Some(color);
            batch.right = i32::MIN;
            self.used += 1;
            self.used - 1
        };
        let batch = &mut self.batches[index];
        batch.right = batch.right.max(right);
        batch.glyphs.extend(run.glyphs.iter().map(|info| {
            let mut info = *info;
            let geometry = info.geometry_mut();
            geometry.set_x_offset(geometry.x_offset() + x);
            info
        }));
        flushed
    }

    fn flush(&mut self, snapshot: &gtk::Snapshot) -> usize {
        let mut nodes = 0;
        for batch in &mut self.batches[..self.used] {
            nodes += batch.flush(snapshot);
        }
        self.used = 0;
        nodes
    }
}

impl TextBatch {
    fn flush(&mut self, snapshot: &gtk::Snapshot) -> usize {
        if self.glyphs.is_empty() {
            return 0;
        }
        let mut glyphs = pango::GlyphString::new();
        glyphs.set_size(self.glyphs.len() as i32);
        glyphs.glyph_info_mut().clone_from_slice(&self.glyphs);
        glyphs.log_clusters_mut().fill(0);
        let mut color = self.color.unwrap();
        let alpha = color.alpha();
        color.set_alpha(1.0);
        let node = gsk::TextNode::new(
            self.font.as_ref().unwrap(),
            &glyphs,
            &color,
            &graphene::Point::new(0.0, 0.0),
        );
        self.glyphs.clear();
        if let Some(node) = node {
            // Color emoji ignore a text node's foreground color, including
            // its alpha. Applying opacity to the node also dims those glyphs.
            if alpha < 1.0 {
                snapshot.push_opacity(alpha as f64);
            }
            snapshot.append_node(node);
            if alpha < 1.0 {
                snapshot.pop();
            }
            1
        } else {
            0
        }
    }
}

fn row_selection(
    selection: Option<(usize, usize)>,
    base: usize,
    len: usize,
) -> Option<(usize, usize)> {
    selection.and_then(|(start, end)| {
        (start < base + len && end >= base).then(|| {
            (
                start.saturating_sub(base),
                end.saturating_sub(base).min(len - 1),
            )
        })
    })
}

fn backgrounds(scene: &Scene<'_>, index: usize, cell: &Cell) -> [Option<([u8; 3], f32)>; 2] {
    if scene.selected(index) || scene.highlight(index) == 2 {
        [Some((scene.theme.selection_background, 1.0)), None]
    } else if scene.highlight(index) == 1 {
        [Some((cell.bg, 1.0)), Some((scene.theme.palette[11], 0.3))]
    } else if cell.bg != scene.frame.background {
        [Some((cell.bg, 1.0)), None]
    } else {
        [None, None]
    }
}

fn rgba(rgb: [u8; 3], alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
        alpha,
    )
}

fn append_rect(snapshot: &gtk::Snapshot, rgb: [u8; 3], alpha: f32, (x, y, w, h): Rect) {
    if w > 0.0 && h > 0.0 {
        snapshot.append_color(
            &rgba(rgb, alpha),
            &graphene::Rect::new(x as f32, y as f32, w as f32, h as f32),
        );
    }
}

// Snap shared rectangle boundaries in surface device coordinates. Snapping
// locally would leave seams when a row or pane starts between device pixels.
fn append_row_rect(
    snapshot: &gtk::Snapshot,
    scene: &Scene<'_>,
    row_y: f64,
    rgb: [u8; 3],
    alpha: f32,
    (x, y, w, h): Rect,
) {
    let snap = |position: f64, origin: f64| {
        ((position + origin) * scene.scale).round() / scene.scale - origin
    };
    let left = snap(x, scene.origin.0);
    let right = snap(x + w, scene.origin.0);
    let top = snap(y, scene.origin.1 + row_y);
    let bottom = snap(y + h, scene.origin.1 + row_y);
    append_rect(
        snapshot,
        rgb,
        alpha,
        (left, top, right - left, bottom - top),
    );
}
