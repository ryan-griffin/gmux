use super::{
    Events, GMUX_GRAPHICS_MAX_PIXEL_BYTES, GMUX_GRAPHICS_MAX_PLACEMENTS, GmuxEngine,
    GmuxGraphicsHold, GmuxImage, GmuxImagePlacement, GmuxView, gmux_engine_graphics,
    gmux_engine_graphics_virtual_runs, gmux_engine_image,
};
use gmux_protocol::{
    Cell as TerminalCell, Frame,
    bidi::Row,
    graphics::{Image, ImageSizing, Layer, PixelFormat, Placement},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
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
    held_geometry: GraphicsViewport,
    budget: Rc<Cell<usize>>,
}

#[derive(Default)]
pub(super) struct LayoutSnapshot {
    generation: u64,
    cell_pixels: [u32; 2],
    image_ids: Vec<u32>,
    pub layout: Option<Arc<GraphicsLayout>>,
    anchors: Vec<VirtualAnchor>,
    has_sizing: bool,
    has_unicode: bool,
    viewport: Option<Viewport>,
    visible: Option<Arc<GraphicsLayout>>,
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
    index: usize,
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
            held_geometry: GraphicsViewport::default(),
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

    pub fn publish(
        &mut self,
        engine: *const GmuxEngine,
        held: bool,
        frame: Frame,
        revision: u64,
        cells: &[TerminalCell],
    ) -> (Option<Arc<GraphicsLayout>>, bool) {
        let geometry = if held {
            self.held_geometry
        } else {
            self.geometry
        };
        let latest = if held { &mut self.held } else { &mut self.live };
        let newly_visible = latest.fit_viewport(engine, frame, revision, cells, geometry);
        let latest = &latest.visible;
        if let (Some(published), Some(latest)) = (&self.published, latest)
            && (Arc::ptr_eq(published, latest)
                || (published.cell_pixels == latest.cell_pixels
                    && published.unicode_cell_pixels == latest.unicode_cell_pixels
                    && published.placements == latest.placements))
        {
            return (Some(published.clone()), newly_visible);
        }
        self.published.clone_from(latest);
        (self.published.clone(), newly_visible)
    }
}

impl LayoutSnapshot {
    pub fn refresh(
        &mut self,
        engine: *const GmuxEngine,
        view: *const GmuxView,
        pixels: &mut HashMap<u64, Weak<Image>>,
        geometry: GraphicsViewport,
    ) {
        unsafe extern "C" fn collect(data: *mut c_void, raw: *const GmuxImagePlacement) {
            unsafe { &mut *data.cast::<Vec<GmuxImagePlacement>>() }.push(unsafe { *raw });
        }

        let mut raw = Vec::<GmuxImagePlacement>::new();
        let mut cell_pixels = self.cell_pixels;
        let generation = unsafe {
            gmux_engine_graphics(
                engine,
                view,
                self.generation,
                cell_pixels.as_mut_ptr(),
                ptr::from_mut(&mut raw).cast(),
                Some(collect),
            )
        };
        if generation == self.generation && (generation == 0 || self.cell_pixels == cell_pixels) {
            return;
        }
        self.generation = generation;
        self.cell_pixels = cell_pixels;
        self.anchors.clear();
        self.has_sizing = false;
        self.has_unicode = false;
        self.viewport = None;
        if raw.is_empty() {
            self.layout = None;
            pixels.retain(|_, image| image.strong_count() != 0);
            return;
        }
        raw.sort_unstable_by_key(|p| {
            (
                p.layer,
                p.z,
                p.id,
                p.cell,
                p.offset,
                p.source,
                p.source_subpixels,
                p.virtual_anchor,
                p.fit_rows,
                p.sizing,
                p.size,
            )
        });
        let mut images = BTreeMap::new();
        let mut placements = Vec::with_capacity(raw.len());
        for p in raw {
            images.entry(p.generation).or_insert(ImageSource {
                key: p.generation,
                id: p.id,
            });
            let [image, placement, tag, column] = p.virtual_anchor;
            if image != 0 {
                self.anchors.push(VirtualAnchor {
                    index: placements.len(),
                    key: [image, placement, tag],
                    column,
                    runs_stamp: p.anchor_runs_stamp,
                });
            }
            let fit_rows = (p.fit_rows[1] != 0).then_some(p.fit_rows);
            let sizing = match p.sizing {
                1 => Some(ImageSizing::Columns),
                2 => Some(ImageSizing::Rows),
                3 => Some(ImageSizing::Native),
                _ => None,
            };
            self.has_sizing |= fit_rows.is_some() || sizing.is_some();
            let source_subpixels = (p.source_subpixels[2] != 0).then_some(p.source_subpixels);
            self.has_unicode |= source_subpixels.is_some();
            placements.push(Placement {
                image: p.generation,
                layer: [Layer::BelowBackground, Layer::BelowText, Layer::AboveText]
                    [p.layer as usize],
                cell: p.cell,
                offset: p.offset,
                size: p.size,
                source: p.source,
                source_subpixels,
                fit_rows,
                sizing,
            });
        }
        let [width, height] = geometry.cell_pixels;
        let [owner_width, owner_height] = cell_pixels;
        let unicode_cell_pixels = (self.has_unicode
            && width != 0
            && height != 0
            && u64::from(owner_width) * u64::from(height)
                != u64::from(owner_height) * u64::from(width))
        .then_some(geometry.cell_pixels);
        if !self.layout.as_ref().is_some_and(|old| {
            old.cell_pixels == cell_pixels
                && old.unicode_cell_pixels == unicode_cell_pixels
                && old.placements == placements
        }) {
            self.layout = Some(Arc::new(GraphicsLayout {
                cell_pixels,
                unicode_cell_pixels,
                placements,
                images: images.into_values().collect(),
            }));
        }
        pixels.retain(|_, image| image.strong_count() != 0);
    }

