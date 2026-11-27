use super::Scene;
use gmux_protocol::graphics::{
    Graphics, Image, Layer, MAX_PIXEL_BYTES, MAX_PLACEMENTS, PixelFormat, SOURCE_SUBPIXELS,
};
use gtk::{gdk, glib, graphene, gsk, prelude::*};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{Arc, Weak},
};

#[derive(Default)]
pub(super) struct Textures {
    full: HashMap<u64, CachedTexture>,
    crops: HashMap<(u64, [u32; 4]), CachedTexture>,
    crop_bytes: usize,
    crop_edges: HashMap<(u64, [u32; 4]), CropEdge>,
    crop_nodes: HashMap<CropKey, CachedCrop>,
}

#[derive(PartialEq, Eq, Hash)]
struct CropKey {
    image: u64,
    source: [u32; 4],
    rect: [u32; 4],
    device: [u64; 3],
}

struct CachedCrop {
    owner: Weak<Image>,
    node: gsk::RenderNode,
    painted: bool,
}

struct CropEdge {
    owner: Weak<Image>,
    texture: glib::WeakRef<gdk::MemoryTexture>,
}

struct CachedTexture {
    owner: Weak<Image>,
    texture: gdk::MemoryTexture,
    bytes: usize,
}

impl CachedTexture {
    fn new(image: &Arc<Image>, source: [u32; 4], bytes: usize) -> Self {
        Self {
            owner: Arc::downgrade(image),
            texture: texture(image, source),
            bytes,
        }
    }

    fn belongs_to(&self, image: Option<&Arc<Image>>) -> bool {
        image.is_some_and(|image| std::ptr::eq(self.owner.as_ptr(), Arc::as_ptr(image)))
    }
}

struct Pixels {
    image: Arc<Image>,
    range: Range<usize>,
}

impl AsRef<[u8]> for Pixels {
    fn as_ref(&self) -> &[u8] {
        &self.image.pixels[self.range.clone()]
    }
}

fn texture(image: &Arc<Image>, [left, top, width, height]: [u32; 4]) -> gdk::MemoryTexture {
    let channels = image.format.channels();
    let stride = image.width as usize * channels;
    let row_bytes = width as usize * channels;
    let start = top as usize * stride + left as usize * channels;
    let len = row_bytes * height as usize;
    let pixels = if width == image.width || height == 1 {
        glib::Bytes::from_owned(Pixels {
            image: image.clone(),
            range: start..start + len,
        })
    } else {
        let mut pixels = Vec::with_capacity(len);
        for row in 0..height as usize {
            let start = start + row * stride;
            pixels.extend_from_slice(&image.pixels[start..start + row_bytes]);
        }
        glib::Bytes::from_owned(pixels)
    };
    let format = match image.format {
        PixelFormat::Rgb => gdk::MemoryFormat::R8g8b8,
        PixelFormat::Rgba => gdk::MemoryFormat::R8g8b8a8,
        PixelFormat::GrayAlpha => gdk::MemoryFormat::G8a8,
        PixelFormat::Gray => gdk::MemoryFormat::G8,
    };
    gdk::MemoryTexture::new(width as i32, height as i32, format, &pixels, row_bytes)
}

fn project_texture(
    image: &Image,
    dest: &graphene::Rect,
    [x, y, width, height]: [f64; 4],
) -> graphene::Rect {
    let sx = f64::from(dest.width()) / width;
    let sy = f64::from(dest.height()) / height;
    graphene::Rect::new(
        (f64::from(dest.x()) - x * sx) as f32,
        (f64::from(dest.y()) - y * sy) as f32,
        (f64::from(image.width) * sx) as f32,
        (f64::from(image.height) * sy) as f32,
    )
}

fn append_clipped_texture(
    snapshot: &gtk::Snapshot,
    texture: &gdk::MemoryTexture,
    clip: &graphene::Rect,
    bounds: &graphene::Rect,
) {
    snapshot.push_clip(clip);
    snapshot.append_scaled_texture(texture, gsk::ScalingFilter::Linear, bounds);
    snapshot.pop();
}

