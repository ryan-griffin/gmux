#[path = "renderer/glyphs.rs"]
mod glyphs;
#[path = "renderer/graphics.rs"]
mod graphics;

use crate::{
    bidi,
    render::{self, Rect},
};
use gmux_protocol::{
    Cell, Frame,
    cells::{decoration, flags, underline},
    glyphs::{Glyphs, Vertical, cell_codepoint},
    graphics::{Graphics, Layer},
    theme::Theme,
};
use gtk::{gdk, graphene, gsk, pango, prelude::*};
use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::Arc,
};

pub struct Scene<'a> {
    pub frame: Frame,
    pub cells: &'a [Cell],
    pub graphics: Option<&'a Graphics>,
    pub bidi: Option<&'a bidi::Grid>,
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

    pub fn cursor_column(&self) -> usize {
        let x = self.frame.cursor_x as usize;
        self.bidi
            .and_then(|grid| grid.row(self.frame.cursor_y as usize))
            .map_or(x, |row| row.visual(x))
    }

    fn cursor_rect(&self) -> Rect {
        let (cw, ch) = self.metrics;
        let column = self.cursor_column();
        let width = cw * self.cursor_width() as f64;
        let mut x = column as f64 * cw;
        if self.frame.cursor_style == 0
            && !self.frame.cursor_password_input
            && self
                .bidi
                .and_then(|grid| grid.row(self.frame.cursor_y as usize))
                .is_some_and(|row| row.rtl(column))
        {
            x += (width - 2.0).max(0.0);
        }
        (x, self.frame.cursor_y as f64 * ch, width, ch)
    }

    fn highlight(&self, index: usize) -> u8 {
        self.matches.get(index).copied().unwrap_or(0)
    }
}

/// Color lookups use the selection and cursor spans resolved for the row.
struct RowColors<'a, 'b> {
    scene: &'a Scene<'b>,
    bidi: Option<&'a gmux_protocol::bidi::Row>,
    base: usize,
    selection: Option<(usize, usize)>,
    cursor: Option<std::ops::Range<usize>>,
    explicit_backgrounds: bool,
}

impl RowColors<'_, '_> {
    fn logical(&self, visual: usize) -> usize {
        self.bidi
            .map_or(visual, |row| self.base + row.logical(visual - self.base))
    }

    fn selected(&self, logical: usize) -> bool {
        self.selection
            .is_some_and(|(start, end)| (start..=end).contains(&logical))
    }

