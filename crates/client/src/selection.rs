//! Immediate viewport selection while the daemon resolves full history bounds.
use crate::{session::ScreenSnapshot, transport::SelectionMode};

type Span = (usize, usize);

#[derive(Default)]
pub struct Preview {
    key: Option<(u64, u16, u16, u64, SelectionMode)>,
    bounds: [Option<Span>; 2],
}

impl Preview {
    pub fn range(
        &mut self,
        screen: &ScreenSnapshot<'_>,
        origin: usize,
        endpoint: usize,
        mode: SelectionMode,
    ) -> Option<Span> {
        if mode == SelectionMode::Cell {
            return Some((origin.min(endpoint), origin.max(endpoint)));
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
        let mut unit = |from: usize, toward: usize, slot: usize| -> Option<Span> {
            // An offscreen anchor is enough to select through the viewport edge.
            if from < base || from > limit {
                return Some((from, from));
            }
            for &(start, end) in self.bounds.iter().flatten() {
                if (start..=end).contains(&from) {
                    return Some((start, end));
                }
            }
            let index = from - base;
            let range = if mode == SelectionMode::Line {
                let mut first = index / cols;
                let mut last = first;
                while first > 0 && wrapped(screen, first - 1) {
                    first -= 1;
                }
                while (last + 1) * cols < screen.cells.len() && wrapped(screen, last) {
                    last += 1;
                }
                Some((
                    first * cols,
                    ((last + 1) * cols - 1).min(screen.cells.len() - 1),
                ))
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
            let range = (base + range.0, base + range.1);
            self.bounds[slot] = Some(range);
            Some(range)
        };
        let anchor = unit(origin, endpoint, 0)?;
        let current = unit(endpoint, origin, 1)?;
        Some((anchor.0.min(current.0), anchor.1.max(current.1)))
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