fn append_crop_texture(
    snapshot: &gtk::Snapshot,
    scene: &Scene<'_>,
    texture: &gdk::MemoryTexture,
    bounds: &graphene::Rect,
    clip: Option<&graphene::Rect>,
) {
    let scale = scene.scale as f32;
    let phase_x = (scene.origin.0 * scene.scale).fract() as f32;
    let phase_y = (scene.origin.1 * scene.scale).fract() as f32;
    if scale == 1.0 && phase_x == 0.0 && phase_y == 0.0 {
        if let Some(clip) = clip {
            append_clipped_texture(snapshot, texture, clip, bounds);
        } else {
            snapshot.append_scaled_texture(texture, gsk::ScalingFilter::Linear, bounds);
        }
        return;
    }

    // Cairo rasterizes a scaled texture's intermediate in node coordinates.
    // Use device pixels so adjacent crop pieces share the same sample grid.
    let device = |rect: &graphene::Rect| {
        graphene::Rect::new(
            rect.x() * scale + phase_x,
            rect.y() * scale + phase_y,
            rect.width() * scale,
            rect.height() * scale,
        )
    };
    let node = gsk::TextureScaleNode::new(texture, &device(bounds), gsk::ScalingFilter::Linear);
    let transform = gsk::Transform::new()
        .translate(&graphene::Point::new(-phase_x / scale, -phase_y / scale))
        .scale(1.0 / scale, 1.0 / scale);
    if let Some(clip) = clip {
        let node = gsk::ClipNode::new(&node, &device(clip));
        snapshot.append_node(gsk::TransformNode::new(&node, Some(&transform)));
    } else {
        snapshot.append_node(gsk::TransformNode::new(&node, Some(&transform)));
    }
}

#[derive(Clone, Copy)]
struct CropSpan {
    source: [u32; 2],
    bounds: [f32; 2],
    clip: [f32; 2],
}

impl CropSpan {
    fn clip_limits(self) -> [f32; 2] {
        let start = self.clip[0];
        let end = start + self.clip[1];
        // A clip coincident with the texture edge applies its coverage twice
        // in Cairo. The texture bounds already cut off that side.
        [
            if start == self.bounds[0] {
                start - 1.0
            } else {
                start
            },
            if end == self.bounds[0] + self.bounds[1] {
                end + 1.0
            } else {
                end
            },
        ]
    }
}

fn crop_spans(
    start: f32,
    size: f32,
    [offset, count, total]: [u32; 3],
    origin: f64,
    device_scale: f64,
) -> [CropSpan; 3] {
    let start = f64::from(start);
    let end = start + f64::from(size);
    let scale = f64::from(size) / f64::from(count);
    // Round the internal joins between sample centers so adjacent nodes
    // cover whole output pixels even when the placement is translucent.
    let snap = |value: f64| {
        (((value + origin) * device_scale).round() / device_scale - origin).clamp(start, end)
    };
    let left = if offset == 0 {
        start
    } else {
        snap(start + 0.5 * scale)
    };
    let right = if offset + count == total {
        end
    } else {
        snap(end - 0.5 * scale)
    };
    let edge = |source, start, size| CropSpan {
        source: [source, 1],
        bounds: [start, size],
        clip: [start, size],
    };
    [
        edge(offset, start as f32, (left - start) as f32),
        CropSpan {
            source: [0, total],
            bounds: [
                (start - f64::from(offset) * scale) as f32,
                (scale * f64::from(total)) as f32,
            ],
            clip: [left as f32, (right - left) as f32],
        },
        edge(offset + count - 1, right as f32, (end - right) as f32),
    ]
}

fn append_unicode_texture(
    snapshot: &gtk::Snapshot,
    scene: &Scene<'_>,
    texture: &gdk::MemoryTexture,
    dest: &graphene::Rect,
    bounds: &graphene::Rect,
    row: i32,
) {
    let Some(bidi) = usize::try_from(row)
        .ok()
        .and_then(|row| scene.bidi?.row(row))
    else {
        append_clipped_texture(snapshot, texture, dest, bounds);
        return;
    };

    let cw = scene.metrics.0;
    let left = f64::from(dest.x());
    let right = left + f64::from(dest.width());
    let start = (left / cw).floor().max(0.0) as usize;
    let end = ((right / cw).ceil().max(0.0) as usize).min(scene.frame.cols.into());
    let mut column = start;
    while column < end {
        let cell_x = column as f64 * cw;
        let visual = bidi.visual(column);
        let shift = (visual as f64 - column as f64) * cw;
        let mut next = column + 1;
        while next < end && bidi.visual(next) == visual + (next - column) {
            next += 1;
        }
        let left = left.max(cell_x);
        let right = right.min(next as f64 * cw);
        let clip = graphene::Rect::new(
            (left + shift) as f32,
            dest.y(),
            (right - left) as f32,
            dest.height(),
        );
        let translated = graphene::Rect::new(
            bounds.x() + shift as f32,
            bounds.y(),
            bounds.width(),
            bounds.height(),
        );
        append_clipped_texture(snapshot, texture, &clip, &translated);
        column = next;
    }
}

