use gmux_protocol::glyphs::{Glyph, Horizontal, Point, Size, Vertical};
use gtk::{cairo, gdk, glib, graphene, gsk, prelude::*};
use std::collections::HashMap;

// Row nodes can retain textures, so keep admitted entries until their rows
// are invalidated. Once full, draw new variants as paths.
const MAX_RASTER_BYTES: usize = 2 * 1024 * 1024;
const MAX_RASTERS: usize = 2048;
const PHASES: f64 = 64.0;

#[derive(Default)]
pub(super) struct GlyphRasters {
    entries: HashMap<RasterKey, Raster>,
    bytes: usize,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct RasterKey {
    codepoint: char,
    color: [u32; 4],
    phase: [u8; 2],
    span: u8,
}

struct Raster {
    texture: gdk::MemoryTexture,
    bytes: usize,
}

pub(super) struct RasterCell {
    pub bounds: (f64, f64, f64),
    pub row_y: f64,
    pub origin: (f64, f64),
    pub scale: f64,
    pub span: u8,
    pub baseline: f64,
}

impl GlyphRasters {
    pub fn retain(&mut self, keep: impl Fn(char) -> bool) {
        self.entries.retain(|key, raster| {
            if keep(key.codepoint) {
                true
            } else {
                self.bytes -= raster.bytes;
                false
            }
        });
    }

    pub fn append(
        &mut self,
        snapshot: &gtk::Snapshot,
        path: &GlyphPath,
        glyph: &Glyph,
        cell: RasterCell,
        color: &gdk::RGBA,
    ) -> bool {
        let (x, width, height) = cell.bounds;
        let scale = cell.scale;
        let phase = [
            ((cell.origin.0 + x) * scale).rem_euclid(1.0),
            ((cell.origin.1 + cell.row_y) * scale).rem_euclid(1.0),
        ];
        // Share rasterizations across equivalent device-pixel positions.
        let key = RasterKey {
            codepoint: glyph.codepoint,
            color: [color.red(), color.green(), color.blue(), color.alpha()].map(f32::to_bits),
            phase: phase.map(|value| (value * PHASES).round().min(PHASES - 1.0) as u8),
            span: cell.span,
        };
        if !self.entries.contains_key(&key) {
            let [px, py] = key.phase.map(|value| f64::from(value) / PHASES);
            let w = (px + width * scale).ceil() as i32;
            let h = (py + height * scale).ceil() as i32;
            let bytes = w as usize * h as usize * 4;
            if self.entries.len() >= MAX_RASTERS || bytes > MAX_RASTER_BYTES - self.bytes {
                return false;
            }
            let Some(texture) = rasterize(w, h, |cr| {
                let glyph_snapshot = gtk::Snapshot::new();
                path.append(
                    &glyph_snapshot,
                    glyph,
                    (0.0, width, height),
                    cell.baseline,
                    color,
                );
                cr.translate(px, py);
                cr.scale(scale, scale);
                glyph_snapshot.to_node()?.draw(cr);
                Some(())
            }) else {
                return false;
            };
            self.entries.insert(key, Raster { texture, bytes });
            self.bytes += bytes;
        }
        let texture = &self.entries[&key].texture;
        snapshot.append_texture(
            texture,
            &graphene::Rect::new(
                (x - phase[0] / scale) as f32,
                (-phase[1] / scale) as f32,
                (f64::from(texture.width()) / scale) as f32,
                (f64::from(texture.height()) / scale) as f32,
            ),
        );
        true
    }
}

fn rasterize(
    w: i32,
    h: i32,
    paint: impl FnOnce(&cairo::Context) -> Option<()>,
) -> Option<gdk::MemoryTexture> {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
    let cr = cairo::Context::new(&surface).ok()?;
    paint(&cr)?;
    cr.status().ok()?;
    drop(cr);
    let stride = surface.stride() as usize;
    let bytes = glib::Bytes::from_owned(surface.take_data().ok()?);
    let format = if cfg!(target_endian = "little") {
        gdk::MemoryFormat::B8g8r8a8Premultiplied
    } else {
        gdk::MemoryFormat::A8r8g8b8Premultiplied
    };
    Some(gdk::MemoryTexture::new(w, h, format, &bytes, stride))
}

pub(super) struct GlyphPath {
    path: gsk::Path,
    extent: [f64; 4],
}

impl GlyphPath {
    pub fn new(glyph: &Glyph) -> Self {
        let builder = gsk::PathBuilder::new();
        for contour in glyph.outline.contours() {
            append_contour(&builder, contour);
        }
        let extent = if let Some(extent) = glyph.extent {
            [
                0.0,
                f64::from(extent.ymin),
                f64::from(extent.advance),
                f64::from(extent.line_height),
            ]
        } else {
            outline_extent(glyph.outline.points())
        };
        Self {
            path: builder.to_path(),
            extent,
        }
    }

