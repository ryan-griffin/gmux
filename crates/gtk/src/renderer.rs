use crate::render::{self, Rect};
use gmux_protocol::{
    Cell, Frame,
    cells::{decoration, flags, underline},
    theme::Theme,
};
use gtk::{gdk, graphene, gsk, pango, prelude::*};
use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
};

pub struct Scene<'a> {
    pub frame: Frame,
    pub cells: &'a [Cell],
    pub metrics: (f64, f64),
    pub scale: f64,
    pub origin: (f64, f64),
    pub width: i32,
    pub height: i32,
    pub theme: Theme,
    pub selection: Option<gmux_protocol::selection::Selection>,
    /// Half-open range from the hyperlink map captured with this frame.
    pub hovered_link: Option<(usize, usize)>,
    pub matches: &'a [u8],
    pub focused: bool,
    pub exited: bool,
    pub text_blink_visible: bool,
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

    fn highlight(&self, index: usize) -> u8 {
        self.matches.get(index).copied().unwrap_or(0)
    }
}

/// Shape and wide edges are resolved once per row. Color lookups share this
/// interval without copying the scene or calculating geometry for each cell.
struct RowColors<'a, 'b> {
    scene: &'a Scene<'b>,
    selection: Option<(usize, usize)>,
}

impl RowColors<'_, '_> {
    fn selected(&self, index: usize) -> bool {
        self.selection
            .is_some_and(|(start, end)| (start..=end).contains(&index))
    }