impl Textures {
    pub fn finish_frame(&mut self) {
        self.crop_nodes
            .retain(|_, crop| std::mem::take(&mut crop.painted));
        self.crop_edges
            .retain(|_, edge| edge.texture.upgrade().is_some());
    }

    pub fn retain(&mut self, graphics: &Graphics) {
        if !self.full.is_empty() {
            self.full
                .retain(|&key, cached| cached.belongs_to(graphics.image(key)));
        }
        if !self.crops.is_empty() {
            self.crops.retain(|&(key, _), cached| {
                let keep = cached.belongs_to(graphics.image(key));
                if !keep {
                    self.crop_bytes -= cached.bytes;
                }
                keep
            });
        }
        if !self.crop_edges.is_empty() {
            self.crop_edges.retain(|&(key, _), edge| {
                graphics
                    .image(key)
                    .is_some_and(|image| edge.owner.as_ptr() == Arc::as_ptr(image))
            });
        }
        if !self.crop_nodes.is_empty() {
            self.crop_nodes.retain(|key, crop| {
                graphics
                    .image(key.image)
                    .is_some_and(|image| crop.owner.as_ptr() == Arc::as_ptr(image))
            });
        }
    }

    fn crop_fits(&self, bytes: usize) -> bool {
        self.crops.len() < MAX_PLACEMENTS && bytes <= MAX_PIXEL_BYTES - self.crop_bytes
    }

    fn release_unused_crops(&mut self, graphics: &Graphics) {
        let placements = graphics
            .placements
            .iter()
            .map(|p| (p.image, p.source))
            .collect::<HashSet<_>>();
        self.crops.retain(|key, cached| {
            let used = placements.contains(key);
            if !used {
                self.crop_bytes -= cached.bytes;
            }
            used
        });
    }

    fn append_uncached_crop(
        &mut self,
        snapshot: &gtk::Snapshot,
        scene: &Scene<'_>,
        image: &Arc<Image>,
        dest: &graphene::Rect,
        source: [u32; 4],
    ) {
        let key = CropKey {
            image: image.key,
            source,
            rect: [dest.x(), dest.y(), dest.width(), dest.height()].map(f32::to_bits),
            device: [scene.scale, scene.origin.0, scene.origin.1].map(f64::to_bits),
        };
        if let Some(crop) = self.crop_nodes.get_mut(&key) {
            crop.painted = true;
            snapshot.append_node(&crop.node);
            return;
        }

        let crop_snapshot = gtk::Snapshot::new();
        let [left, top, width, height] = source;
        let full = [0, 0, image.width, image.height];
        let columns = crop_spans(
            dest.x(),
            dest.width(),
            [left, width, image.width],
            scene.origin.0,
            scene.scale,
        );
        let rows = crop_spans(
            dest.y(),
            dest.height(),
            [top, height, image.height],
            scene.origin.1,
            scene.scale,
        );
        // The interior can sample the original image. At a crop boundary,
        // stretch the edge texel so linear filtering cannot sample outside it.
        for y in rows.iter().filter(|y| y.clip[1] > 0.0) {
            for x in columns.iter().filter(|x| x.clip[1] > 0.0) {
                let source = [x.source[0], y.source[0], x.source[1], y.source[1]];
                let texture = if source == full {
                    self.full
                        .entry(image.key)
                        .or_insert_with(|| CachedTexture::new(image, full, 0))
                        .texture
                        .clone()
                } else {
                    let edge = self
                        .crop_edges
                        .entry((image.key, source))
                        .or_insert_with(|| CropEdge {
                            owner: Arc::downgrade(image),
                            texture: glib::WeakRef::new(),
                        });
                    edge.texture.upgrade().unwrap_or_else(|| {
                        let texture = texture(image, source);
                        edge.texture.set(Some(&texture));
                        texture
                    })
                };
                let bounds =
                    graphene::Rect::new(x.bounds[0], y.bounds[0], x.bounds[1], y.bounds[1]);
                let [left, right] = x.clip_limits();
                let [top, bottom] = y.clip_limits();
                let clip = graphene::Rect::new(left, top, right - left, bottom - top);
                append_crop_texture(&crop_snapshot, scene, &texture, &bounds, Some(&clip));
            }
        }
        if let Some(node) = crop_snapshot.to_node() {
            snapshot.append_node(&node);
            self.crop_nodes.insert(
                key,
                CachedCrop {
                    owner: Arc::downgrade(image),
                    node,
                    painted: true,
                },
            );
        }
    }