    pub fn append(
        &self,
        snapshot: &gtk::Snapshot,
        glyph: &Glyph,
        (x, width, height): (f64, f64, f64),
        baseline: f64,
        color: &gdk::RGBA,
    ) {
        let [xmin, ymin, aw, lh] = self.extent;
        // An empty or degenerate outline has no ink.
        if aw <= 0.0 || lh <= 0.0 || self.path.is_empty() {
            return;
        }
        let [top, right, bottom, left] = glyph.pad;
        let inner_width = width * (1.0 - left - right);
        let inner_height = height * (1.0 - top - bottom);
        let sx = inner_width / aw;
        let sy = inner_height / lh;
        let (sx, sy) = match glyph.size {
            Size::Height => (sy, sy),
            Size::Advance => (sx, sx),
            Size::Contain => {
                let fit = sx.min(sy);
                let fit = if glyph.extent.is_none() {
                    fit.min(inner_width.min(inner_height) / f64::from(glyph.upm))
                } else {
                    fit
                };
                (fit, fit)
            }
            Size::Cover => (sx.max(sy), sx.max(sy)),
            Size::Stretch => (sx, sy),
        };
        let dx = inner_width - aw * sx;
        let dy = inner_height - lh * sy;
        let x_offset = x + width * left - xmin * sx
            + match glyph.horizontal {
                Horizontal::Start => 0.0,
                Horizontal::Center => dx * 0.5,
                Horizontal::End => dx,
            };
        // Paths use y-down coordinates. The extent's top is -(ymin + lh).
        let y_offset = match glyph.vertical {
            Vertical::Baseline => baseline,
            alignment => {
                height * top
                    + (ymin + lh) * sy
                    + match alignment {
                        Vertical::Start => dy,
                        Vertical::Center => dy * 0.5,
                        Vertical::End => 0.0,
                        Vertical::Baseline => unreachable!(),
                    }
            }
        };
        snapshot.push_clip(&graphene::Rect::new(
            x as f32,
            0.0,
            width as f32,
            height as f32,
        ));
        snapshot.save();
        snapshot.translate(&graphene::Point::new(x_offset as f32, y_offset as f32));
        snapshot.scale(sx as f32, sy as f32);
        snapshot.append_fill(&self.path, gsk::FillRule::Winding, color);
        snapshot.restore();
        snapshot.pop();
    }
}

fn outline_extent(mut points: impl Iterator<Item = Point>) -> [f64; 4] {
    let Some(first) = points.next() else {
        return [0.0; 4];
    };
    let (mut xmin, mut ymin) = (first.x, first.y);
    let (mut xmax, mut ymax) = (xmin, ymin);
    for point in points {
        xmin = xmin.min(point.x);
        ymin = ymin.min(point.y);
        xmax = xmax.max(point.x);
        ymax = ymax.max(point.y);
    }
    let (xmin, ymin) = (f64::from(xmin), f64::from(ymin));
    [xmin, ymin, f64::from(xmax) - xmin, f64::from(ymax) - ymin]
}

fn coordinates(point: Point) -> (f32, f32) {
    (point.x as f32, -(point.y as f32))
}

fn midpoint((x1, y1): (f32, f32), (x2, y2): (f32, f32)) -> (f32, f32) {
    ((x1 + x2) * 0.5, (y1 + y2) * 0.5)
}

fn append_contour(builder: &gsk::PathBuilder, mut points: impl DoubleEndedIterator<Item = Point>) {
    let (Some(first), Some(last)) = (points.next(), points.next_back()) else {
        return;
    };
    let start = if first.on_curve {
        coordinates(first)
    } else if last.on_curve {
        coordinates(last)
    } else {
        midpoint(coordinates(last), coordinates(first))
    };
    let rest = (!first.on_curve)
        .then_some(first)
        .into_iter()
        .chain(points)
        .chain((first.on_curve || !last.on_curve).then_some(last));
    builder.move_to(start.0, start.1);
    let mut control = None;
    for point in rest {
        let end = coordinates(point);
        if point.on_curve {
            if let Some((cx, cy)) = control.take() {
                builder.quad_to(cx, cy, end.0, end.1);
            } else {
                builder.line_to(end.0, end.1);
            }
        } else if let Some((cx, cy)) = control.replace(end) {
            let implied = midpoint((cx, cy), end);
            builder.quad_to(cx, cy, implied.0, implied.1);
        }
    }
    if let Some((cx, cy)) = control {
        builder.quad_to(cx, cy, start.0, start.1);
    }
    builder.close();
}