    fn fit_viewport(
        &mut self,
        engine: *const GmuxEngine,
        frame: Frame,
        revision: u64,
        cells: &[TerminalCell],
        geometry: GraphicsViewport,
    ) -> bool {
        let mut viewport = Viewport {
            revision,
            offset: frame.scrollback.offset,
            cols: frame.cols,
            rows: frame.rows,
            geometry,
        };
        let (cell, canvas) = viewport.pixels(self.cell_pixels);
        let unicode_clipped = self.has_unicode
            && (canvas[0] < f64::from(viewport.cols) * cell[0]
                || canvas[1] < f64::from(viewport.rows) * cell[1]);
        if self.anchors.is_empty() && !unicode_clipped {
            viewport.revision = 0;
        }
        if self.viewport == Some(viewport) {
            return false;
        }
        let unicode_viewport = unicode_clipped.then_some((viewport, self.cell_pixels));
        if self.unicode_viewport != unicode_viewport {
            self.unicode_columns.clear();
            self.unicode_viewport = unicode_viewport;
        }
        self.viewport = Some(viewport);
        let text_viewport = Viewport {
            geometry: GraphicsViewport::default(),
            ..viewport
        };
        let visible = match &self.layout {
            Some(layout) if self.has_sizing || !self.anchors.is_empty() || unicode_clipped => {
                if self.anchors.is_empty() {
                    self.origins.clear();
                    self.origin_viewport = None;
                } else if self.origin_viewport != Some(text_viewport)
                    || self.anchors.iter().any(|a| {
                        self.origins
                            .get(&a.key)
                            .is_none_or(|o| o.runs_stamp != a.runs_stamp)
                    })
                {
                    self.origins.clear();
                    self.origins.extend(self.anchors.iter().map(|a| {
                        (
                            a.key,
                            VisualOrigin {
                                left: usize::MAX,
                                runs_stamp: a.runs_stamp,
                            },
                        )
                    }));
                    read_visual_origins(engine, &mut self.origins, cells, frame.cols.into());
                    self.origin_viewport = Some(text_viewport);
                }
                project_viewport(
                    layout,
                    &self.anchors,
                    &self.origins,
                    viewport,
                    unicode_clipped,
                    cells,
                    &mut self.unicode_columns,
                )
            }
            layout => {
                self.origins.clear();
                self.origin_viewport = None;
                layout.clone()
            }
        };
        let visible_images = visible.as_ref().map_or(&[][..], |layout| &layout.images);
        let newly_visible = visible_images
            .iter()
            .any(|source| self.image_ids.binary_search(&source.id).is_err());
        self.image_ids.clear();
        self.image_ids
            .extend(visible_images.iter().map(|source| source.id));
        self.image_ids.sort_unstable();
        self.image_ids.dedup();
        self.visible = visible;
        newly_visible
    }

