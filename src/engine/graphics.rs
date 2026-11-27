use super::{
    Events, GMUX_GRAPHICS_MAX_PIXEL_BYTES, GMUX_GRAPHICS_MAX_PLACEMENTS, GmuxEngine,
    GmuxGraphicsHold, GmuxImage, GmuxImagePlacement, GmuxView, gmux_engine_graphics,
    gmux_engine_graphics_sources, gmux_engine_graphics_virtual_runs, gmux_engine_image,
};
use gmux_protocol::{
    Cell as TerminalCell, Frame,
    bidi::Row,
    graphics::{Image, ImageSizing, Layer, PixelFormat, Placement},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    ffi::c_void,
    mem::MaybeUninit,
    ptr,
    rc::Rc,
    slice,
    sync::{Arc, Weak},
};

const _: () =
    assert!(GMUX_GRAPHICS_MAX_PIXEL_BYTES as usize == gmux_protocol::graphics::MAX_PIXEL_BYTES);
const _: () =
    assert!(GMUX_GRAPHICS_MAX_PLACEMENTS as usize == gmux_protocol::graphics::MAX_PLACEMENTS);

pub(super) struct Snapshot {
    pub live: LayoutSnapshot,
    pub held: LayoutSnapshot,
    pub published: Option<Arc<GraphicsLayout>>,
    pub geometry: GraphicsViewport,
    held_revision: u64,
    held_cell_pixels: [u32; 2],
    budget: Rc<Cell<usize>>,
}

#[derive(Default)]
pub(super) struct LayoutSnapshot {
    generation: u64,
    cell_pixels: [u32; 2],
    image_ids: Vec<u32>,
    pub layout: Option<Arc<GraphicsLayout>>,
    needs_text: bool,
    viewport: Option<Viewport>,
    origin_viewport: Option<Viewport>,
    origins: HashMap<[u32; 3], VisualOrigin>,
    unicode_columns: HashMap<i32, Option<VisibleColumns>>,
    unicode_viewport: Option<(Viewport, [u32; 2])>,
}

struct VisibleColumns {
    full: Vec<u64>,
    partial: usize,
}

struct VirtualAnchor {
    key: [u32; 3],
    column: u32,
    runs_stamp: u64,
}

