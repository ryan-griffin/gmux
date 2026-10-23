//! Immediate viewport selection while the daemon resolves full history bounds.
use crate::{session::ScreenSnapshot, transport::SelectionMode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    #[default]
    Linear,
    Rectangle,
}

/// Inclusive cell indices in the terminal's physical rows. Rectangle endpoints
/// are normalized to top-left and bottom-right with the current column count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub start: u64,
    pub end: u64,
    pub shape: Shape,
}

impl Selection {
    pub fn linear(start: u64, end: u64) -> Self {
        Self {
            start: start.min(end),
            end: start.max(end),
            shape: Shape::Linear,
        }
    }

    pub fn rectangle(a: u64, b: u64, cols: u16) -> Self {
        let cols = u64::from(cols.max(1));
        Self {
            start: (a / cols).min(b / cols) * cols + (a % cols).min(b % cols),
            end: (a / cols).max(b / cols) * cols + (a % cols).max(b % cols),
            shape: Shape::Rectangle,
        }
    }

    /// Clip to a viewport and return indices relative to its first cell.
    pub fn viewport(self, offset: u64, cols: u16, len: usize) -> Option<Self> {
        if cols == 0 || len == 0 {
            return None;
        }
        let cols = u64::from(cols);
        let base = offset * cols;
        let limit = base + len as u64 - 1;
        if self.start > limit || self.end < base {
            return None;
        }
        let (start, end) = match self.shape {
            Shape::Linear => (self.start.max(base), self.end.min(limit)),
            Shape::Rectangle => (
                (self.start / cols).max(offset) * cols + self.start % cols,
                (self.end / cols).min(limit / cols) * cols + self.end % cols,
            ),
        };
        Some(Self {
            start: start - base,
            end: end - base,
            ..self
        })
    }

    /// Inclusive columns in a physical row before expanding wide glyph edges.
    pub fn columns(self, base: usize, len: usize, cols: u16) -> Option<(usize, usize)> {
        if len == 0 || cols == 0 {
            return None;
        }
        let base = base as u64;
        let cols = u64::from(cols);
        if self.start > base + len as u64 - 1 || self.end < base {
            return None;
        }
        let (start, end) = match self.shape {
            Shape::Linear => (
                self.start.saturating_sub(base) as usize,
                (self.end - base).min(len as u64 - 1) as usize,
            ),
            Shape::Rectangle => ((self.start % cols) as usize, (self.end % cols) as usize),
        };
        (start <= end && end < len).then_some((start, end))
    }

    /// Include the complete wide glyph at either edge, as Ghostty's formatter
    /// does. Geometry uses constant storage, independent of selection area.
    pub fn row(self, base: usize, cells: &[crate::Cell], cols: u16) -> Option<(usize, usize)> {
        let (mut start, mut end) = self.columns(base, cells.len(), cols)?;
        if self.shape == Shape::Rectangle {
            if start > 0 && cells[start].flags & 64 != 0 {
                start -= 1;
            }
            if end + 1 < cells.len() && cells[end + 1].flags & 64 != 0 {
                end += 1;
            }
        }
        (start <= end && end < cells.len()).then_some((start, end))
    }
}

type Span = (usize, usize);

#[derive(Clone, Copy)]
struct Unit {
    range: Span,
    empty: bool,
}

#[derive(Default)]
pub struct Preview {
    key: Option<(u64, u16, u16, u64, SelectionMode)>,
    bounds: [Option<Unit>; 2],
}