    fn foreground(&self, index: usize, cell: &Cell) -> ([u8; 3], f32) {
        let scene = self.scene;
        let under_cursor = self
            .cursor
            .as_ref()
            .is_some_and(|span| span.contains(&index));
        let index = self.logical(index);
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
        let index = self.logical(index);
        if self.selected(index) || scene.highlight(index) == 2 {
            [Some((scene.theme.selection_background, 1.0)), None]
        } else if scene.highlight(index) == 1 {
            [Some((cell.bg, 1.0)), Some((scene.theme.palette[11], 0.3))]
        } else if cell.bg != scene.frame.background
            || (self.explicit_backgrounds && cell.decoration & decoration::EXPLICIT_BACKGROUND != 0)
        {
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
    below_glyphs: Option<gsk::RenderNode>,
    hidden_below_glyphs: Option<gsk::RenderNode>,
    foreground: Option<gsk::RenderNode>,
    hidden_foreground: Option<gsk::RenderNode>,
    blink: bool,
    blink_start: Option<u16>,
    glyphs_changed: bool,
    bidi: bool,
}

impl CachedRow {
    fn below_glyphs(&self, blink_visible: bool) -> Option<&gsk::RenderNode> {
        if self.blink && !blink_visible {
            self.hidden_below_glyphs.as_ref()
        } else {
            self.below_glyphs.as_ref()
        }
    }
}

fn append_cached_row(snapshot: &gtk::Snapshot, node: Option<&gsk::RenderNode>, y: f64) {
    if let Some(node) = node {
        snapshot.save();
        snapshot.translate(&graphene::Point::new(0.0, y as f32));
        snapshot.append_node(node);
        snapshot.restore();
    }
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
    rtl: RtlScratch,
    visual_cells: Vec<Cell>,
    batches: TextBatches,
    appearance: Option<Appearance>,
    rows: Vec<CachedRow>,
    glyphs: Glyphs,
    textures: graphics::Textures,
    image_layers: [bool; 2],
    glyph_paths: HashMap<char, glyphs::GlyphPath>,
    glyph_rasters: Option<glyphs::GlyphRasters>,
    baseline: Option<f64>,
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.shapes = ShapeCache::default();
        self.text = String::new();
        self.rtl = RtlScratch::default();
        self.visual_cells = Vec::new();
        self.batches = TextBatches::default();
        self.font = None;
        self.baseline = None;
        if let Some(rasters) = self.glyph_rasters.as_mut() {
            *rasters = Default::default();
        }
        self.rows.clear();
        self.clear_graphics();
        self.appearance = None;
    }

    pub fn clear_graphics(&mut self) {
        self.textures = Default::default();
    }

    pub fn set_glyphs(&mut self, glyphs: &Glyphs) {
        if self.glyphs.same(glyphs) {
            return;
        }
        let changed = |cp| match (self.glyphs.get(cp), glyphs.get(cp)) {
            (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
            (None, None) => false,
            _ => true,
        };
        for row in &mut self.rows {
            row.glyphs_changed |= row
                .cells
                .iter()
                .any(|cell| cell_codepoint(cell.text()).is_some_and(changed));
        }
        self.glyph_paths.retain(|cp, _| !changed(*cp));
        if let Some(rasters) = self.glyph_rasters.as_mut() {
            rasters.retain(|cp| !changed(cp));
        }
        self.glyphs = glyphs.clone();
    }

    pub fn set_cairo_glyphs(&mut self, enabled: bool) {
        if enabled != self.glyph_rasters.is_some() {
            self.glyph_rasters = enabled.then(glyphs::GlyphRasters::default);
            self.rows.clear();
        }
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
            if let Some(rasters) = self.glyph_rasters.as_mut() {
                *rasters = Default::default();
            }
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
        if let Some(graphics) = scene.graphics {
            self.textures.retain(graphics);
        }
        let graphics = scene
            .graphics
            .filter(|graphics| !graphics.placements.is_empty());
        let [below_background, below_text] = graphics.map_or([false, false], |graphics| {
            [
                graphics.placements[0].layer == Layer::BelowBackground,
                graphics
                    .placements
                    .binary_search_by_key(&Layer::BelowText, |placement| placement.layer)
                    .is_ok(),
            ]
        });
        let layers = [below_background, below_text];
        let image_layers_changed = self.image_layers != layers;
        self.image_layers = layers;
        if let Some(graphics) = graphics {
            self.textures
                .append(snapshot, scene, graphics, Layer::BelowBackground);
        }
        let cols = scene.frame.cols as usize;
        let cursor_visible = scene.cursor_visible();
        let block_cursor = (cursor_visible
            && scene.focused
            && scene.frame.cursor_style == 1
            && !scene.frame.cursor_password_input)
            .then(|| {
                (
                    scene.cursor_column(),
                    scene.cursor_width(),
                    scene.frame.cursor,
                )
            });
        self.reuse_scrolled_rows(scene.cells, cols);
        let mut stats = RenderStats::default();
        let batch = &mut self.batches;
        for (y, logical_cells) in scene.cells.chunks(cols).enumerate() {
            let base = y * cols;
            let bidi = scene.bidi.and_then(|grid| grid.row(y));
            let highlights = scene
                .matches
                .get(base..base + logical_cells.len())
                .unwrap_or(&[]);
            let selection = scene
                .selection
                .and_then(|selection| selection.row(base, logical_cells, scene.frame.cols));
            let cursor = block_cursor.filter(|_| scene.frame.cursor_y as usize == y);
            let colors = RowColors {
                scene,
                bidi,
                base,
                selection: selection.map(|(start, end)| (base + start, base + end)),
                cursor: cursor.map(|(start, width, _)| base + start..base + start + width),
                explicit_backgrounds: below_background,
            };
            let row = &mut self.rows[y];
            // Rectangle snapping depends on the row's position in device pixels.
            let pixel_phase = (y as f64 * scene.metrics.1 * scene.scale).fract();
            let rebuild = row.cells != logical_cells
                || row.bidi != bidi.is_some()
                || row.highlights != highlights
                || row.selection != selection
                || row.cursor != cursor
                || row.pixel_phase != pixel_phase
                || row.glyphs_changed;
            let hovered_link = scene.hovered_link.and_then(|(start, end)| {
                (start < base + logical_cells.len() && end > base).then(|| {
                    (
                        // Both row-local endpoints fit Frame::cols, including
                        // the half-open end at the maximum 65535 columns.
                        start.saturating_sub(base) as u16,
                        end.saturating_sub(base).min(logical_cells.len()) as u16,
                    )
                })
            });
            let rebuild_background =
                rebuild || image_layers_changed || row.hovered_link != hovered_link;
            let cells = if let Some(bidi) = bidi.filter(|_| rebuild_background) {
                self.visual_cells.clear();
                self.visual_cells.extend(
                    (0..logical_cells.len()).map(|x| logical_cells[bidi.logical(x)].clone()),
                );
                &self.visual_cells[..]
            } else {
                logical_cells
            };
            let mut flags = 0;
            if rebuild_background {
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
                let below_glyphs = if below_text {
                    row.background = background.to_node();
                    gtk::Snapshot::new()
                } else {
                    row.background = None;
                    background
                };
                if let Some((x, width, rgb)) = cursor {
                    append_row_rect(
                        &below_glyphs,
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
                                        (start as usize..end as usize)
                                            .contains(&(colors.logical(base + x) - base))
                                    }))
                        })
                    })
                    .flatten()
                    // Row-local columns are bounded by Frame::cols.
                    .map(|x| x as u16);
                let mut below_glyphs = RowPhases::with_snapshot(below_glyphs, row.blink);
                // Ghostty paints underlines and overlines below glyphs so
                // contrasting colors cannot cover descenders or tall ink.
                if decorations & decoration::UNDERLINE_MASK != 0 || hovered_link.is_some() {
                    append_decorations(
                        &mut below_glyphs,
                        &colors,
                        cells,
                        base,
                        y,
                        DecorationKind::Underline,
                    );
                }
                if decorations & decoration::OVERLINE != 0 {
                    append_decorations(
                        &mut below_glyphs,
                        &colors,
                        cells,
                        base,
                        y,
                        DecorationKind::Overline,
                    );
                }
                (row.below_glyphs, row.hidden_below_glyphs) = below_glyphs.finish();
                row.hovered_link = hovered_link;
                stats.background_rows += 1;
            }
            if rebuild {
                let row_y = y as f64 * scene.metrics.1;
                let mut foreground = RowPhases::new(row.blink);
                let mut x = 0;
                while x < cells.len() {
                    let cell = &cells[x];
                    let text = cell.text();
                    if text.starts_with('\u{10eeee}') {
                        x += 1;
                        continue;
                    }
                    if let Some(bidi) = bidi.filter(|row| row.rtl(x))
                        && can_shape_rtl(cells, x, &self.glyphs)
                    {
                        let mut end = x + 1;
                        while end < cells.len()
                            && bidi.rtl(end)
                            && bidi.logical(end) + 1 == bidi.logical(end - 1)
                            && can_shape_rtl(cells, end, &self.glyphs)
                        {
                            end += 1;
                        }
                        stats.text_nodes += batch.flush(&mut foreground);
                        stats.text_nodes += append_rtl(
                            &mut foreground,
                            cells,
                            x..end,
                            &colors,
                            font,
                            context,
                            &mut self.rtl,
                        );
                        x = end;
                        continue;
                    }
                    if cell.flags & 32 != 0 || text.is_empty() || text == " " {
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
                    if !self.glyphs.is_empty()
                        && let Some(cp) = cell_codepoint(text)
                        && let Some(glyph) = self.glyphs.get(cp)
                    {
                        stats.text_nodes += batch.flush(&mut foreground);
                        foreground.set_blink(cell.blink());
                        let span = if cells.get(x + 1).is_some_and(|next| {
                            next.flags & 64 != 0
                                || (glyph.width == 2
                                    && next.flags & 128 == 0
                                    && matches!(next.text(), "" | " "))
                        }) {
                            2
                        } else {
                            1
                        };
                        let baseline = if matches!(glyph.vertical, Vertical::Baseline) {
                            *self.baseline.get_or_insert_with(|| {
                                let metrics = context.metrics(Some(font), None);
                                f64::from(metrics.ascent()) / f64::from(pango::SCALE) + 1.0
                            })
                        } else {
                            0.0
                        };
                        let path = self
                            .glyph_paths
                            .entry(cp)
                            .or_insert_with(|| glyphs::GlyphPath::new(glyph));
                        let bounds = (rect.0, rect.2 * f64::from(span), rect.3);
                        let color = rgba(rgb, alpha);
                        let rasterized = self.glyph_rasters.as_mut().is_some_and(|rasters| {
                            rasters.append(
                                &foreground.current,
                                path,
                                glyph,
                                glyphs::RasterCell {
                                    bounds,
                                    row_y,
                                    origin: scene.origin,
                                    scale: scene.scale,
                                    span,
                                    baseline,
                                },
                                &color,
                            )
                        });
                        if !rasterized {
                            path.append(&foreground.current, glyph, bounds, baseline, &color);
                        }
                        x += 1;
                        continue;
                    }
                    if let Some(graphic) = render::Graphic::from_text(text) {
                        stats.text_nodes += batch.flush(&mut foreground);
                        foreground.set_blink(cell.blink());
                        let snapshot = &foreground.current;
                        match graphic {
                            render::Graphic::Block(mask) => render::block_rects(mask, rect, |r| {
                                append_row_rect(snapshot, scene, row_y, rgb, alpha, r);
                            }),
                            render::Graphic::Box(drawing) => drawing.paint(
                                rect,
                                (scene.origin.0, scene.origin.1 + row_y),
                                scene.scale,
                                |part| match part {
                                    render::BoxPart::Rect(r) => {
                                        append_rect(snapshot, rgb, alpha, r)
                                    }
                                    render::BoxPart::Diagonals {
                                        bounds,
                                        stroke,
                                        rising,
                                        falling,
                                    } => {
                                        append_box_diagonals(
                                            snapshot,
                                            bounds,
                                            stroke,
                                            rising,
                                            falling,
                                            &rgba(rgb, alpha),
                                        );
                                    }
                                    render::BoxPart::Corner {
                                        center,
                                        reach,
                                        half_stroke,
                                        radius,
                                    } => append_box_corner(
                                        snapshot,
                                        center,
                                        reach,
                                        half_stroke,
                                        radius,
                                        &rgba(rgb, alpha),
                                    ),
                                },
                            ),
                        }
                        x += 1;
                    } else {
                        let mut end = x + 1;
                        let text = if ascii_cell(cell) {
                            self.text.clear();
                            self.text.push(cell.text().as_bytes()[0] as char);
                            // Keep selection and block cursors as shaping
                            // boundaries even when foreground colors match.
                            let mut limit = cells.len();
                            for (start, last) in [
                                selection.filter(|_| bidi.is_none()),
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
                                && bidi.is_none_or(|row| {
                                    !row.rtl(end)
                                        && colors.selected(base + row.logical(x))
                                            == colors.selected(base + row.logical(end))
                                })
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
                row.cells.extend_from_slice(logical_cells);
                row.bidi = bidi.is_some();
                row.highlights.clear();
                row.highlights.extend_from_slice(highlights);
                row.selection = selection;
                row.cursor = cursor;
                row.pixel_phase = pixel_phase;
                row.glyphs_changed = false;
                stats.rebuilt_rows += 1;
            }
            if row
                .blink_start
                .is_some_and(|x| x as f64 * scene.metrics.0 < scene.width as f64)
                && (y as f64 * scene.metrics.1) < scene.height as f64
            {
                stats.blinking_rows += 1;
            }
            let background = if below_text {
                row.background.as_ref()
            } else {
                row.below_glyphs(scene.text_blink_visible)
            };
            append_cached_row(snapshot, background, y as f64 * scene.metrics.1);
        }
        if let Some(graphics) = graphics.filter(|_| below_text) {
            self.textures
                .append(snapshot, scene, graphics, Layer::BelowText);
            for (y, row) in self.rows.iter().enumerate() {
                append_cached_row(
                    snapshot,
                    row.below_glyphs(scene.text_blink_visible),
                    y as f64 * scene.metrics.1,
                );
            }
        }
        // Paint every background before any text, including wide and tall glyphs.
        for (y, row) in self.rows.iter().enumerate() {
            let foreground = if row.blink && !scene.text_blink_visible {
                row.hidden_foreground.as_ref()
            } else {
                row.foreground.as_ref()
            };
            append_cached_row(snapshot, foreground, y as f64 * scene.metrics.1);
        }
        if let Some(graphics) = graphics {
            self.textures
                .append(snapshot, scene, graphics, Layer::AboveText);
        }
        if cursor_visible && block_cursor.is_none() {
            let alpha = if scene.focused { 1.0 } else { 0.35 };
            let password = scene.frame.cursor_password_input;
            if !password
                || !append_password_cursor(snapshot, scene, rgba(scene.frame.cursor, alpha))
            {
                render::cursor_rects(
                    scene.frame.cursor_style,
                    scene.focused && !password,
                    scene.cursor_rect(),
                    |rect| append_row_rect(snapshot, scene, 0.0, scene.frame.cursor, alpha, rect),
                );
            }
        }
        self.visual_cells.clear();
        stats.cached_shapes = self.shapes.order.len();
        stats.cached_shape_bytes = self.shapes.bytes;
        stats
    }
}

fn can_shape_rtl(cells: &[Cell], x: usize, glyphs: &Glyphs) -> bool {
    let cell = &cells[x];
    cell.flags & 64 == 0
        && !cells.get(x + 1).is_some_and(|cell| cell.flags & 64 != 0)
        && render::Graphic::from_text(cell.text()).is_none()
        && !cell.text().starts_with('\u{10eeee}')
        && cell_codepoint(cell.text()).is_none_or(|cp| glyphs.get(cp).is_none())
}

#[derive(Default)]
struct RtlScratch {
    text: String,
    offsets: Vec<usize>,
    layout: Option<pango::Layout>,
    cluster: Option<pango::GlyphString>,
}

// Shape a visual RTL run in logical order so Pango sees the joining context.
// Paint is resolved afterwards: a cursor, selection, or blinking cell must
// not split the run and change the shapes of its neighbors.
fn append_rtl(
    phases: &mut RowPhases,
    cells: &[Cell],
    span: std::ops::Range<usize>,
    colors: &RowColors<'_, '_>,
    font: &pango::FontDescription,
    context: &pango::Context,
    scratch: &mut RtlScratch,
) -> usize {
    use gtk::pango::IsAttribute;

    let run = &cells[span.clone()];
    let RtlScratch {
        text,
        offsets,
        layout,
        cluster,
    } = scratch;
    // A run inherited from the bidi algorithm can contain only neutral
    // characters. Force RTL for mirroring and strip the empty prefix glyph.
    text.clear();
    text.push('\u{200f}');
    offsets.clear();
    offsets.reserve(run.len() + 1);
    let mut attrs = None;
    let mut logical = 0;
    while logical < run.len() {
        let style = run[run.len() - 1 - logical].flags & 3;
        let start = text.len() as u32;
        while logical < run.len() {
            let cell = &run[run.len() - 1 - logical];
            if cell.flags & 3 != style {
                break;
            }
            offsets.push(text.len());
            let glyph = cell.text();
            text.push_str(if glyph.is_empty() { " " } else { glyph });
            logical += 1;
        }
        for mut attr in [
            (style & 1 != 0).then(|| pango::AttrInt::new_weight(pango::Weight::Bold).upcast()),
            (style & 2 != 0).then(|| pango::AttrInt::new_style(pango::Style::Italic).upcast()),
        ]
        .into_iter()
        .flatten()
        {
            attr.set_start_index(start);
            attr.set_end_index(text.len() as u32);
            attrs.get_or_insert_with(pango::AttrList::new).insert(attr);
        }
    }
    offsets.push(text.len());
    let layout = layout.get_or_insert_with(|| {
        // Renderer::clear drops the layout when its font or context changes.
        let layout = pango::Layout::new(context);
        layout.set_single_paragraph_mode(true);
        layout.set_font_description(Some(font));
        layout
    });
    layout.set_attributes(attrs.as_ref());
    layout.set_text(text);
    let cluster = cluster.get_or_insert_with(pango::GlyphString::new);
    let (cw, ch) = colors.scene.metrics;
    let mut iter = layout.iter();
    let mut nodes = 0;
    loop {
        if let Some(shaped) = iter.run_readonly() {
            let glyphs = shaped.glyph_string();
            let item = shaped.item();
            let item_offset = item.offset() as usize;
            let item_end = item_offset + item.length() as usize;
            let rtl = item.analysis().level() % 2 != 0;
            let font = item.analysis().font();
            let baseline = (iter.run_baseline() + pango::SCALE) as f32 / pango::SCALE as f32;
            let mut last_byte = item_end;
            let mut i = 0;
            while i < glyphs.glyph_info().len() {
                let start = i;
                let byte = item_offset + glyphs.log_clusters()[i] as usize;
                i += 1;
                while glyphs.log_clusters().get(i) == glyphs.log_clusters().get(start) {
                    i += 1;
                }
                let next_byte = if rtl {
                    last_byte
                } else {
                    glyphs
                        .log_clusters()
                        .get(i)
                        .map_or(item_end, |&b| item_offset + b as usize)
                };
                last_byte = byte;
                let ink = &glyphs.glyph_info()[start..i];
                if byte < offsets[0] || ink.iter().all(|glyph| glyph.glyph() == pango::GLYPH_EMPTY)
                {
                    continue;
                }
                let first = offsets.partition_point(|&p| p <= byte) - 1;
                let after = offsets
                    .partition_point(|&p| p < next_byte)
                    .max(first + 1)
                    .min(run.len());
                let visual = span.end - after..span.end - first;
                let advance: i64 = ink
                    .iter()
                    .map(|glyph| i64::from(glyph.geometry().width()))
                    .sum();
                let scale = if advance > 0 {
                    (cw * visual.len() as f64 * f64::from(pango::SCALE) / advance as f64) as f32
                } else {
                    1.0
                };
                cluster.set_size(ink.len() as i32);
                cluster.glyph_info_mut().clone_from_slice(ink);
                cluster.log_clusters_mut().fill(0);
                let paint = |i: usize| {
                    let cell = &cells[i];
                    (
                        cell.flags & 32 == 0,
                        cell.blink(),
                        colors.foreground(colors.base + i, cell),
                    )
                };
                let mut col = visual.start;
                while col < visual.end {
                    let key = paint(col);
                    let mut end = col + 1;
                    while end < visual.end && paint(end) == key {
                        end += 1;
                    }
                    let (visible, blink, (rgb, alpha)) = key;
                    if visible
                        && let Some(node) = gsk::TextNode::new(
                            &font,
                            cluster,
                            &rgba(rgb, 1.0),
                            &graphene::Point::new(0.0, baseline),
                        )
                    {
                        phases.set_blink(blink);
                        let snapshot = &phases.current;
                        let clip = col != visual.start || end != visual.end;
                        if clip {
                            snapshot.push_clip(&graphene::Rect::new(
                                (col as f64 * cw) as f32,
                                0.0,
                                ((end - col) as f64 * cw) as f32,
                                ch as f32,
                            ));
                        }
                        if alpha < 1.0 {
                            snapshot.push_opacity(alpha as f64);
                        }
                        snapshot.save();
                        snapshot.translate(&graphene::Point::new(
                            (visual.start as f64 * cw) as f32,
                            0.0,
                        ));
                        snapshot.scale(scale, 1.0);
                        snapshot.append_node(&node);
                        snapshot.restore();
                        if alpha < 1.0 {
                            snapshot.pop();
                        }
                        if clip {
                            snapshot.pop();
                        }
                        nodes += 1;
                    }
                    col = end;
                }
            }
        }
        if !iter.next_run() {
            break;
        }
    }
    nodes
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

fn append_box_diagonals(
    snapshot: &gtk::Snapshot,
    (x, y, w, h): Rect,
    stroke: f64,
    rising: bool,
    falling: bool,
    color: &gdk::RGBA,
) {
    let (x, y, w, h) = (x as f32, y as f32, w as f32, h as f32);
    let path = gsk::PathBuilder::new();
    // Extend past the corners and clip to the grid, so a diagonal doesn't
    // taper at its ends. Both strokes of a cross use one opacity.
    if rising {
        path.move_to(x - w * 0.5, y + h * 1.5);
        path.line_to(x + w * 1.5, y - h * 0.5);
    }
    if falling {
        path.move_to(x - w * 0.5, y - h * 0.5);
        path.line_to(x + w * 1.5, y + h * 1.5);
    }
    snapshot.push_clip(&graphene::Rect::new(x, y, w, h));
    snapshot.append_stroke(&path.to_path(), &gsk::Stroke::new(stroke as f32), color);
    snapshot.pop();
}

fn append_box_corner(
    snapshot: &gtk::Snapshot,
    (cx, cy): (f64, f64),
    (dx, dy): (f64, f64),
    half: f64,
    radius: f64,
    color: &gdk::RGBA,
) {
    let builder = gsk::PathBuilder::new();
    // Reflect a single quarter-circle outline into the four orientations.
    // Both legs and the bend share one fill, even for faint cells.
    let point = |(x, y): (f64, f64)| ((cx + x * dx.signum()) as f32, (cy + y * dy.signum()) as f32);
    let line_to = |p| {
        let (x, y) = point(p);
        builder.line_to(x, y);
    };
    let quarter_arc_to = |control, end| {
        let (x1, y1) = point(control);
        let (x2, y2) = point(end);
        builder.conic_to(x1, y1, x2, y2, std::f32::consts::FRAC_1_SQRT_2);
    };
    let (x, y) = point((dx.abs(), -half));
    builder.move_to(x, y);
    line_to((radius, -half));
    quarter_arc_to((-half, -half), (-half, radius));
    line_to((-half, dy.abs()));
    line_to((half, dy.abs()));
    line_to((half, radius));
    quarter_arc_to((half, half), (radius, half));
    line_to((dx.abs(), half));
    builder.close();
    snapshot.append_fill(&builder.to_path(), gsk::FillRule::Winding, color);
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

fn append_password_cursor(snapshot: &gtk::Snapshot, scene: &Scene<'_>, color: gdk::RGBA) -> bool {
    thread_local! {
        static LOCK: Option<gtk::IconPaintable> = {
            let theme = gtk::IconTheme::new();
            theme.set_theme_name(Some("Adwaita"));
            let name = "changes-prevent-symbolic";
            theme.has_icon(name).then(|| theme.lookup_icon(
                name, &[], 16, 1, gtk::TextDirection::None, gtk::IconLookupFlags::FORCE_SYMBOLIC,
            ))
        };
    }
    LOCK.with(|icon| {
        let Some(icon) = icon else {
            return false;
        };
        let (x, y, w, h) = scene.cursor_rect();
        // The visible lock occupies 12 of the icon's 16 columns.
        let size = (scene.metrics.0 * (16.0 / 12.0)).min(h);
        let (x, y, width, height) = snapped_row_rect(
            scene,
            0.0,
            (x + (w - size) / 2.0, y + (h - size) / 2.0, size, size),
        );
        snapshot.save();
        snapshot.translate(&graphene::Point::new(x as f32, y as f32));
        icon.snapshot_symbolic(snapshot, width, height, &[color]);
        snapshot.restore();
        true
    })
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
                .is_some_and(|(start, end)| (start..end).contains(&colors.logical(index)))
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
