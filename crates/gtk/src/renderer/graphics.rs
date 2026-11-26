use super::Scene;
use gmux_protocol::graphics::{
    Graphics, Image, Layer, MAX_PIXEL_BYTES, MAX_PLACEMENTS, PixelFormat,
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

impl Textures {
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
        let [pixel_width, pixel_height] = graphics.cell_pixels;
        let sx = cw / f64::from(pixel_width);
        let sy = ch / f64::from(pixel_height);
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
            let x = f64::from(p.cell[0]) * cw + f64::from(p.offset[0]) * sx;
            let y = f64::from(p.cell[1]) * ch + f64::from(p.offset[1]) * sy;
            let width = f64::from(p.size[0]) * sx;
            let height = f64::from(p.size[1]) * sy;
            let dest = graphene::Rect::new(x as f32, y as f32, width as f32, height as f32);
            if viewport.intersection(&dest).is_none() {
                continue;
            }
            let full = [0, 0, image.width, image.height];
            if p.source == full {
                let tex = self
                    .full
                    .entry(image.key)
                    .or_insert_with(|| CachedTexture::new(image, full, 0));
                snapshot.append_scaled_texture(&tex.texture, gsk::ScalingFilter::Linear, &dest);
                continue;
            }
            let key = (image.key, p.source);
            let [left, top, source_width, source_height] = p.source;
            let bytes = source_width as usize * source_height as usize * image.format.channels();
            if !pruned && !self.crop_fits(bytes) && !self.crops.contains_key(&key) {
                self.release_unused_crops(graphics);
                pruned = true;
            }
            if let Some(tex) = self.crops.get(&key) {
                snapshot.append_scaled_texture(&tex.texture, gsk::ScalingFilter::Linear, &dest);
            } else if self.crop_fits(bytes) {
                let tex = CachedTexture::new(image, p.source, bytes);
                snapshot.append_scaled_texture(&tex.texture, gsk::ScalingFilter::Linear, &dest);
                self.crops.insert(key, tex);
                self.crop_bytes += bytes;
            } else {
                // Nearest sampling keeps texels outside the crop from bleeding into it.
                let tex = self
                    .full
                    .entry(image.key)
                    .or_insert_with(|| CachedTexture::new(image, full, 0));
                let tx = width / f64::from(source_width);
                let ty = height / f64::from(source_height);
                let bounds = graphene::Rect::new(
                    (x - f64::from(left) * tx) as f32,
                    (y - f64::from(top) * ty) as f32,
                    (f64::from(image.width) * tx) as f32,
                    (f64::from(image.height) * ty) as f32,
                );
                snapshot.push_clip(&dest);
                snapshot.append_scaled_texture(&tex.texture, gsk::ScalingFilter::Nearest, &bounds);
                snapshot.pop();
            }
        }
        snapshot.pop();
    }
}
