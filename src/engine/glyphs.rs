use super::{Events, GmuxEngine, GmuxGlyph, gmux_engine_glyphs};
use gmux_protocol::glyphs::{Extent, Glyph, Glyphs, Horizontal, Outline, Point, Size, Vertical};
use std::{ffi::c_void, slice, sync::Arc};

#[derive(Clone, Default)]
pub(super) struct Snapshot {
    pub revision: u64,
    pub glyphs: Glyphs,
}

#[derive(Default)]
pub(super) struct GlyphState {
    pub live: Snapshot,
    pub held: Snapshot,
}

impl Snapshot {
    pub fn refresh(&mut self, engine: *const GmuxEngine) {
        struct Capture<'a> {
            previous: &'a Glyphs,
            next: Vec<Arc<Glyph>>,
        }

        unsafe extern "C" fn collect(data: *mut c_void, raw: *const GmuxGlyph) {
            // Native slices and metrics are borrowed only for this callback.
            let (capture, raw) = unsafe { (&mut *data.cast::<Capture<'_>>(), &*raw) };
            let cp = char::from_u32(raw.codepoint).expect("registered glyph codepoint");
            if let Some(glyph) = capture.previous.get(cp)
                && glyph.revision == raw.revision
            {
                capture.next.push(glyph.clone());
                return;
            }
            let outline = if raw.point_count == 0 {
                Outline::new(std::iter::empty())
            } else {
                let points = unsafe { slice::from_raw_parts(raw.points, raw.point_count) };
                let contours = unsafe { slice::from_raw_parts(raw.contours, raw.contour_count) };
                let mut ends = contours.iter().copied().map(usize::from).peekable();
                Outline::new(points.iter().enumerate().map(|(index, point)| {
                    let contour_end = ends.next_if_eq(&index).is_some();
                    Point {
                        x: point.x,
                        y: point.y,
                        on_curve: point.on_curve,
                        contour_end,
                    }
                }))
            };
            capture.next.push(Arc::new(Glyph {
                codepoint: cp,
                upm: raw.upm,
                extent: raw.declared_extent.then_some(Extent {
                    advance: raw.advance,
                    line_height: raw.line_height,
                    ymin: raw.ymin,
                }),
                width: raw.width,
                size: [
                    Size::Height,
                    Size::Advance,
                    Size::Contain,
                    Size::Cover,
                    Size::Stretch,
                ][usize::from(raw.size)],
                horizontal: [Horizontal::Start, Horizontal::Center, Horizontal::End]
                    [usize::from(raw.horizontal)],
                vertical: [
                    Vertical::Start,
                    Vertical::Center,
                    Vertical::End,
                    Vertical::Baseline,
                ][usize::from(raw.vertical)],
                pad: raw.pad,
                outline,
                revision: raw.revision,
            }));
        }

        let mut capture = Capture {
            previous: &self.glyphs,
            next: Vec::new(),
        };
        let revision = unsafe {
            gmux_engine_glyphs(
                engine,
                self.revision,
                (&mut capture as *mut Capture<'_>).cast(),
                Some(collect),
            )
        };
        if revision != self.revision {
            self.revision = revision;
            self.glyphs = Glyphs::new(capture.next);
        }
    }
}

pub(super) unsafe extern "C" fn glyph_hold(
    data: *mut c_void,
    engine: *const GmuxEngine,
    held: bool,
) {
    let state = &mut unsafe { &mut *data.cast::<Events>() }.glyphs;
    if held {
        state.live.refresh(engine);
        state.held = state.live.clone();
    } else {
        state.held = Snapshot::default();
    }
}