struct VisualOrigin {
    left: usize,
    runs_stamp: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Viewport {
    revision: u64,
    offset: u64,
    cols: u16,
    rows: u16,
    geometry: GraphicsViewport,
    frozen_cell_pixels: Option<[u32; 2]>,
}

impl Viewport {
    fn pixels(self, owner_pixels: [u32; 2]) -> ([f64; 2], [f64; 2]) {
        let cell = if self.geometry.cell_pixels.contains(&0) {
            owner_pixels
        } else {
            self.geometry.cell_pixels
        }
        .map(f64::from);
        let [canvas_w, canvas_h] = self.geometry.canvas_pixels;
        let size = [
            f64::from(self.cols) * cell[0],
            f64::from(self.rows) * cell[1],
        ];
        let clip = |size: f64, canvas| {
            if canvas == 0 {
                size
            } else {
                size.min(f64::from(canvas))
            }
        };
        (cell, [clip(size[0], canvas_w), clip(size[1], canvas_h)])
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GraphicsViewport {
    pub cell_pixels: [u32; 2],
    pub canvas_pixels: [u16; 2],
}

#[derive(Clone, Copy)]
pub(crate) struct ImageSource {
    pub key: u64,
    id: u32,
}

pub(crate) struct GraphicsLayout {
    pub cell_pixels: [u32; 2],
    pub unicode_cell_pixels: Option<[u32; 2]>,
    pub frozen_cell_pixels: Option<[u32; 2]>,
    pub placements: Vec<Placement>,
    pub images: Vec<ImageSource>,
}

#[derive(Default)]
pub(super) struct GraphicsState {
    pub pixels: HashMap<u64, Weak<Image>>,
    held: HashMap<u64, HeldImage>,
}

struct HeldImage {
    image: Arc<Image>,
    source: ImageSource,
    budget: Rc<Cell<usize>>,
    alternate_screen: bool,
}

impl Drop for HeldImage {
    fn drop(&mut self) {
        self.budget.set(self.budget.get() - self.image.pixels.len());
    }
}

impl Snapshot {
    pub fn new(budget: Rc<Cell<usize>>) -> Self {
        Self {
            live: Default::default(),
            held: Default::default(),
            published: None,
            geometry: GraphicsViewport::default(),
            held_revision: 0,
            held_cell_pixels: [0; 2],
            budget,
        }
    }

    pub fn clear(&mut self) {
        self.live = Default::default();
        self.held = Default::default();
        self.published = None;
    }

    pub fn memory_size(&self) -> usize {
        size_of::<Self>() + self.live.memory_size() + self.held.memory_size()
    }

    pub fn publish(&mut self, held: bool) -> Option<Arc<GraphicsLayout>> {
        let latest = if held {
            &self.held.layout
        } else {
            &self.live.layout
        };
        if let (Some(published), Some(latest)) = (&self.published, latest)
            && (Arc::ptr_eq(published, latest)
                || (published.cell_pixels == latest.cell_pixels
                    && published.unicode_cell_pixels == latest.unicode_cell_pixels
                    && published.frozen_cell_pixels == latest.frozen_cell_pixels
                    && published.placements == latest.placements))
        {
            return Some(published.clone());
        }
        self.published.clone_from(latest);
        self.published.clone()
    }

    pub fn refresh(
        &mut self,
        engine: *const GmuxEngine,
        pixels: &mut HashMap<u64, Weak<Image>>,
        held: bool,
        frame: Frame,
        revision: u64,
        cells: &[TerminalCell],
    ) -> bool {
        let (snapshot, revision) = if held {
            (&mut self.held, self.held_revision)
        } else {
            (&mut self.live, revision)
        };
        snapshot.refresh(
            engine,
            Viewport {
                revision,
                offset: frame.scrollback.offset,
                cols: frame.cols,
                rows: frame.rows,
                geometry: self.geometry,
                frozen_cell_pixels: held.then_some(self.held_cell_pixels),
            },
            cells,
            pixels,
        )
    }
}

impl LayoutSnapshot {
    fn refresh(
        &mut self,
        engine: *const GmuxEngine,
        viewport: Viewport,
        cells: &[TerminalCell],
        pixels: &mut HashMap<u64, Weak<Image>>,
    ) -> bool {
        unsafe extern "C" fn collect(
            data: *mut c_void,
            raw: *const GmuxImagePlacement,
            cell_pixels: *const u32,
        ) -> bool {
            let selection = unsafe { &mut *data.cast::<Selection<'_>>() };
            let raw = unsafe { *raw };
            // The iterator visits positioned images before Unicode runs.
            if raw.source_subpixels[2] == 0 {
                selection.positioned.push(raw);
                return true;
            }
            selection.prepare(unsafe { *cell_pixels.cast::<[u32; 2]>() });
            selection.keep(raw)
        }

        let known_viewport = Viewport {
            revision: if self.needs_text {
                viewport.revision
            } else {
                0
            },
            ..viewport
        };
        let same_viewport = self.viewport == Some(known_viewport);
        let known = if same_viewport { self.generation } else { 0 };
        let mut cell_pixels = self.cell_pixels;
        let mut selection = Selection {
            viewport,
            engine,
            cells,
            positioned: Vec::new(),
            visible: Vec::new(),
            needs_text: false,
            owner_pixels: [0; 2],
            unicode_pixels: None,
            frozen_pixels: None,
            cell: [0.0; 2],
            canvas: [0.0; 2],
            prepared: false,
            origin_viewport: &mut self.origin_viewport,
            origins: &mut self.origins,
            unicode_viewport: &mut self.unicode_viewport,
            unicode_columns: &mut self.unicode_columns,
        };
        let generation = unsafe {
            gmux_engine_graphics(
                engine,
                known,
                cell_pixels.as_mut_ptr(),
                ptr::from_mut(&mut selection).cast(),
                Some(collect),
            )
        };
        if same_viewport
            && generation == self.generation
            && (generation == 0 || self.cell_pixels == cell_pixels)
        {
            return false;
        }
        selection.prepare(cell_pixels);
        self.generation = generation;
        self.cell_pixels = cell_pixels;
        self.needs_text = selection.needs_text;
        self.viewport = Some(Viewport {
            revision: if self.needs_text {
                viewport.revision
            } else {
                0
            },
            ..viewport
        });
        let visible = selection.into_layout();
        let visible_images = visible.as_ref().map_or(&[][..], |layout| &layout.images);
        let newly_visible = visible_images
            .iter()
            .any(|source| self.image_ids.binary_search(&source.id).is_err());
        self.image_ids.clear();
        self.image_ids
            .extend(visible_images.iter().map(|source| source.id));
        self.image_ids.sort_unstable();
        self.image_ids.dedup();
        if !matches!((&self.layout, &visible), (Some(old), Some(new))
            if old.cell_pixels == new.cell_pixels
                && old.unicode_cell_pixels == new.unicode_cell_pixels
                && old.frozen_cell_pixels == new.frozen_cell_pixels
                && old.placements == new.placements)
        {
            self.layout = visible;
        }
        pixels.retain(|_, image| image.strong_count() != 0);
        newly_visible
    }

    pub fn memory_size(&self) -> usize {
        self.image_ids.capacity() * size_of::<u32>()
            + self.layout.as_ref().map_or(0, |layout| {
                size_of::<GraphicsLayout>()
                    + layout.placements.capacity() * size_of::<Placement>()
                    + layout.images.capacity() * size_of::<ImageSource>()
            })
            + self.origins.capacity() * (size_of::<([u32; 3], VisualOrigin)>() + 1)
            + self.unicode_columns.capacity() * (size_of::<(i32, Option<VisibleColumns>)>() + 1)
            + self
                .unicode_columns
                .values()
                .flatten()
                .map(|columns| columns.full.capacity() * size_of::<u64>())
                .sum::<usize>()
    }

    pub fn shows_images(&self, changed: &[u32]) -> bool {
        changed
            .iter()
            .any(|id| self.image_ids.binary_search(id).is_ok())
    }
}

fn bidi_row(cells: &[TerminalCell], cols: usize, row: i32) -> Option<Row> {
    let start = usize::try_from(row).ok()? * cols;
    Row::new(cells.get(start..start + cols)?)
}

fn read_visual_origins(
    engine: *const GmuxEngine,
    columns: &mut HashMap<[u32; 3], VisualOrigin>,
    cells: &[TerminalCell],
    cols: usize,
) {
    struct Capture<'a> {
        columns: &'a mut HashMap<[u32; 3], VisualOrigin>,
        cells: &'a [TerminalCell],
        cols: usize,
        viewport_row: Option<i32>,
        bidi: Option<Row>,
    }
    unsafe extern "C" fn visit(
        userdata: *mut c_void,
        image: u32,
        placement: u32,
        tag: u32,
        row: i32,
        column: u32,
        width: u32,
    ) {
        let capture = unsafe { &mut *userdata.cast::<Capture<'_>>() };
        let Some(left) = capture.columns.get_mut(&[image, placement, tag]) else {
            return;
        };
        if capture.viewport_row != Some(row) {
            capture.viewport_row = Some(row);
            capture.bidi = bidi_row(capture.cells, capture.cols, row);
        }
        let column = column as usize;
        let visual = capture.bidi.as_ref().map_or(column, |row| {
            (column..column + width as usize)
                .map(|x| row.visual(x))
                .min()
                .unwrap_or(column)
        });
        left.left = left.left.min(visual);
    }

    let mut capture = Capture {
        columns,
        cells,
        cols,
        viewport_row: None,
        bidi: None,
    };
    unsafe {
        gmux_engine_graphics_virtual_runs(engine, ptr::from_mut(&mut capture).cast(), Some(visit))
    };
}

struct Selection<'a> {
    viewport: Viewport,
    engine: *const GmuxEngine,
    cells: &'a [TerminalCell],
    positioned: Vec<GmuxImagePlacement>,
    visible: Vec<(u32, i32, Placement)>,
    owner_pixels: [u32; 2],
    unicode_pixels: Option<[u32; 2]>,
    frozen_pixels: Option<[u32; 2]>,
    cell: [f64; 2],
    canvas: [f64; 2],
    needs_text: bool,
    prepared: bool,
    origin_viewport: &'a mut Option<Viewport>,
    origins: &'a mut HashMap<[u32; 3], VisualOrigin>,
    unicode_viewport: &'a mut Option<(Viewport, [u32; 2])>,
    unicode_columns: &'a mut HashMap<i32, Option<VisibleColumns>>,
}

impl Selection<'_> {
    fn prepare(&mut self, owner_pixels: [u32; 2]) {
        if self.prepared {
            return;
        }
        self.prepared = true;
        self.owner_pixels = owner_pixels;
        (self.cell, self.canvas) = self.viewport.pixels(owner_pixels);
        let source = self
            .viewport
            .frozen_cell_pixels
            .unwrap_or(self.viewport.geometry.cell_pixels);
        let [width, height] = source;
        if self.viewport.frozen_cell_pixels.is_some() {
            let frozen = if source.contains(&0) {
                owner_pixels
            } else {
                source
            };
            self.frozen_pixels = (frozen.map(f64::from) != self.cell).then_some(frozen);
        }
        let [owner_width, owner_height] = owner_pixels;
        self.unicode_pixels = (width != 0
            && height != 0
            && u64::from(owner_width) * u64::from(height)
                != u64::from(owner_height) * u64::from(width))
        .then_some([width, height]);
        let viewport = Viewport {
            geometry: GraphicsViewport::default(),
            frozen_cell_pixels: None,
            ..self.viewport
        };
        let positioned = std::mem::take(&mut self.positioned);
        let anchors = || positioned.iter().filter_map(VirtualAnchor::from_raw);
        let any_anchors = anchors().next().is_some();
        self.needs_text |= any_anchors;
        if !any_anchors {
            self.origins.clear();
            *self.origin_viewport = None;
        } else if *self.origin_viewport != Some(viewport)
            || anchors().any(|a| {
                self.origins
                    .get(&a.key)
                    .is_none_or(|o| o.runs_stamp != a.runs_stamp)
            })
        {
            self.origins.clear();
            self.origins.extend(anchors().map(|a| {
                (
                    a.key,
                    VisualOrigin {
                        left: usize::MAX,
                        runs_stamp: a.runs_stamp,
                    },
                )
            }));
            read_visual_origins(
                self.engine,
                self.origins,
                self.cells,
                usize::from(viewport.cols),
            );
            *self.origin_viewport = Some(viewport);
        }
        let narrow = self.canvas[0] < f64::from(self.viewport.cols) * self.cell[0];
        let unicode_viewport = narrow.then_some((self.viewport, owner_pixels));
        if *self.unicode_viewport != unicode_viewport {
            self.unicode_columns.clear();
            *self.unicode_viewport = unicode_viewport;
        }
        for raw in positioned {
            if !self.keep(raw) {
                break;
            }
        }
    }

    fn keep(&mut self, raw: GmuxImagePlacement) -> bool {
        if self.visible.len() == GMUX_GRAPHICS_MAX_PLACEMENTS as usize {
            return false;
        }
        let source_subpixels = (raw.source_subpixels[2] != 0).then_some(raw.source_subpixels);
        let unicode = source_subpixels.is_some();
        let [view_w, view_h] = self.canvas;
        let narrow = view_w < f64::from(self.viewport.cols) * self.cell[0];
        self.needs_text |= unicode && narrow;
        let mut cell = raw.cell;
        if let Some(anchor) = VirtualAnchor::from_raw(&raw)
            && let Some(origin) = self.origins.get(&anchor.key)
            && let Ok(visual) = i32::try_from(origin.left)
        {
            cell[0] = cell[0].saturating_add(visual - anchor.column as i32);
        }
        let placement = Placement {
            image: raw.generation,
            layer: [Layer::BelowBackground, Layer::BelowText, Layer::AboveText][raw.layer as usize],
            cell,
            offset: raw.offset,
            size: raw.size,
            source: raw.source,
            source_subpixels,
            fit_rows: (raw.fit_rows[1] != 0).then_some(raw.fit_rows),
            sizing: match raw.sizing {
                1 => Some(ImageSizing::Columns),
                2 => Some(ImageSizing::Rows),
                3 => Some(ImageSizing::Native),
                _ => None,
            },
        };
        let pixels = if unicode {
            self.unicode_pixels.unwrap_or(self.owner_pixels)
        } else {
            self.owner_pixels
        };
        let bounds = placement.viewport_bounds(self.cell, pixels, self.frozen_pixels);
        let [x, _, w, _] = bounds.rect;
        if bounds.visible_y(view_h).is_some()
            && self.visible_x(unicode && narrow, cell[1], x, w, view_w)
        {
            self.visible.push((raw.id, raw.z, placement));
        }
        self.visible.len() < GMUX_GRAPHICS_MAX_PLACEMENTS as usize
    }

    fn visible_x(&mut self, reorder: bool, row: i32, x: f64, w: f64, view_w: f64) -> bool {
        if reorder {
            let cols = usize::from(self.viewport.cols);
            let cw = self.cell[0];
            let whole_cols = (view_w / cw).floor() as usize;
            let columns = self.unicode_columns.entry(row).or_insert_with(|| {
                let row = bidi_row(self.cells, cols, row)?;
                let mut full = vec![0; cols.div_ceil(64)];
                for visual in 0..whole_cols {
                    let logical = row.logical(visual);
                    full[logical / 64] |= 1 << (logical % 64);
                }
                Some(VisibleColumns {
                    full,
                    partial: row.logical(whole_cols),
                })
            });
            if let Some(columns) = columns {
                let start = (x / cw).floor().max(0.0) as usize;
                let end = (((x + w) / cw).ceil().max(0.0) as usize).min(cols);
                let partial_width = view_w - whole_cols as f64 * cw;
                return (start..end).any(|col| {
                    columns.full[col / 64] & (1 << (col % 64)) != 0
                        || (col == columns.partial
                            && (x - col as f64 * cw).max(0.0) < partial_width)
                });
            }
        }
        x < view_w && x + w > 0.0
    }

    fn into_layout(mut self) -> Option<Arc<GraphicsLayout>> {
        if self.visible.is_empty() {
            return None;
        }
        self.visible.sort_unstable_by_key(|&(id, z, p)| {
            (
                p.layer,
                z,
                id,
                p.cell,
                p.offset,
                p.source,
                p.source_subpixels,
                p.fit_rows,
                p.size,
            )
        });
        let mut images = BTreeMap::new();
        let mut uses_unicode = false;
        let placements = self
            .visible
            .into_iter()
            .map(|(id, _, p)| {
                images
                    .entry(p.image)
                    .or_insert(ImageSource { key: p.image, id });
                uses_unicode |= p.source_subpixels.is_some();
                p
            })
            .collect();
        Some(Arc::new(GraphicsLayout {
            cell_pixels: self.owner_pixels,
            unicode_cell_pixels: self.unicode_pixels.filter(|_| uses_unicode),
            frozen_cell_pixels: self.frozen_pixels,
            placements,
            images: images.into_values().collect(),
        }))
    }
}

impl VirtualAnchor {
    fn from_raw(raw: &GmuxImagePlacement) -> Option<Self> {
        let [image, placement, tag, column] = raw.virtual_anchor;
        (image != 0).then_some(Self {
            key: [image, placement, tag],
            column,
            runs_stamp: raw.anchor_runs_stamp,
        })
    }
}

impl GraphicsState {
    pub fn has_held_pixels(&self) -> bool {
        !self.held.is_empty()
    }