    pub fn memory_size(&self) -> usize {
        let layout_size = |layout: &Arc<GraphicsLayout>| {
            size_of::<GraphicsLayout>()
                + layout.placements.capacity() * size_of::<Placement>()
                + layout.images.capacity() * size_of::<ImageSource>()
        };
        self.image_ids.capacity() * size_of::<u32>()
            + self.anchors.capacity() * size_of::<VirtualAnchor>()
            + self.layout.as_ref().map_or(0, layout_size)
            + self.visible.as_ref().map_or(0, |visible| {
                if self
                    .layout
                    .as_ref()
                    .is_some_and(|layout| Arc::ptr_eq(visible, layout))
                {
                    0
                } else {
                    layout_size(visible)
                }
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

fn bidi_row(cells: &[TerminalCell], cols: usize, row: i32) -> Option<Row> {
    let start = usize::try_from(row).ok()? * cols;
    Row::new(cells.get(start..start + cols)?)
}

fn project_viewport(
    layout: &Arc<GraphicsLayout>,
    anchors: &[VirtualAnchor],
    origins: &HashMap<[u32; 3], VisualOrigin>,
    viewport: Viewport,
    unicode_clipped: bool,
    cells: &[TerminalCell],
    unicode_columns: &mut HashMap<i32, Option<VisibleColumns>>,
) -> Option<Arc<GraphicsLayout>> {
    let (cell, [view_w, view_h]) = viewport.pixels(layout.cell_pixels);
    let cols = usize::from(viewport.cols);
    let whole_cols = (view_w / cell[0]).floor() as usize;
    let partial_width = view_w - whole_cols as f64 * cell[0];
    let mut placements = Vec::new();
    let mut copied = 0;
    let mut anchors = anchors.iter().peekable();
    for (index, &original) in layout.placements.iter().enumerate() {
        let mut placement = original;
        let unicode = placement.source_subpixels.is_some();
        if anchors.peek().is_some_and(|anchor| anchor.index == index) {
            let anchor = anchors.next().unwrap();
            if let Some(origin) = origins.get(&anchor.key)
                && let Ok(visual) = i32::try_from(origin.left)
            {
                placement.cell[0] = placement.cell[0].saturating_add(visual - anchor.column as i32);
            }
        } else if placement.fit_rows.is_none()
            && placement.sizing.is_none()
            && !(unicode && unicode_clipped)
        {
            continue;
        }
        let pixels = if unicode {
            layout.unicode_cell_pixels.unwrap_or(layout.cell_pixels)
        } else {
            layout.cell_pixels
        };
        let bounds = placement.bounds(cell, pixels);
        let [x, y, w, h] = bounds.rect;
        let [top, bottom] = bounds.clip_y.unwrap_or([0.0, view_h]);
        let visible_y = y.max(top).max(0.0) < (y + h).min(bottom).min(view_h);
        let visible = visible_y
            && if unicode && whole_cols < cols {
                let columns = unicode_columns.entry(placement.cell[1]).or_insert_with(|| {
                    let row = bidi_row(cells, cols, placement.cell[1])?;
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
                    let start = (x / cell[0]).floor().max(0.0) as usize;
                    let end = (((x + w) / cell[0]).ceil().max(0.0) as usize).min(cols);
                    (start..end).any(|col| {
                        columns.full[col / 64] & (1 << (col % 64)) != 0
                            || (col == columns.partial
                                && (x - col as f64 * cell[0]).max(0.0) < partial_width)
                    })
                } else {
                    x < view_w && x + w > 0.0
                }
            } else {
                x < view_w && x + w > 0.0
            };
        if visible && placement == original {
            continue;
        }
        placements.extend_from_slice(&layout.placements[copied..index]);
        if visible {
            placements.push(placement);
        }
        copied = index + 1;
    }
    if copied == 0 {
        return Some(layout.clone());
    }
    placements.extend_from_slice(&layout.placements[copied..]);
    if placements.is_empty() {
        return None;
    }
    let used: HashSet<_> = placements.iter().map(|p| p.image).collect();
    Some(Arc::new(GraphicsLayout {
        cell_pixels: layout.cell_pixels,
        unicode_cell_pixels: layout.unicode_cell_pixels,
        placements,
        images: layout
            .images
            .iter()
            .filter(|im| used.contains(&im.key))
            .copied()
            .collect(),
    }))
}

impl GraphicsState {
    pub fn has_held_pixels(&self) -> bool {
        !self.held.is_empty()
    }

    pub fn clear_held(&mut self) {
        self.held.clear();
    }

    pub fn image(&mut self, engine: *const GmuxEngine, source: ImageSource) -> Option<Arc<Image>> {
        if let Some(image) = self.pixels.get(&source.key).and_then(Weak::upgrade) {
            return Some(image);
        }
        let image = read_image(engine, source)?;
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

fn read_image(engine: *const GmuxEngine, source: ImageSource) -> Option<Arc<Image>> {
    let raw = native_image(engine, source)?;
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
    let geometry = snapshot.geometry;
    snapshot.held_geometry = geometry;
    snapshot
        .held
        .refresh(engine, view, &mut state.pixels, geometry);
    let Some(layout) = &snapshot.held.layout else {
        return true;
    };
    for &source in &layout.images {
        if snapshot.published.as_ref().is_some_and(|layout| {
            layout
                .images
                .binary_search_by_key(&source.key, |im| im.key)
                .is_ok()
        }) || state.held.contains_key(&source.key)
        {
            continue;
        }
        let Some(image) = state.image(engine, source) else {
            return false;
        };
        let bytes = snapshot.budget.get() + image.pixels.len();
        hold.staged_bytes += image.pixels.len();
        if bytes > super::MAX_GRAPHICS_HOLD_BYTES {
            return false;
        }
        snapshot.budget.set(bytes);
        state.held.insert(
            source.key,
            HeldImage {
                image,
                source,
                budget: snapshot.budget.clone(),
                alternate_screen: hold.alternate_screen,
            },
        );
    }
    true
}