    fn foreground(&self, index: usize, cell: &Cell) -> ([u8; 3], f32) {
        let scene = self.scene;
        let under_cursor = scene.block_cursor()
            && (scene.cursor_index()..scene.cursor_index() + scene.cursor_width()).contains(&index);
        (
            if under_cursor {
                cell.bg
            } else if self.selected(index) || scene.highlight(index) == 2 {
                scene.theme.selection_foreground
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

    fn backgrounds(&self, index: usize, cell: &Cell) -> [Option<([u8; 3], f32)>; 2] {
        let scene = self.scene;
        if self.selected(index) || scene.highlight(index) == 2 {
            [Some((scene.theme.selection_background, 1.0)), None]
        } else if scene.highlight(index) == 1 {
            [Some((cell.bg, 1.0)), Some((scene.theme.palette[11], 0.3))]
        } else if cell.bg != scene.frame.background {
            [Some((cell.bg, 1.0)), None]
        } else {
            [None, None]
        }
    }
}

struct GlyphRun {
    font: pango::Font,
    glyphs: Vec<pango::GlyphInfo>,
    ink: (i32, i32),
}

fn shape(
    text: &str,
    flags: u8,
    cell_width: f64,
    font: &pango::FontDescription,
    context: &pango::Context,
) -> Vec<GlyphRun> {
    let layout = pango::Layout::new(context);
    let mut font = font.clone();
    if flags & 1 != 0 {
        font.set_weight(pango::Weight::Bold);
    }
    if flags & 2 != 0 {
        font.set_style(pango::Style::Italic);
    }
    layout.set_font_description(Some(&font));
    layout.set_text(text);
    let ascii = text.is_ascii();
    let mut result = Vec::with_capacity(1);
    let mut iter = layout.iter();
    loop {
        if let Some(run) = iter.run_readonly() {
            let item = run.item();
            // ASCII clusters move to grid positions, so their old extents are
            // unnecessary. Unicode clusters preserve their original geometry.
            let extents = (!ascii).then(|| iter.run_extents());
            let mut x = extents.as_ref().map_or(0, |(_, logical)| logical.x());
            let baseline = iter.run_baseline() + pango::SCALE;
            let mut glyphs = run.glyph_string();
            let mut cluster = -1;
            for index in 0..glyphs.num_glyphs() as usize {
                let next_cluster = glyphs.log_clusters()[index];
                if ascii && next_cluster != cluster {
                    // Anchor each shaped cluster to its terminal column. A
                    // ligature can span cells; glyphs within it keep their
                    // offsets and advances. Kerning cannot shift later cells.
                    x = ((item.offset() + next_cluster) as f64 * cell_width * pango::SCALE as f64)
                        as i32;
                    cluster = next_cluster;
                }
                let geometry = glyphs.glyph_info_mut()[index].geometry_mut();
                let advance = geometry.width();
                geometry.set_x_offset(x + geometry.x_offset());
                geometry.set_y_offset(baseline + geometry.y_offset());
                geometry.set_width(0);
                x += advance;
            }
            let run_font = item.analysis().font();
            let ink = extents.map_or_else(|| glyphs.extents(&run_font).0, |(ink, _)| ink);
            result.push(GlyphRun {
                font: run_font,
                glyphs: glyphs.glyph_info().to_vec(),
                ink: (ink.x(), ink.x().saturating_add(ink.width())),
            });
        }
        if !iter.next_run() {
            break;
        }
    }
    result
}

const MAX_SHAPES: usize = 4096;
const MAX_SHAPE_BYTES: usize = 1024 * 1024;

struct CachedShape {
    runs: Rc<[GlyphRun]>,
    bytes: usize,
}

#[derive(Default)]
struct ShapeCache {
    entries: [HashMap<Rc<str>, CachedShape>; 4],
    order: VecDeque<(Rc<str>, usize)>,
    bytes: usize,
}

impl ShapeCache {
    fn get(
        &mut self,
        text: &str,
        flags: u8,
        cell_width: f64,
        font: &pango::FontDescription,
        context: &pango::Context,
    ) -> Rc<[GlyphRun]> {
        let style = (flags & 3) as usize;
        if let Some(cached) = self.entries[style].get(text) {
            return cached.runs.clone();
        }
        let runs: Rc<[GlyphRun]> = shape(text, flags, cell_width, font, context).into();
        // Bound variable-length keys and glyph arrays as well as entry
        // count. Font objects are shared with Pango and retained row nodes.
        let bytes = text.len()
            + 2 * size_of::<usize>()
            + size_of::<(Rc<str>, CachedShape)>()
            + size_of::<(Rc<str>, usize)>()
            + 2 * size_of::<usize>()
            + runs.len() * size_of::<GlyphRun>()
            + runs
                .iter()
                .map(|run| run.glyphs.capacity() * size_of::<pango::GlyphInfo>())
                .sum::<usize>();
        if bytes > MAX_SHAPE_BYTES {
            return runs;
        }
        while self.order.len() == MAX_SHAPES || self.bytes + bytes > MAX_SHAPE_BYTES {
            let (key, style) = self.order.pop_front().unwrap();
            self.bytes -= self.entries[style].remove(&key).unwrap().bytes;
        }
        let key = Rc::<str>::from(text);
        self.order.push_back((key.clone(), style));
        self.entries[style].insert(
            key,
            CachedShape {
                runs: runs.clone(),
                bytes,
            },
        );
        self.bytes += bytes;
        runs
    }
}

#[derive(PartialEq)]
struct FontKey {
    description: pango::FontDescription,
    context: pango::Context,
    serial: u32,
    cell_width: f64,
}

#[derive(Default)]
pub struct RenderStats {
    pub rebuilt_rows: usize,
    pub background_rows: usize,
    pub text_nodes: usize,
    pub cached_shapes: usize,
    pub cached_shape_bytes: usize,
    pub blinking_rows: usize,
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
    hovered_link: Option<(u16, u16)>,
    pixel_phase: f64,
    background: Option<gsk::RenderNode>,
    foreground: Option<gsk::RenderNode>,
    hidden_background: Option<gsk::RenderNode>,
    hidden_foreground: Option<gsk::RenderNode>,
    blink: bool,
    blink_start: Option<u16>,
}

/// Retain both phases with shared nodes. Segment boundaries preserve paint
/// order even when italic/fallback ink overlaps adjacent steady and blink cells.
/// Ordinary rows use just one snapshot, with no extra nodes or snapshots.
struct RowPhases {
    current: gtk::Snapshot,
    phases: Option<(gtk::Snapshot, gtk::Snapshot)>,
    blinking: bool,
}

impl RowPhases {
    fn new(blink: bool) -> Self {
        Self::with_snapshot(gtk::Snapshot::new(), blink)
    }

    fn with_snapshot(current: gtk::Snapshot, blink: bool) -> Self {
        Self {
            current,
            phases: blink.then(|| (gtk::Snapshot::new(), gtk::Snapshot::new())),
            blinking: false,
        }
    }

    fn append_segment(
        current: gtk::Snapshot,
        (shown, hidden): &(gtk::Snapshot, gtk::Snapshot),
        blinking: bool,
    ) {
        if let Some(node) = current.to_node() {
            shown.append_node(&node);
            if !blinking {
                hidden.append_node(&node);
            }
        }
    }

    fn set_blink(&mut self, blink: bool) {
        if let Some(phases) = &self.phases
            && self.blinking != blink
        {
            let current = std::mem::replace(&mut self.current, gtk::Snapshot::new());
            Self::append_segment(current, phases, self.blinking);
            self.blinking = blink;
        }
    }

    fn finish(self) -> (Option<gsk::RenderNode>, Option<gsk::RenderNode>) {
        if let Some(phases) = self.phases {
            Self::append_segment(self.current, &phases, self.blinking);
            (phases.0.to_node(), phases.1.to_node())
        } else {
            (self.current.to_node(), None)
        }
    }
}

#[derive(Default)]
pub struct Renderer {
    font: Option<FontKey>,
    shapes: ShapeCache,
    text: String,
    batches: TextBatches,
    appearance: Option<Appearance>,
    rows: Vec<CachedRow>,
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.shapes = ShapeCache::default();
        self.text = String::new();
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
                || previous.cell_width != scene.metrics.0
        }) {
            self.clear();
            self.font = Some(FontKey {
                description: font.clone(),
                context: context.clone(),
                serial: context.serial(),
                cell_width: scene.metrics.0,
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
            let selection = scene
                .selection
                .and_then(|selection| selection.row(base, cells, scene.frame.cols));
            let colors = RowColors {
                scene,
                selection: selection.map(|(start, end)| (base + start, base + end)),
            };
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
            let rebuild = row.cells != cells
                || row.highlights != highlights
                || row.selection != selection
                || row.cursor != cursor
                || row.pixel_phase != pixel_phase;
            let hovered_link = scene.hovered_link.and_then(|(start, end)| {
                (start < base + cells.len() && end > base).then(|| {
                    (
                        // Both row-local endpoints fit Frame::cols, including
                        // the half-open end at the maximum 65535 columns.
                        start.saturating_sub(base) as u16,
                        end.saturating_sub(base).min(cells.len()) as u16,
                    )
                })
            });
            let mut flags = 0;
            if rebuild || row.hovered_link != hovered_link {
                let background = gtk::Snapshot::new();
                // Adjacent backgrounds share a node, which keeps full-screen
                // applications from creating one color node for every cell.
                let mut decorations = 0;
                let mut x = 0;
                while x < cells.len() {
                    decorations |= cells[x].decoration;
                    flags |= cells[x].flags;
                    let backgrounds = colors.backgrounds(base + x, &cells[x]);
                    let mut end = x + 1;
                    while end < cells.len()
                        && colors.backgrounds(base + end, &cells[end]) == backgrounds
                    {
                        decorations |= cells[end].decoration;
                        flags |= cells[end].flags;
                        end += 1;
                    }
                    for (rgb, alpha) in backgrounds.into_iter().flatten() {
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
                // Reuse the background scan for the ordinary no-blink path.
                // Empty and concealed blink cells cannot change visible ink.
                row.blink = flags & flags::BLINK != 0;
                row.blink_start = row
                    .blink
                    .then(|| {
                        cells.iter().enumerate().position(|(x, cell)| {
                            cell.blink()
                                && cell.flags & 32 == 0
                                && ((!cell.text().is_empty() && cell.text() != " ")
                                    || cell.underline() != underline::NONE
                                    || cell.overline()
                                    || cell.flags & 16 != 0
                                    || hovered_link.is_some_and(|(start, end)| {
                                        (start as usize..end as usize).contains(&x)
                                    }))
                        })
                    })
                    .flatten()
                    // Row-local columns are bounded by Frame::cols.
                    .map(|x| x as u16);
                let mut background = RowPhases::with_snapshot(background, row.blink);
                // Ghostty paints underlines and overlines below glyphs so
                // contrasting colors cannot cover descenders or tall ink.
                if decorations & decoration::UNDERLINE_MASK != 0 || hovered_link.is_some() {
                    append_decorations(
                        &mut background,
                        &colors,
                        cells,
                        base,
                        y,
                        DecorationKind::Underline,
                    );
                }
                if decorations & decoration::OVERLINE != 0 {
                    append_decorations(
                        &mut background,
                        &colors,
                        cells,
                        base,
                        y,
                        DecorationKind::Overline,
                    );
                }
                (row.background, row.hidden_background) = background.finish();
                row.hovered_link = hovered_link;
                stats.background_rows += 1;
            }
            if rebuild {
                let mut foreground = RowPhases::new(row.blink);
                let mut x = 0;
                while x < cells.len() {
                    let cell = &cells[x];
                    let text = cell.text();
                    let has_glyph = !text.is_empty() && text != " ";
                    if cell.flags & 32 != 0 || !has_glyph {
                        x += 1;
                        continue;
                    }
                    let rect = (
                        x as f64 * scene.metrics.0,
                        0.0,
                        scene.metrics.0,
                        scene.metrics.1,
                    );
                    let (rgb, alpha) = colors.foreground(base + x, cell);
                    let mut first_rect = true;
                    let graphic = has_glyph
                        && render::graphic_rects(text, rect, |rect| {
                            // Flush once before graphics to preserve glyph order.
                            if first_rect {
                                stats.text_nodes += batch.flush(&mut foreground);
                                foreground.set_blink(cell.blink());
                                first_rect = false;
                            }
                            append_row_rect(
                                &foreground.current,
                                scene,
                                y as f64 * scene.metrics.1,
                                rgb,
                                alpha,
                                rect,
                            )
                        });
                    if has_glyph && !graphic {
                        let mut end = x + 1;
                        let text = if ascii_cell(cell) {
                            self.text.clear();
                            self.text.push(cell.text().as_bytes()[0] as char);
                            // Keep selection and block cursors as shaping
                            // boundaries even when foreground colors match.
                            let mut limit = cells.len();
                            for (start, last) in [
                                selection,
                                cursor.map(|(start, width, _)| (start, start + width - 1)),
                            ]
                            .into_iter()
                            .flatten()
                            {
                                if x < start {
                                    limit = limit.min(start);
                                } else if x <= last {
                                    limit = limit.min(last + 1);
                                }
                            }
                            // Whitespace separates reusable words and operator
                            // sequences. Backgrounds paint separately and do not
                            // split shaping. Unicode clusters keep their own
                            // layout, baseline, and terminal-defined visual order.
                            while end < limit
                                && ascii_cell(&cells[end])
                                && cells[end].flags & (1 | 2 | 4 | flags::BLINK)
                                    == cell.flags & (1 | 2 | 4 | flags::BLINK)
                                && colors.foreground(base + end, &cells[end]) == (rgb, alpha)
                            {
                                self.text.push(cells[end].text().as_bytes()[0] as char);
                                end += 1;
                            }
                            &self.text
                        } else {
                            text
                        };
                        for run in self
                            .shapes
                            .get(text, cell.flags, scene.metrics.0, font, context)
                            .iter()
                        {
                            stats.text_nodes += batch.append(
                                &mut foreground,
                                run,
                                rgba(rgb, alpha),
                                (rect.0 * pango::SCALE as f64) as i32,
                                cell.blink(),
                            );
                        }
                        x = end;
                    } else {
                        x += 1;
                    }
                }
                stats.text_nodes += batch.flush(&mut foreground);
                // Strikethrough stays above the glyphs, matching Ghostty.
                if flags & 16 != 0 {
                    append_decorations(
                        &mut foreground,
                        &colors,
                        cells,
                        base,
                        y,
                        DecorationKind::Strike,
                    );
                }
                (row.foreground, row.hidden_foreground) = foreground.finish();
                row.cells.clear();
                row.cells.extend_from_slice(cells);
                row.highlights.clear();
                row.highlights.extend_from_slice(highlights);
                row.selection = selection;
                row.cursor = cursor;
                row.pixel_phase = pixel_phase;
                stats.rebuilt_rows += 1;
            }
            if row
                .blink_start
                .is_some_and(|x| x as f64 * scene.metrics.0 < scene.width as f64)
                && (y as f64 * scene.metrics.1) < scene.height as f64
            {
                stats.blinking_rows += 1;
            }
            if let Some(node) = if row.blink && !scene.text_blink_visible {
                &row.hidden_background
            } else {
                &row.background
            } {
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
            if let Some(node) = if row.blink && !scene.text_blink_visible {
                &row.hidden_foreground
            } else {
                &row.foreground
            } {
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
        stats.cached_shapes = self.shapes.order.len();
        stats.cached_shape_bytes = self.shapes.bytes;
        stats
    }
}

fn ascii_cell(cell: &Cell) -> bool {
    cell.flags & (32 | 64) == 0 && matches!(cell.text().as_bytes(), [b'!'..=b'~'])
}

#[derive(Default)]
struct TextBatch {
    font: Option<pango::Font>,
    color: Option<gdk::RGBA>,
    glyphs: Vec<pango::GlyphInfo>,
    right: i32,
    blink: bool,
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
        phases: &mut RowPhases,
        run: &GlyphRun,
        color: gdk::RGBA,
        x: i32,
        blink: bool,
    ) -> usize {
        let compatible = self.batches[..self.used].iter().rposition(|batch| {
            batch.font.as_ref() == Some(&run.font)
                && batch.color == Some(color)
                && batch.blink == blink
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
                flushed = self.flush(phases);
            }
            if self.used == self.batches.len() {
                self.batches.push(TextBatch::default());
            }
            let batch = &mut self.batches[self.used];
            batch.font = Some(run.font.clone());
            batch.color = Some(color);
            batch.right = i32::MIN;
            batch.blink = blink;
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

    fn flush(&mut self, phases: &mut RowPhases) -> usize {
        let mut nodes = 0;
        for batch in &mut self.batches[..self.used] {
            phases.set_blink(batch.blink);
            nodes += batch.flush(&phases.current);
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
    append_rect(
        snapshot,
        rgb,
        alpha,
        snapped_row_rect(scene, row_y, (x, y, w, h)),
    );
}

fn snapped_row_rect(scene: &Scene<'_>, row_y: f64, (x, y, w, h): Rect) -> Rect {
    let snap = |position: f64, origin: f64| {
        ((position + origin) * scene.scale).round() / scene.scale - origin
    };
    let left = snap(x, scene.origin.0);
    let right = snap(x + w, scene.origin.0);
    let top = snap(y, scene.origin.1 + row_y);
    let bottom = snap(y + h, scene.origin.1 + row_y);
    (left, top, right - left, bottom - top)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DecorationKind {
    Underline,
    Strike,
    Overline,
}

// Explicit underline colors remain
// independent of selection and cursor text colors, including black.
fn decoration(
    colors: &RowColors<'_, '_>,
    index: usize,
    cell: &Cell,
    kind: DecorationKind,
) -> Option<(u8, [u8; 3], f32)> {
    if cell.flags & 32 != 0 {
        return None;
    }
    let scene = colors.scene;
    let style = match kind {
        DecorationKind::Underline => {
            if scene
                .hovered_link
                .is_some_and(|(start, end)| (start..end).contains(&index))
            {
                // Pinned Ghostty uses a double line only for an existing single
                // underline. Other styles become single while the link is active.
                if cell.underline() == underline::SINGLE {
                    underline::DOUBLE
                } else {
                    underline::SINGLE
                }
            } else {
                cell.underline()
            }
        }
        DecorationKind::Strike => u8::from(cell.flags & 16 != 0),
        DecorationKind::Overline => u8::from(cell.overline()),
    };
    if style == underline::NONE || style > underline::DASHED {
        return None;
    }
    let (mut rgb, alpha) = colors.foreground(index, cell);
    if kind == DecorationKind::Underline && cell.has_underline_color() {
        rgb = cell.underline_color;
    }
    Some((style, rgb, alpha))
}

fn append_decoration(
    snapshot: &gtk::Snapshot,
    scene: &Scene<'_>,
    row: usize,
    start: usize,
    end: usize,
    kind: DecorationKind,
    (style, rgb, alpha): (u8, [u8; 3], f32),
) {
    let (cw, ch) = scene.metrics;
    let row_y = row as f64 * ch;
    let x = start as f64 * cw;
    let width = (end - start) as f64 * cw;
    let line_y = match kind {
        DecorationKind::Underline => ch - 2.0,
        DecorationKind::Strike => ch / 2.0,
        DecorationKind::Overline => 0.0,
    };
    let line = |left, top, width, height| {
        append_row_rect(
            snapshot,
            scene,
            row_y,
            rgb,
            alpha,
            (left, top, width, height),
        );
    };
    if kind != DecorationKind::Underline || style == underline::SINGLE {
        line(x, line_y, width, 1.0);
    } else if style == underline::DOUBLE {
        line(x, ch - 4.0, width, 1.0);
        line(x, line_y, width, 1.0);
    } else if style == underline::CURLY {
        // One smooth path per run. Clip at the run bounds so a color or
        // selection change cannot paint over the neighboring decoration.
        // Anchor the wave to columns so its phase survives run boundaries.
        let path = gsk::PathBuilder::new();
        let period = cw.max(4.0);
        let mut left = (x / period).floor() * period;
        let center = ch - 2.5;
        path.move_to(left as f32, center as f32);
        while left < x + width {
            path.cubic_to(
                (left + period / 4.0) as f32,
                (center - 2.0) as f32,
                (left + period / 4.0) as f32,
                (center - 2.0) as f32,
                (left + period / 2.0) as f32,
                center as f32,
            );
            path.cubic_to(
                (left + period * 0.75) as f32,
                (center + 2.0) as f32,
                (left + period * 0.75) as f32,
                (center + 2.0) as f32,
                (left + period) as f32,
                center as f32,
            );
            left += period;
        }
        snapshot.push_clip(&graphene::Rect::new(x as f32, 0.0, width as f32, ch as f32));
        snapshot.append_stroke(&path.to_path(), &gsk::Stroke::new(1.0), &rgba(rgb, alpha));
        snapshot.pop();
    } else {
        // Device-aligned dots and dashes stay crisp at fractional pane origins.
        let unit = if style == underline::DOTTED { 1.0 } else { 3.0 };
        let period = unit * 2.0;
        // Repetition must preserve the exact device-pixel rounding phase.
        // Use a short binary scale period, rather than approximating arbitrary
        // scales. Negative origins can cross round()'s asymmetric zero boundary.
        let cycles = if alpha == 1.0 && scene.origin.0 >= 0.0 {
            [1, 2, 4, 8, 16, 32]
                .into_iter()
                .find(|&n| (scene.scale * n as f64).fract() == 0.0)
                .map(|n| (n / 2).max(1))
        } else {
            None
        };
        let tile_width = cycles.map(|n| n as f64 * period);
        let repeat = tile_width.filter(|&tile| width > tile * 2.0);
        if let Some(tile) = repeat {
            let (left, top, width, height) =
                snapped_row_rect(scene, row_y, (x, line_y, width, 1.0));
            let (tile_x, tile_y, tile_w, tile_h) =
                snapped_row_rect(scene, row_y, (0.0, line_y, tile, 1.0));
            snapshot.push_repeat(
                &graphene::Rect::new(left as f32, top as f32, width as f32, height as f32),
                Some(&graphene::Rect::new(
                    tile_x as f32,
                    tile_y as f32,
                    tile_w as f32,
                    tile_h as f32,
                )),
            );
            let mut left = 0.0;
            while left < tile {
                append_row_rect(snapshot, scene, row_y, rgb, 1.0, (left, line_y, unit, 1.0));
                left += period;
            }
            snapshot.pop();
        } else {
            // Faint repeats would blend through an intermediate texture. Retain
            // rectangles for identical opacity, and for short/non-periodic runs.
            let mut left = (x / period).floor() * period;
            while left < x + width {
                let begin = left.max(x);
                let last = (left + unit).min(x + width);
                if last > begin {
                    line(begin, line_y, last - begin, 1.0);
                }
                left += period;
            }
        }
    }
}

fn append_decorations(
    phases: &mut RowPhases,
    colors: &RowColors<'_, '_>,
    cells: &[Cell],
    base: usize,
    y: usize,
    kind: DecorationKind,
) {
    let mut start = 0;
    while start < cells.len() {
        let Some(style) = decoration(colors, base + start, &cells[start], kind) else {
            start += 1;
            continue;
        };
        let mut end = start + 1;
        while end < cells.len()
            && decoration(colors, base + end, &cells[end], kind) == Some(style)
            && cells[end].blink() == cells[start].blink()
        {
            end += 1;
        }
        phases.set_blink(cells[start].blink());
        append_decoration(&phases.current, colors.scene, y, start, end, kind, style);
        start = end;
    }
}
