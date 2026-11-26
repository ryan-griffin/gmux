use super::Rect;

// Two bits per arm (left, right, up, down): absent, light, heavy, double.
// Dashes, arcs, and diagonals also use the codepoint to select their geometry.
const ARMS: [u8; 128] = [
    0x05, 0x0a, 0x50, 0xa0, 0x05, 0x0a, 0x50, 0xa0, // ─━│┃┄┅┆┇
    0x05, 0x0a, 0x50, 0xa0, 0x44, 0x48, 0x84, 0x88, // ┈┉┊┋┌┍┎┏
    0x41, 0x42, 0x81, 0x82, 0x14, 0x18, 0x24, 0x28, // ┐┑┒┓└┕┖┗
    0x11, 0x12, 0x21, 0x22, 0x54, 0x58, 0x64, 0x94, // ┘┙┚┛├┝┞┟
    0xa4, 0x68, 0x98, 0xa8, 0x51, 0x52, 0x61, 0x91, // ┠┡┢┣┤┥┦┧
    0xa1, 0x62, 0x92, 0xa2, 0x45, 0x46, 0x49, 0x4a, // ┨┩┪┫┬┭┮┯
    0x85, 0x86, 0x89, 0x8a, 0x15, 0x16, 0x19, 0x1a, // ┰┱┲┳┴┵┶┷
    0x25, 0x26, 0x29, 0x2a, 0x55, 0x56, 0x59, 0x5a, // ┸┹┺┻┼┽┾┿
    0x65, 0x95, 0xa5, 0x66, 0x69, 0x96, 0x99, 0x6a, // ╀╁╂╃╄╅╆╇
    0x9a, 0xa6, 0xa9, 0xaa, 0x05, 0x0a, 0x50, 0xa0, // ╈╉╊╋╌╍╎╏
    0x0f, 0xf0, 0x4c, 0xc4, 0xcc, 0x43, 0xc1, 0xc3, // ═║╒╓╔╕╖╗
    0x1c, 0x34, 0x3c, 0x13, 0x31, 0x33, 0x5c, 0xf4, // ╘╙╚╛╜╝╞╟
    0xfc, 0x53, 0xf1, 0xf3, 0x4f, 0xc5, 0xcf, 0x1f, // ╠╡╢╣╤╥╦╧
    0x35, 0x3f, 0x5f, 0xf5, 0xff, 0x44, 0x41, 0x11, // ╨╩╪╫╬╭╮╯
    0x14, 0x00, 0x00, 0x00, 0x01, 0x10, 0x04, 0x40, // ╰╱╲╳╴╵╶╷
    0x02, 0x20, 0x08, 0x80, 0x09, 0x90, 0x06, 0x60, // ╸╹╺╻╼╽╾╿
];

pub struct BoxDrawing(u8);

pub enum BoxPart {
    Rect(Rect),

    Corner {
        center: (f64, f64),
        reach: (f64, f64),
        half_stroke: f64,
        radius: f64,
    },
    Diagonals {
        bounds: Rect,
        stroke: f64,
        rising: bool,
        falling: bool,
    },
}

// Keep all the shared edges in device pixels until emitting the geometry.
#[derive(Clone, Copy)]
struct Band(f64, f64);

impl Band {
    fn width(self) -> f64 {
        self.1 - self.0
    }

    fn center(self) -> f64 {
        (self.0 + self.1) / 2.0
    }

    fn edge(self, positive: bool) -> f64 {
        if positive { self.1 } else { self.0 }
    }

    fn until(self, positive: bool, at: f64) -> Self {
        if positive {
            Self(at, self.1)
        } else {
            Self(self.0, at)
        }
    }

    fn arm(self, positive: bool, center: Self) -> Self {
        self.until(positive, center.edge(!positive))
    }
}

#[derive(Clone, Copy)]
struct Axis {
    cell: Band,
    center: f64,
}

impl Axis {
    fn new(start: f64, length: f64, origin: f64, scale: f64) -> Self {
        Self {
            cell: Band(
                ((start + origin) * scale).round(),
                ((start + length + origin) * scale).round(),
            ),
            center: (start + length / 2.0 + origin) * scale,
        }
    }

    fn band(self, width: f64) -> Band {
        let width = width.min(self.cell.width());
        // Snap one edge and derive the other, preserving the stroke width.
        let start = (self.center - width / 2.0)
            .round()
            .clamp(self.cell.0, self.cell.1 - width);
        Band(start, start + width)
    }

    fn rails(self, width: f64) -> [Band; 2] {
        let Band(a, b) = self.band(width);
        [Band(a - width, a), Band(b, b + width)]
    }
}

struct Grid {
    x: Axis,
    y: Axis,
    origin: (f64, f64),
    scale: f64,
    light: f64,
    heavy: f64,
}

