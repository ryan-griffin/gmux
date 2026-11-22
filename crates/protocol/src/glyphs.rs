//! Pane-local registered outlines, separate from the terminal cell buffer.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{mem::size_of, sync::Arc};

const MAX_GLOSSARY_BYTES: usize = 512 * 1024;
const MAX_OUTLINE_BYTES: usize = 64 * 1024;
const SLOT_COUNT: usize = 1024;
const POINT_BYTES: usize = 9;

fn slot(cp: char) -> bool {
    ('\u{100000}'..='\u{1003ff}').contains(&cp)
}

pub fn cell_codepoint(text: &str) -> Option<char> {
    if matches!(text.as_bytes(), [0xf4, 0x80, 0x80..=0x8f, ..]) {
        text.chars().next()
    } else {
        None
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Glyphs(Arc<[Arc<Glyph>]>);

impl Glyphs {
    pub fn new(mut glyphs: Vec<Arc<Glyph>>) -> Self {
        glyphs.sort_unstable_by_key(|glyph| glyph.codepoint);
        Self(glyphs.into())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn same(&self, other: &Self) -> bool {
        (self.is_empty() && other.is_empty()) || Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn get(&self, cp: char) -> Option<&Arc<Glyph>> {
        let index = self
            .0
            .binary_search_by_key(&cp, |glyph| glyph.codepoint)
            .ok()?;
        Some(&self.0[index])
    }

    /// Share unchanged entries and keep the previous snapshot on an exact replay.
    pub fn reuse_entries(&mut self, previous: &Self) {
        let Some(glyphs) = Arc::get_mut(&mut self.0) else {
            return;
        };
        for glyph in &mut *glyphs {
            if let Some(old) = previous.get(glyph.codepoint)
                && **old == **glyph
            {
                *glyph = old.clone();
            }
        }
        if glyphs.len() == previous.0.len()
            && glyphs
                .iter()
                .zip(previous.0.iter())
                .all(|(next, old)| Arc::ptr_eq(next, old))
        {
            self.0.clone_from(&previous.0);
        }
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.0.len() <= SLOT_COUNT, "Too many registered glyphs");
        let mut bytes = 0;
        let mut previous = None;
        for glyph in &*self.0 {
            anyhow::ensure!(
                slot(glyph.codepoint)
                    && previous.is_none_or(|cp| cp < glyph.codepoint)
                    && glyph.upm != 0
                    && glyph
                        .extent
                        .is_none_or(|extent| extent.advance > 0 && extent.line_height > 0)
                    && (1..=2).contains(&glyph.width)
                    && glyph.pad.iter().all(|value| (0.0..=1.0).contains(value))
                    && glyph.pad[0] + glyph.pad[2] < 1.0
                    && glyph.pad[1] + glyph.pad[3] < 1.0,
                "Invalid registered glyph"
            );
            bytes += glyph.outline.decoded_size();
            previous = Some(glyph.codepoint);
        }
        anyhow::ensure!(
            bytes <= MAX_GLOSSARY_BYTES,
            "Glyph glossary exceeds its allocation limit"
        );
        Ok(())
    }

    pub(crate) fn memory_size(&self) -> usize {
        2 * size_of::<usize>()
            + self.0.len() * (size_of::<Arc<Glyph>>() + size_of::<Glyph>() + 2 * size_of::<usize>())
            + self
                .0
                .iter()
                .map(|glyph| glyph.outline.0.len())
                .sum::<usize>()
    }
}

#[derive(PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Glyph {
    pub codepoint: char,
    pub upm: u32,
    pub extent: Option<Extent>,
    pub width: u8,
    pub size: Size,
    pub horizontal: Horizontal,
    pub vertical: Vertical,
    pub pad: [f64; 4],
    pub outline: Outline,
    #[serde(skip)]
    pub revision: u64,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extent {
    pub advance: u32,
    pub line_height: u32,
    pub ymin: i32,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    Height,
    Advance,
    Contain,
    Cover,
    Stretch,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Horizontal {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vertical {
    Start,
    Center,
    End,
    Baseline,
}

#[derive(Clone, Copy)]
pub struct Point {
    pub x: i32,
    pub y: i32,
    pub on_curve: bool,
    pub contour_end: bool,
}

/// Each point is two little-endian i32 coordinates and one byte of flags.
/// The last point of each contour carries its end flag.
#[derive(PartialEq)]
pub struct Outline(Box<[u8]>);

impl Outline {
    pub fn new(points: impl ExactSizeIterator<Item = Point>) -> Self {
        let mut bytes = Vec::with_capacity(points.len() * POINT_BYTES);
        for point in points {
            bytes.extend_from_slice(&point.x.to_le_bytes());
            bytes.extend_from_slice(&point.y.to_le_bytes());
            bytes.push(u8::from(point.on_curve) | (u8::from(point.contour_end) << 1));
        }
        Self(bytes.into_boxed_slice())
    }

    pub fn points(&self) -> impl Iterator<Item = Point> + '_ {
        self.0.as_chunks::<POINT_BYTES>().0.iter().map(decode_point)
    }

    pub fn contours(
        &self,
    ) -> impl Iterator<Item = impl DoubleEndedIterator<Item = Point> + '_> + '_ {
        self.0
            .as_chunks::<POINT_BYTES>()
            .0
            .split_inclusive(|point| point[8] & 2 != 0)
            .map(|points| points.iter().map(decode_point))
    }

    fn decoded_size(&self) -> usize {
        self.0
            .as_chunks::<POINT_BYTES>()
            .0
            .iter()
            .map(|point| 12 + if point[8] & 2 != 0 { 2 } else { 0 })
            .sum()
    }
}

fn decode_point(bytes: &[u8; POINT_BYTES]) -> Point {
    Point {
        x: i32::from_le_bytes(bytes[..4].try_into().unwrap()),
        y: i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        on_curve: bytes[8] & 1 != 0,
        contour_end: bytes[8] & 2 != 0,
    }
}

impl Serialize for Outline {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&base64::display::Base64Display::new(&self.0, &STANDARD))
    }
}

impl<'de> Deserialize<'de> for Outline {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(OutlineVisitor)
    }
}

struct OutlineVisitor;

impl serde::de::Visitor<'_> for OutlineVisitor {
    type Value = Outline;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a base64 glyph outline")
    }

    fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Outline, E> {
        let max_encoded = (MAX_OUTLINE_BYTES / 12 * POINT_BYTES).div_ceil(3) * 4;
        if text.len() > max_encoded {
            return Err(E::custom("Glyph outline is too large"));
        }
        let bytes = STANDARD.decode(text.as_bytes()).map_err(E::custom)?;
        let (points, remainder) = bytes.as_chunks::<POINT_BYTES>();
        if !remainder.is_empty()
            || points.last().is_some_and(|point| point[8] & 2 == 0)
            || points.iter().any(|point| point[8] > 3)
        {
            return Err(E::custom("Invalid glyph outline"));
        }
        if points.len() * 12 > MAX_OUTLINE_BYTES {
            return Err(E::custom(
                "Glyph outline expands beyond its allocation limit",
            ));
        }
        Ok(Outline(bytes.into_boxed_slice()))
    }
}