impl Preview {
    pub fn range(
        &mut self,
        screen: &ScreenSnapshot<'_>,
        origin: usize,
        endpoint: usize,
        mode: SelectionMode,
    ) -> Option<Selection> {
        if screen.frame.cols == 0 {
            return None;
        }
        if mode == SelectionMode::Cell || mode == SelectionMode::Rectangle {
            return Some(if mode == SelectionMode::Rectangle {
                Selection::rectangle(origin as u64, endpoint as u64, screen.frame.cols)
            } else {
                Selection::linear(origin as u64, endpoint as u64)
            });
        }
        let key = (
            screen.text_revision,
            screen.frame.cols,
            screen.frame.rows,
            screen.frame.scrollback.offset,
            mode,
        );
        if self.key != Some(key) {
            self.key = Some(key);
            self.bounds = [None; 2];
        }
        let cols = screen.frame.cols as usize;
        if cols == 0 || screen.cells.is_empty() {
            return None;
        }
        let base = screen.frame.scrollback.offset as usize * cols;
        let limit = base + screen.cells.len() - 1;
        let mut unit = |from: usize, toward: usize, slot: usize| -> Option<Unit> {
            // An offscreen anchor is enough to select through the viewport edge.
            if from < base || from > limit {
                return Some(Unit {
                    range: (from, from),
                    empty: false,
                });
            }
            for &unit in self.bounds.iter().flatten() {
                let (start, end) = unit.range;
                if (start..=end).contains(&from) {
                    return Some(unit);
                }
            }
            let index = from - base;
            let mut empty = false;
            let range = if mode == SelectionMode::Line {
                let mut first = index / cols;
                let mut last = first;
                while first > 0 && wrapped(screen, first - 1) {
                    first -= 1;
                }
                while (last + 1) * cols < screen.cells.len() && wrapped(screen, last) {
                    last += 1;
                }
                let start = first * cols;
                let end = ((last + 1) * cols - 1).min(screen.cells.len() - 1);
                empty = !screen.cells[start..=end]
                    .iter()
                    .any(|cell| cell.text[0] != 0);
                Some(if empty {
                    (index / cols * cols, (index / cols + 1) * cols - 1)
                } else {
                    (start, end)
                })
            } else {
                let target = toward.clamp(base, limit) - base;
                let mut point = index;
                loop {
                    if let Some(class) = word_class(screen, point) {
                        let mut start = point;
                        let mut end = point;
                        while start > 0
                            && (!start.is_multiple_of(cols) || wrapped(screen, start / cols - 1))
                            && word_class(screen, start - 1) == Some(class)
                        {
                            start -= 1;
                        }
                        while end + 1 < screen.cells.len()
                            && (!(end + 1).is_multiple_of(cols) || wrapped(screen, end / cols))
                            && word_class(screen, end + 1) == Some(class)
                        {
                            end += 1;
                        }
                        break Some((start, end));
                    }
                    if point == target {
                        break None;
                    }
                    if point < target {
                        point += 1;
                    } else {
                        point -= 1;
                    }
                }
            }?;
            let unit = Unit {
                range: (base + range.0, base + range.1),
                empty,
            };
            self.bounds[slot] = Some(unit);
            Some(unit)
        };
        let anchor = unit(origin, endpoint, 0)?;
        let current = unit(endpoint, origin, 1)?;
        // Empty rows can extend a line drag, but cannot start a selection alone.
        if anchor.empty && current.empty && anchor.range == current.range {
            return None;
        }
        let anchor = anchor.range;
        let current = current.range;
        Some(Selection::linear(
            anchor.0.min(current.0) as u64,
            anchor.1.max(current.1) as u64,
        ))
    }
}

fn wrapped(screen: &ScreenSnapshot<'_>, row: usize) -> bool {
    screen
        .row_flags
        .get(row)
        .is_some_and(|flags| flags & 1 != 0)
}

fn word_class(screen: &ScreenSnapshot<'_>, mut index: usize) -> Option<bool> {
    let cell = screen.cells.get(index)?;
    if cell.flags & 64 != 0 {
        index = index.checked_sub(1)?;
    }
    if cell.flags & 128 != 0 {
        index += 1;
    }
    let cp = screen.cells.get(index)?.text().chars().next()?;
    // Ghostty's default word boundaries. Grapheme continuations use their owner.
    Some(matches!(
        cp,
        '\0' | ' '
            | '\t'
            | '\''
            | '"'
            | '│'
            | '`'
            | '|'
            | ':'
            | ';'
            | ','
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '<'
            | '>'
            | '$'
    ))
}