    pub fn clear_held(&mut self) {
        self.held.clear();
    }

    pub fn image(&mut self, engine: *const GmuxEngine, source: ImageSource) -> Option<Arc<Image>> {
        self.image_with(source, || native_image(engine, source))
    }

    fn image_with(
        &mut self,
        source: ImageSource,
        load: impl FnOnce() -> Option<GmuxImage>,
    ) -> Option<Arc<Image>> {
        if let Some(image) = self.pixels.get(&source.key).and_then(Weak::upgrade) {
            return Some(image);
        }
        let image = read_image(source, load()?)?;
        self.pixels.insert(source.key, Arc::downgrade(&image));
        Some(image)
    }
}

fn native_image(engine: *const GmuxEngine, source: ImageSource) -> Option<GmuxImage> {
    let mut raw = MaybeUninit::<GmuxImage>::uninit();
    if !unsafe { gmux_engine_image(engine, source.id, source.key, raw.as_mut_ptr()) } {
        return None;
    }
    Some(unsafe { raw.assume_init() })
}

fn read_image(source: ImageSource, raw: GmuxImage) -> Option<Arc<Image>> {
    let mut image = Image {
        key: source.key,
        width: raw.width,
        height: raw.height,
        format: match raw.channels {
            1 => PixelFormat::Gray,
            2 => PixelFormat::GrayAlpha,
            3 => PixelFormat::Rgb,
            4 => PixelFormat::Rgba,
            _ => return None,
        },
        pixels: Vec::new(),
    };
    if image.pixel_len().ok()? != raw.pixels.len {
        return None;
    }
    image.pixels = unsafe { slice::from_raw_parts(raw.pixels.ptr, raw.pixels.len) }.to_vec();
    Some(Arc::new(image))
}

pub(super) unsafe extern "C" fn visible_ids(data: *const c_void, out: *mut *const u32) -> usize {
    let snapshot = unsafe { &*data.cast::<RefCell<Snapshot>>() };
    let snapshot = snapshot.borrow();
    // The native tick borrows the IDs without updating any view snapshots.
    unsafe { out.write(snapshot.live.image_ids.as_ptr()) };
    snapshot.live.image_ids.len()
}

pub(super) unsafe extern "C" fn capture_hold(
    data: *mut c_void,
    engine: *const GmuxEngine,
    view: *const GmuxView,
    viewdata: *mut c_void,
    hold: *mut GmuxGraphicsHold,
) -> bool {
    let state = &mut unsafe { &mut *data.cast::<Events>() }.graphics;
    let hold = unsafe { &mut *hold };
    if !hold.held {
        state.clear_held();
        return true;
    }
    if viewdata.is_null() {
        // Reuse pixels across holds within one feed only while their generation is current.
        state.held.retain(|_, held| {
            held.alternate_screen != hold.alternate_screen
                || native_image(engine, held.source).is_some()
        });
        return true;
    }
    let snapshot = unsafe { &*viewdata.cast::<RefCell<Snapshot>>() };
    let mut snapshot = snapshot.borrow_mut();
    snapshot.held_revision = snapshot.held_revision.wrapping_add(1);
    snapshot.held_cell_pixels = snapshot.geometry.cell_pixels;
    struct Capture<'a> {
        state: &'a mut GraphicsState,
        budget: &'a Rc<Cell<usize>>,
        hold: &'a mut GmuxGraphicsHold,
        engine: *const GmuxEngine,
    }
    unsafe extern "C" fn stage(data: *mut c_void, id: u32, key: u64) -> bool {
        let capture = unsafe { &mut *data.cast::<Capture<'_>>() };
        if capture.state.held.contains_key(&key) {
            return true;
        }
        let source = ImageSource { id, key };
        let Some(raw) = native_image(capture.engine, source) else {
            return false;
        };
        let bytes = capture.budget.get() + raw.pixels.len;
        if bytes > super::MAX_GRAPHICS_HOLD_BYTES {
            return false;
        }
        let Some(image) = capture.state.image_with(source, || Some(raw)) else {
            return false;
        };
        capture.hold.staged_bytes += image.pixels.len();
        capture.budget.set(bytes);
        capture.state.held.insert(
            key,
            HeldImage {
                image,
                source,
                budget: capture.budget.clone(),
                alternate_screen: capture.hold.alternate_screen,
            },
        );
        true
    }

    // A later resize can reveal any of the candidates in the frozen render.
    let mut capture = Capture {
        state,
        budget: &snapshot.budget,
        hold,
        engine,
    };
    unsafe {
        gmux_engine_graphics_sources(
            engine,
            view,
            ptr::from_mut(&mut capture).cast(),
            Some(stage),
        )
    }
}