    pub fn append(
        &mut self,
        snapshot: &gtk::Snapshot,
        scene: &Scene<'_>,
        graphics: &Graphics,
        layer: Layer,
    ) {
        let start = graphics.placements.partition_point(|p| p.layer < layer);
        let end = graphics.placements.partition_point(|p| p.layer <= layer);
        if start == end {
            return;
        }
        let (cw, ch) = scene.metrics;
        let viewport = graphene::Rect::new(
            0.0,
            0.0,
            (cw * f64::from(scene.frame.cols)).min(f64::from(scene.width)) as f32,
            (ch * f64::from(scene.frame.rows)).min(f64::from(scene.height)) as f32,
        );
        snapshot.push_clip(&viewport);
        let mut pruned = false;
        for p in &graphics.placements[start..end] {
            let Some(image) = graphics.image(p.image) else {
                continue;
            };
            let pixels = if p.source_subpixels.is_some() {
                graphics.unicode_cell_pixels.unwrap_or(graphics.cell_pixels)
            } else {
                graphics.cell_pixels
            };
            let bounds = p.viewport_bounds([cw, ch], pixels, graphics.frozen_cell_pixels);
            let [x, y, width, height] = bounds.rect.map(|value| value as f32);
            let dest = graphene::Rect::new(x, y, width, height);
            let reordered_tile = p.source_subpixels.is_some()
                && usize::try_from(p.cell[1])
                    .ok()
                    .and_then(|row| scene.bidi?.row(row))
                    .is_some();
            if y >= viewport.height()
                || y + height <= 0.0
                || (!reordered_tile && viewport.intersection(&dest).is_none())
            {
                continue;
            }
            let clip = bounds.clip_y.map(|[top, bottom]| {
                graphene::Rect::new(0.0, top as f32, viewport.width(), (bottom - top) as f32)
            });
            if let Some(clip) = &clip {
                if clip.intersection(&dest).is_none() {
                    continue;
                }
                snapshot.push_clip(clip);
            }
            let full = [0, 0, image.width, image.height];
            if let Some(source) = p.source_subpixels {
                let tex = self
                    .full
                    .entry(image.key)
                    .or_insert_with(|| CachedTexture::new(image, full, 0));
                let source = source.map(|value| f64::from(value) / f64::from(SOURCE_SUBPIXELS));
                let bounds = project_texture(image, &dest, source);
                append_unicode_texture(snapshot, scene, &tex.texture, &dest, &bounds, p.cell[1]);
            } else if p.source == full {
                let tex = self
                    .full
                    .entry(image.key)
                    .or_insert_with(|| CachedTexture::new(image, full, 0));
                snapshot.append_scaled_texture(&tex.texture, gsk::ScalingFilter::Linear, &dest);
            } else {
                let key = (image.key, p.source);
                let [_, _, source_width, source_height] = p.source;
                let bytes =
                    source_width as usize * source_height as usize * image.format.channels();
                if !pruned && !self.crop_fits(bytes) && !self.crops.contains_key(&key) {
                    self.release_unused_crops(graphics);
                    pruned = true;
                }
                if let Some(tex) = self.crops.get(&key) {
                    append_crop_texture(snapshot, scene, &tex.texture, &dest, None);
                } else if self.crop_fits(bytes) {
                    let tex = CachedTexture::new(image, p.source, bytes);
                    append_crop_texture(snapshot, scene, &tex.texture, &dest, None);
                    self.crops.insert(key, tex);
                    self.crop_bytes += bytes;
                } else {
                    self.append_uncached_crop(snapshot, scene, image, &dest, p.source);
                }
            }
            if clip.is_some() {
                snapshot.pop();
            }
        }
        snapshot.pop();
    }
}
