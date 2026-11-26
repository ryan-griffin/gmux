use super::{
    Events, GMUX_GRAPHICS_MAX_PIXEL_BYTES, GMUX_GRAPHICS_MAX_PLACEMENTS, GmuxEngine, GmuxImage,
    GmuxImagePlacement, GmuxView, gmux_engine_graphics, gmux_engine_image,
};
use gmux_protocol::graphics::{Graphics, Image, Layer, PixelFormat, Placement};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    ffi::c_void,
    mem::MaybeUninit,
    ptr, slice,
    sync::{Arc, Weak},
};

const _: () =
    assert!(GMUX_GRAPHICS_MAX_PIXEL_BYTES as usize == gmux_protocol::graphics::MAX_PIXEL_BYTES);
const _: () =
    assert!(GMUX_GRAPHICS_MAX_PLACEMENTS as usize == gmux_protocol::graphics::MAX_PLACEMENTS);

#[derive(Default)]
pub(super) struct Snapshot {
    generation: u64,
    cell_pixels: [u32; 2],
    image_ids: Vec<u32>,
    pub graphics: Option<Arc<Graphics>>,
}

#[derive(Default)]
pub(super) struct GraphicsState {
    pub held: Snapshot,
    pub pixels: HashMap<u64, Weak<Image>>,
}

impl Snapshot {
    pub fn refresh(
        &mut self,
        engine: *const GmuxEngine,
        view: *const GmuxView,
        pixels: &mut HashMap<u64, Weak<Image>>,
    ) -> bool {
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
            return false;
        }
        self.generation = generation;
        self.cell_pixels = cell_pixels;
        let newly_visible = raw
            .iter()
            .any(|placement| self.image_ids.binary_search(&placement.id).is_err());
        self.image_ids.clear();
        if raw.is_empty() {
            self.graphics = None;
            pixels.retain(|_, image| image.strong_count() != 0);
            return false;
        }
        raw.sort_unstable_by_key(|p| (p.layer, p.z, p.id, p.cell, p.offset, p.source, p.size));
        let mut images = BTreeMap::new();
        let mut placements = Vec::with_capacity(raw.len());
        for p in raw {
            let image = images.entry(p.generation).or_insert_with(|| {
                pixels
                    .get(&p.generation)
                    .and_then(Weak::upgrade)
                    .or_else(|| {
                        let image = read_image(engine, p.id, p.generation)?;
                        pixels.insert(p.generation, Arc::downgrade(&image));
                        Some(image)
                    })
            });
            if image.is_none() {
                continue;
            }
            self.image_ids.push(p.id);
            placements.push(Placement {
                image: p.generation,
                layer: [Layer::BelowBackground, Layer::BelowText, Layer::AboveText]
                    [p.layer as usize],
                cell: p.cell,
                offset: p.offset,
                size: p.size,
                source: p.source,
            });
        }
        self.image_ids.sort_unstable();
        self.image_ids.dedup();
        if placements.is_empty() {
            self.graphics = None;
        } else if !self
            .graphics
            .as_ref()
            .is_some_and(|old| old.cell_pixels == cell_pixels && old.placements == placements)
        {
            self.graphics = Some(Arc::new(Graphics {
                cell_pixels,
                placements,
                images: images.into_values().flatten().collect(),
            }));
        }
        pixels.retain(|_, image| image.strong_count() != 0);
        newly_visible
    }
}

fn read_image(engine: *const GmuxEngine, id: u32, key: u64) -> Option<Arc<Image>> {
    let mut raw = MaybeUninit::<GmuxImage>::uninit();
    if !unsafe { gmux_engine_image(engine, id, key, raw.as_mut_ptr()) } {
        return None;
    }
    let raw = unsafe { raw.assume_init() };
    let mut image = Image {
        key,
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
    unsafe { out.write(snapshot.image_ids.as_ptr()) };
    snapshot.image_ids.len()
}

pub(super) unsafe extern "C" fn graphics_hold(
    data: *mut c_void,
    engine: *const GmuxEngine,
    view: *const GmuxView,
    viewdata: *mut c_void,
    held: bool,
) {
    let state = &mut unsafe { &mut *data.cast::<Events>() }.graphics;
    if !held {
        state.held = Snapshot::default();
        state.pixels.retain(|_, image| image.strong_count() != 0);
    } else {
        let mut refresh = |capture: &mut Snapshot| {
            // The held and live render states have independent generation counters.
            capture.generation = u64::MAX;
            capture.refresh(engine, view, &mut state.pixels);
            capture.generation = u64::MAX;
        };
        if viewdata.is_null() {
            refresh(&mut state.held);
        } else {
            let view = unsafe { &*viewdata.cast::<RefCell<Snapshot>>() };
            refresh(&mut view.borrow_mut());
        }
    }
}