impl Grid {
    fn rect(&self, x: Band, y: Band) -> Rect {
        (
            x.0 / self.scale - self.origin.0,
            y.0 / self.scale - self.origin.1,
            x.width() / self.scale,
            y.width() / self.scale,
        )
    }

    fn stroke(&self, weight: u8) -> f64 {
        if weight == 2 { self.heavy } else { self.light }
    }

    fn solid(
        &self,
        [left, right, up, down]: [u8; 4],
        mixed: bool,
        mut rect: impl FnMut(Band, Band),
    ) {
        let h = left.max(right);
        let v = up.max(down);
        let xb = self.x.band(self.stroke(if v == 0 { h } else { v }));
        let yb = self.y.band(self.stroke(if h == 0 { v } else { h }));
        let x = self.x.cell;
        let y = self.y.cell;
        if !mixed {
            // A horizontal bar and at most two disjoint vertical arms.
            if left != 0 || right != 0 {
                rect(
                    Band(
                        if left != 0 { x.0 } else { xb.0 },
                        if right != 0 { x.1 } else { xb.1 },
                    ),
                    yb,
                );
                if up != 0 {
                    rect(xb, Band(y.0, yb.0));
                }
                if down != 0 {
                    rect(xb, Band(yb.1, y.1));
                }
            } else {
                rect(
                    xb,
                    Band(
                        if up != 0 { y.0 } else { yb.0 },
                        if down != 0 { y.1 } else { yb.1 },
                    ),
                );
            }
        } else {
            // Opposite arms of different weights meet at the snapped center.
            let xm = self.x.center.round();
            let ym = self.y.center.round();
            let both = left != 0 && right != 0;
            for (i, weight) in [left, right].into_iter().enumerate() {
                if weight != 0 {
                    let end = if both { xm } else { xb.edge(i == 0) };
                    rect(x.until(i == 1, end), self.y.band(self.stroke(weight)));
                }
            }
            for (i, weight) in [up, down].into_iter().enumerate() {
                if weight == 0 {
                    continue;
                }
                let positive = i == 1;
                let stem = self.x.band(self.stroke(weight));
                if h == 0 {
                    rect(stem, y.until(positive, ym));
                } else if both {
                    // Trim each half of the stem to the horizontal bar on
                    // that side, without overlapping it at the junction.
                    rect(
                        Band(stem.0, stem.1.min(xm)),
                        y.until(positive, self.y.band(self.stroke(left)).edge(positive)),
                    );
                    rect(
                        Band(stem.0.max(xm), stem.1),
                        y.until(positive, self.y.band(self.stroke(right)).edge(positive)),
                    );
                } else {
                    rect(stem, y.until(positive, yb.edge(positive)));
                }
            }
        }
    }

    fn double(&self, [left, right, up, down]: [u8; 4], mut rect: impl FnMut(Band, Band)) {
        let hs = [left, right];
        let vs = [up, down];
        let xr = self.x.rails(self.light);
        let yr = self.y.rails(self.light);
        if hs.contains(&3) && vs.contains(&3) {
            // Adjacent double arms pair the nearest rails. For a two-arm
            // corner the other two rails bend around the outside together.
            for (h, xrail) in xr.into_iter().enumerate() {
                for (v, yrail) in yr.into_iter().enumerate() {
                    if hs[h] == 0 || vs[v] == 0 {
                        continue;
                    }
                    rect(self.x.cell.arm(h == 1, xrail), yrail);
                    rect(xrail, self.y.cell.until(v == 1, yrail.edge(v == 1)));
                    if hs[1 - h] == 0 && vs[1 - v] == 0 {
                        rect(self.x.cell.arm(h == 1, xr[1 - h]), yr[1 - v]);
                        rect(xr[1 - h], self.y.cell.until(v == 1, yr[1 - v].edge(v == 1)));
                    }
                }
            }
        }
        // An unpaired rail continues straight when both opposite arms exist.
        if left == 3 && right == 3 {
            for (v, rail) in vs.into_iter().zip(yr) {
                if v != 3 {
                    rect(self.x.cell, rail);
                }
            }
        }
        if up == 3 && down == 3 {
            for (h, rail) in hs.into_iter().zip(xr) {
                if h != 3 {
                    rect(rail, self.y.cell);
                }
            }
        }
        if [left, right, up, down].contains(&1) {
            // Handle a mixed junction in horizontal-double coordinates,
            // transposing the vertical-double cases.
            let transpose = !hs.contains(&3);
            let (main, cross, ds, ss, rails) = if transpose {
                (self.y, self.x, vs, hs, xr)
            } else {
                (self.x, self.y, hs, vs, yr)
            };
            let mut rect = |a, b| if transpose { rect(b, a) } else { rect(a, b) };
            let center = main.band(self.light);
            let both_double = ds == [3; 2];
            if !both_double {
                let positive = ds[1] == 3;
                for rail in rails {
                    rect(main.cell.until(positive, center.edge(positive)), rail);
                }
                if ss == [1; 2] {
                    rect(center, cross.cell);
                } else {
                    let positive = ss[1] == 1;
                    rect(
                        center,
                        cross.cell.arm(positive, rails[usize::from(!positive)]),
                    );
                }
            } else {
                // The rails stay continuous through the junction. Fill the
                // gap only when the single stroke crosses from both sides.
                if ss[0] == 1 {
                    rect(center, cross.cell.until(false, rails[0].0));
                }
                if ss[1] == 1 {
                    rect(center, cross.cell.until(true, rails[1].1));
                }
                if ss == [1; 2] {
                    rect(center, Band(rails[0].1, rails[1].0));
                }
            }
        }
    }
}

