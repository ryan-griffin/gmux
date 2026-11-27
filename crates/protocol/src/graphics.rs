//! Decoded images and their placements in a single terminal viewport.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, mem::size_of, sync::Arc};

pub const MAX_PIXEL_BYTES: usize = 10_000_000;
pub const MAX_PLACEMENTS: usize = 4096;
pub const SOURCE_SUBPIXELS: u32 = 1 << 16;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    Rgb,
    Rgba,
    GrayAlpha,
    Gray,
}

impl PixelFormat {
    pub fn channels(self) -> usize {
        match self {
            Self::Rgb => 3,
            Self::Rgba => 4,
            Self::GrayAlpha => 2,
            Self::Gray => 1,
        }
    }
}

pub struct Image {
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub pixels: Vec<u8>,
}

impl Serialize for Image {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (self.key, self.width, self.height, self.format).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Image {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (key, width, height, format) = Deserialize::deserialize(deserializer)?;
        Ok(Self {
            key,
            width,
            height,
            format,
            pixels: Vec::new(),
        })
    }
}

impl Image {
    pub fn pixel_len(&self) -> anyhow::Result<usize> {
        anyhow::ensure!(
            self.key != 0
                && (1..=10_000).contains(&self.width)
                && (1..=10_000).contains(&self.height),
            "Invalid terminal image dimensions or key"
        );
        let len = self.width as usize * self.height as usize * self.format.channels();
        anyhow::ensure!(
            len <= MAX_PIXEL_BYTES,
            "Terminal image exceeds its pixel budget"
        );
        Ok(len)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    BelowBackground,
    BelowText,
    AboveText,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSizing {
    Columns,
    Rows,
    Native,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub image: u64,
    pub layer: Layer,
    pub cell: [i32; 2],
    pub offset: [u32; 2],
    pub size: [u32; 2],
    pub source: [u32; 4],
    /// Optional Unicode crop in 16.16 source pixels.
    pub source_subpixels: Option<[u32; 4]>,
    /// For an aspect-preserving sizing box, the trimmed top and visible row counts.
    pub fit_rows: Option<[u32; 2]>,
    pub sizing: Option<ImageSizing>,
}

pub struct PlacementBounds {
    pub rect: [f64; 4],
    pub clip_y: Option<[f64; 2]>,
}

impl Placement {
    pub fn bounds(&self, [cw, ch]: [f64; 2], [pixel_w, pixel_h]: [u32; 2]) -> PlacementBounds {
        let sx = cw / f64::from(pixel_w);
        let sy = ch / f64::from(pixel_h);
        let row_y = f64::from(self.cell[1]) * ch;
        let mut x = f64::from(self.cell[0]) * cw + f64::from(self.offset[0]) * sx;
        let mut y = row_y + f64::from(self.offset[1]) * sy;
        let mut width = f64::from(self.size[0]) * sx;
        let mut height = f64::from(self.size[1]) * sy;
        let mut clip_y = None;
        let source_w = f64::from(self.source[2]);
        let source_h = f64::from(self.source[3]);
        if let Some([trim_top, rows]) = self.fit_rows {
            let scale = (width / source_w).min(height / source_h);
            let fitted_width = source_w * scale;
            let fitted_height = source_h * scale;
            x += (width - fitted_width) / 2.0;
            y += (height - fitted_height) / 2.0 - f64::from(trim_top) * ch;
            width = fitted_width;
            height = fitted_height;
            let bottom = row_y + f64::from(rows) * ch;
            if y < row_y || y + height > bottom {
                clip_y = Some([row_y, bottom]);
            }
        } else {
            match self.sizing {
                Some(ImageSizing::Columns) => height = width * source_h / source_w,
                Some(ImageSizing::Rows) => width = height * source_w / source_h,
                Some(ImageSizing::Native) => [width, height] = self.size.map(f64::from),
                None => {}
            }
        }
        PlacementBounds {
            rect: [x, y, width, height],
            clip_y,
        }
    }
}

impl Serialize for Placement {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (
            self.image,
            self.layer,
            self.cell,
            self.offset,
            self.size,
            self.source,
            self.source_subpixels,
            self.fit_rows,
            self.sizing,
        )
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Placement {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (image, layer, cell, offset, size, source, source_subpixels, fit_rows, sizing) =
            Deserialize::deserialize(deserializer)?;
        Ok(Self {
            image,
            layer,
            cell,
            offset,
            size,
            source,
            source_subpixels,
            fit_rows,
            sizing,
        })
    }
}

#[derive(Default)]
pub struct Graphics {
    pub cell_pixels: [u32; 2],
    pub placements: Vec<Placement>,
    /// Sorted by generation.
    pub images: Vec<Arc<Image>>,
}

impl Graphics {
    pub fn image(&self, key: u64) -> Option<&Arc<Image>> {
        find_image(&self.images, key)
    }
}

fn find_image(images: &[Arc<Image>], key: u64) -> Option<&Arc<Image>> {
    let index = images.binary_search_by_key(&key, |image| image.key).ok()?;
    Some(&images[index])
}

/// Replace the placement list and supply only the pixels absent from the previous viewport.
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraphicsUpdate {
    pub replace: bool,
    pub cell_pixels: [u32; 2],
    pub placements: Vec<Placement>,
    pub images: Vec<Arc<Image>>,
}

impl GraphicsUpdate {
    pub fn payload_len(&self) -> anyhow::Result<usize> {
        anyhow::ensure!(
            self.placements.len() <= MAX_PLACEMENTS
                && self.images.len() <= self.placements.len()
                && (self.placements.is_empty() || !self.cell_pixels.contains(&0))
                && self
                    .placements
                    .windows(2)
                    .all(|pair| pair[0].layer <= pair[1].layer)
                && self.images.windows(2).all(|pair| pair[0].key < pair[1].key),
            "Invalid terminal graphics update"
        );
        let mut len = 0;
        for image in &self.images {
            len += image.pixel_len()?;
            anyhow::ensure!(
                len <= MAX_PIXEL_BYTES,
                "Terminal images exceed their pixel budget"
            );
        }
        Ok(len)
    }

    pub(crate) fn memory_size(&self) -> usize {
        self.placements.capacity() * size_of::<Placement>()
            + self.images.capacity() * size_of::<Arc<Image>>()
            + self
                .images
                .iter()
                .map(|image| size_of::<Image>() + 2 * size_of::<usize>() + image.pixels.capacity())
                .sum::<usize>()
    }

    pub fn apply(self, previous: &mut Graphics) -> anyhow::Result<()> {
        let mut images = BTreeMap::new();
        let mut added = 0;
        let mut bytes = 0;
        for placement in &self.placements {
            let image = match images.entry(placement.image) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let image = if let Some(image) = find_image(&self.images, placement.image) {
                        added += 1;
                        image
                    } else {
                        anyhow::ensure!(!self.replace, "Replacement omitted terminal image pixels");
                        previous
                            .image(placement.image)
                            .ok_or_else(|| anyhow::anyhow!("Missing terminal image pixels"))?
                    };
                    bytes += image.pixels.len();
                    anyhow::ensure!(
                        bytes <= MAX_PIXEL_BYTES,
                        "Terminal images exceed their retained pixel budget"
                    );
                    entry.insert(image.clone())
                }
            };
            let [x, y, width, height] = placement.source;
            anyhow::ensure!(
                !placement.size.contains(&0)
                    && width != 0
                    && height != 0
                    && x.checked_add(width).is_some_and(|end| end <= image.width)
                    && y.checked_add(height).is_some_and(|end| end <= image.height),
                "Invalid terminal image placement"
            );
            if let Some([x, y, width, height]) = placement.source_subpixels {
                anyhow::ensure!(
                    width != 0
                        && height != 0
                        && x.checked_add(width)
                            .is_some_and(|end| end <= image.width * SOURCE_SUBPIXELS)
                        && y.checked_add(height)
                            .is_some_and(|end| end <= image.height * SOURCE_SUBPIXELS),
                    "Invalid fractional terminal image placement"
                );
            }
            if let Some([_, rows]) = placement.fit_rows {
                anyhow::ensure!(
                    rows != 0 && placement.source_subpixels.is_none(),
                    "Invalid terminal image sizing box"
                );
            }
            anyhow::ensure!(
                placement.sizing.is_none()
                    || (placement.fit_rows.is_none() && placement.source_subpixels.is_none()),
                "Conflicting terminal image sizing"
            );
        }
        anyhow::ensure!(
            added == self.images.len(),
            "Unreferenced terminal image pixels"
        );
        *previous = Graphics {
            cell_pixels: self.cell_pixels,
            placements: self.placements,
            images: images.into_values().collect(),
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Cell, Frame,
        transport::{Packet, PaneHeader, PaneUpdate},
    };

    #[test]
    fn full_text_with_search_and_graphics_fits_one_record() {
        let (cols, rows) = crate::viewport::constrain(1000, 1000);
        let mut cell = Cell::default();
        cell.set_text(&format!("🎵{}", "\u{1d185}".repeat(64)))
            .unwrap();
        let image = Arc::new(Image {
            key: 1,
            width: 2500,
            height: 1000,
            format: PixelFormat::Rgba,
            pixels: vec![255; MAX_PIXEL_BYTES],
        });
        let placement = Placement {
            image: image.key,
            layer: Layer::AboveText,
            cell: [0, 0],
            offset: [0, 0],
            size: [1, 1],
            source: [0, 0, 1, 1],
            source_subpixels: None,
            fit_rows: None,
            sizing: None,
        };
        let packet = Packet::Pane(PaneUpdate {
            header: PaneHeader {
                frame: Some(Frame {
                    cols,
                    rows,
                    ..Frame::default()
                }),
                rows: (0..rows).collect(),
                find: Some(crate::search::FindState {
                    total: 65_500,
                    highlights: (0..65_500)
                        .map(|index| (index * 2, index * 2 + 1, 1))
                        .collect(),
                    ..Default::default()
                }),
                graphics: Some(GraphicsUpdate {
                    replace: true,
                    cell_pixels: [8, 16],
                    placements: vec![placement; MAX_PLACEMENTS],
                    images: vec![image],
                }),
                ..PaneHeader::default()
            },
            cells: vec![cell; usize::from(cols) * usize::from(rows)],
        });
        let mut wire = Vec::new();
        packet.encode_into(&mut wire).unwrap();
        let framing = crate::transport::Record::decode(
            wire[..crate::transport::RECORD_PREFIX_BYTES]
                .try_into()
                .unwrap(),
        )
        .unwrap();
        assert!(framing.decode_bytes() > 64 * 1024 * 1024);
        let decoded = Packet::read(&mut wire.as_slice()).unwrap();
        assert!(framing.validate_packet(&decoded).unwrap() < 64 * 1024 * 1024);
    }

    #[test]
    fn image_pixels_follow_cells_and_can_be_reused_by_a_later_frame() {
        let mut placement = Placement {
            image: 1,
            layer: Layer::BelowText,
            cell: [0, -1],
            offset: [1, 2],
            size: [8, 32],
            source: [0, 0, 1, 2],
            source_subpixels: None,
            fit_rows: None,
            sizing: None,
        };
        let packet = Packet::Pane(PaneUpdate {
            header: PaneHeader {
                frame: Some(Frame {
                    cols: 1,
                    rows: 1,
                    ..Frame::default()
                }),
                rows: vec![0],
                graphics: Some(GraphicsUpdate {
                    replace: true,
                    cell_pixels: [8, 16],
                    placements: vec![placement],
                    images: vec![Arc::new(Image {
                        key: 1,
                        width: 1,
                        height: 2,
                        format: PixelFormat::Rgb,
                        pixels: vec![255, 0, 0, 0, 0, 255],
                    })],
                }),
                ..PaneHeader::default()
            },
            cells: vec![Cell::default()],
        });
        let mut wire = Vec::new();
        packet.encode_into(&mut wire).unwrap();
        assert!(Packet::read(&mut &wire[..wire.len() - 1]).is_err());
        #[cfg(feature = "async")]
        let decoded =
            futures_lite::future::block_on(Packet::read_message_async(&mut wire.as_slice()))
                .unwrap()
                .unwrap();
        #[cfg(not(feature = "async"))]
        let decoded = Packet::read(&mut wire.as_slice()).unwrap();
        let Packet::Pane(mut pane) = decoded else {
            panic!("Expected a pane packet")
        };
        assert_eq!(pane.cells.len(), 1);
        let mut graphics = Graphics::default();
        pane.header
            .graphics
            .take()
            .unwrap()
            .apply(&mut graphics)
            .unwrap();
        let pixels = graphics.image(1).unwrap().clone();
        assert_eq!(pixels.pixels, [255, 0, 0, 0, 0, 255]);
        placement.cell[1] = 0;
        GraphicsUpdate {
            replace: false,
            cell_pixels: [8, 16],
            placements: vec![placement],
            images: vec![],
        }
        .apply(&mut graphics)
        .unwrap();
        assert!(Arc::ptr_eq(&pixels, graphics.image(1).unwrap()));
        assert_eq!(graphics.placements[0].cell, [0, 0]);

        placement.source[2] = 2;
        assert!(
            GraphicsUpdate {
                replace: false,
                cell_pixels: [8, 16],
                placements: vec![placement],
                images: vec![],
            }
            .apply(&mut graphics)
            .is_err()
        );
        assert_eq!(graphics.placements[0].source, [0, 0, 1, 2]);
    }
}