impl BoxDrawing {
    pub(super) fn from_char(ch: char) -> Option<Self> {
        let code = (ch as u32).wrapping_sub(0x2500);
        (code < 128).then_some(Self(code as u8))
    }

    pub fn paint(
        self,
        (x, y, width, height): Rect,
        origin: (f64, f64),
        scale: f64,
        mut paint: impl FnMut(BoxPart),
    ) {
        let grid = Grid {
            x: Axis::new(x, width, origin.0, scale),
            y: Axis::new(y, height, origin.1, scale),
            origin,
            scale,
            light: (width * scale / 8.0).round().max(1.0),
            heavy: (width * scale / 4.0).round().max(1.0),
        };
        let bits = ARMS[self.0 as usize];
        let mut arms = [bits & 3, (bits >> 2) & 3, (bits >> 4) & 3, bits >> 6];
        let emit = |paint: &mut dyn FnMut(BoxPart), a: Band, b: Band| {
            if a.width() > 0.0 && b.width() > 0.0 {
                paint(BoxPart::Rect(grid.rect(a, b)));
            }
        };
        match self.0 {
            0x00..=0x01 => {
                emit(&mut paint, grid.x.cell, grid.y.band(grid.stroke(arms[0])));
                return;
            }
            0x02..=0x03 => {
                emit(&mut paint, grid.x.band(grid.stroke(arms[2])), grid.y.cell);
                return;
            }
            0x04..=0x0b | 0x4c..=0x4f => {
                let mut count = match self.0 {
                    0x04..=0x07 => 3,
                    0x08..=0x0b => 4,
                    _ => 2,
                };
                let vertical = arms[0] == 0;
                let (length, cross) = if vertical {
                    (grid.y, grid.x)
                } else {
                    (grid.x, grid.y)
                };
                let band = cross.band(grid.stroke(arms[if vertical { 2 } else { 0 }]));
                let span = length.cell;
                // Keep an on- and an off-pixel per dash at the smallest zooms.
                count = count.min((span.width() / 2.0).floor().max(1.0) as usize);
                let period = span.width() / count as f64;
                for i in 0..count {
                    let start = span.0 + (i as f64 * period).round();
                    let end = span.0 + ((i + 1) as f64 * period).round();
                    let gap = ((end - start) / 3.0)
                        .round()
                        .max(1.0)
                        .min((end - start - 1.0).max(0.0));
                    let a = start + (gap / 2.0).floor();
                    let b = end - (gap / 2.0).ceil();
                    if vertical {
                        emit(&mut paint, band, Band(a, b));
                    } else {
                        emit(&mut paint, Band(a, b), band);
                    }
                }
                return;
            }
            0x6d..=0x70 => {
                let cx = grid.x.band(grid.light).center();
                let cy = grid.y.band(grid.light).center();
                let dx = (grid.x.cell.edge(arms[1] != 0) - cx) / scale;
                let dy = (grid.y.cell.edge(arms[3] != 0) - cy) / scale;
                let half_stroke = grid.light / (2.0 * scale);
                let radius = (width * 0.4).min(dx.abs()).min(dy.abs());
                if radius > half_stroke {
                    paint(BoxPart::Corner {
                        center: (cx / scale - origin.0, cy / scale - origin.1),
                        reach: (dx, dy),
                        half_stroke,
                        radius,
                    });
                    return;
                }
            }
            0x71..=0x73 => {
                paint(BoxPart::Diagonals {
                    bounds: grid.rect(grid.x.cell, grid.y.cell),
                    stroke: grid.light / scale,
                    rising: self.0 != 0x72,
                    falling: self.0 != 0x71,
                });
                return;
            }
            _ => {}
        }
        let double = arms.contains(&3);
        let room_for_rails = (width.min(height) * scale).floor() >= 3.0 * grid.light;
        if double && !room_for_rails {
            arms = arms.map(|arm| if arm == 3 { 1 } else { arm });
        }
        if double && room_for_rails {
            grid.double(arms, |a, b| emit(&mut paint, a, b));
        } else {
            grid.solid(arms, arms.contains(&1) && arms.contains(&2), |a, b| {
                emit(&mut paint, a, b)
            });
        }
    }
}
