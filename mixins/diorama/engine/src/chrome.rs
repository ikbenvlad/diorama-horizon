//! The minimap's frame and compass letters, as shapes the glue fills.
//!
//! Upstream draws the frame from a picture (`render/MinimapFrames.java`), 128 GUI pixels across
//! for a map 120 across, of which only the outer 16-pixel band is used. The classic picture
//! (`textures/gui/minimap_frame/classic_{square,round}.png`) is two one-pixel lines hugging the
//! map: grey right against its edge, dark ink just outside that. That is drawn here as rectangles
//! for a square map and as rings of quads for a round one. The compass follows
//! `MinimapHud.compass` (MinimapHud.java:236-251).

use std::f32::consts::TAU;

/// The outer line of the classic frame, 0xAARRGGBB (classic_square.png, alpha 192).
pub const INK: u32 = 0xC010_1418;
/// The inner line of the classic frame, against the map (classic_square.png, alpha 216).
pub const GREY: u32 = 0xD87A_7D7F;
/// How far the frame's picture reaches past the map, room the glue keeps clear (MinimapFrames.java:33).
pub const OUTSIDE: i32 = 4;
/// Sides of the polygon a round frame is drawn as (Circle.java:16).
pub const SEGMENTS: usize = 96;
/// The box behind a compass letter.
pub const LETTER_BOX: u32 = 0xA010_1418;
/// North's letter.
pub const NORTH: u32 = 0xFFFF_6B6B;
/// The other letters.
pub const LETTER: u32 = 0xFFFF_FFFF;

/// A quad's corners, in the order upstream's quads take them: around counter-clockwise on screen.
pub type Quad = [[f32; 2]; 4];

/// The classic frame round a square map `size` wide at (x, y): rectangles x0, y0, x1, y1 (ends
/// exclusive, GUI pixels) and their colours, none overlapping, drawn after the map.
pub fn square(x: i32, y: i32, size: i32) -> Vec<([i32; 4], u32)> {
    let mut out = Vec::with_capacity(8);
    outline(&mut out, x - 2, y - 2, size + 4, INK);
    outline(&mut out, x - 1, y - 1, size + 2, GREY);
    out
}

/// The classic frame round a round map of radius `r` centred on (cx, cy): grey from r to r + 1,
/// ink from r + 1 to r + 2, as quads with their colours.
pub fn round(cx: f32, cy: f32, r: f32) -> Vec<(Quad, u32)> {
    let mut out = Vec::with_capacity(SEGMENTS * 2);
    out.extend(
        ring(cx, cy, r + 2.0, 1.0, SEGMENTS)
            .into_iter()
            .map(|q| (q, INK)),
    );
    out.extend(
        ring(cx, cy, r + 1.0, 1.0, SEGMENTS)
            .into_iter()
            .map(|q| (q, GREY)),
    );
    out
}

/// The band between a circle of radius `r` round (cx, cy) and one `width` smaller inside it, as
/// `segments` quads: each along the outside, then back along the inside (Circle.java:115-131).
pub fn ring(cx: f32, cy: f32, r: f32, width: f32, segments: usize) -> Vec<Quad> {
    let inner = (r - width).max(0.0);
    let unit = |i: usize| {
        let a = TAU * (i % segments) as f32 / segments as f32;
        [a.cos(), -a.sin()]
    };
    (0..segments)
        .map(|i| {
            let (a, b) = (unit(i), unit(i + 1));
            [
                [cx + r * a[0], cy + r * a[1]],
                [cx + r * b[0], cy + r * b[1]],
                [cx + inner * b[0], cy + inner * b[1]],
                [cx + inner * a[0], cy + inner * a[1]],
            ]
        })
        .collect()
}

/// A one-pixel square outline `width` across at (x, y): full rows top and bottom, the sides between.
fn outline(out: &mut Vec<([i32; 4], u32)>, x: i32, y: i32, width: i32, argb: u32) {
    let (x1, y1) = (x + width, y + width);
    out.push(([x, y, x1, y + 1], argb));
    out.push(([x, y1 - 1, x1, y1], argb));
    out.push(([x, y + 1, x + 1, y1 - 1], argb));
    out.push(([x1 - 1, y + 1, x1, y1 - 1], argb));
}

/// One compass letter: where its text goes (no shadow), its colour, and the box filled behind it
/// first (x0, y0, x1, y1, ends exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Letter {
    pub letter: char,
    pub x: i32,
    pub y: i32,
    pub argb: u32,
    pub back: [i32; 4],
}

/// The compass letters N, E, S, W on a map `size` wide at (x, y) facing `turns` quarter turns
/// from north, given the font's advance for a letter (vanilla's, the one-pixel gap included).
/// Upstream places them the same on a round map as on a square one (MinimapHud.java:236-251).
pub fn compass(
    x: i32,
    y: i32,
    size: i32,
    turns: usize,
    advance: impl Fn(char) -> i32,
) -> [Letter; 4] {
    // Top, right, bottom, left: the middle of each letter, and its text's top.
    let at = [
        (x + size / 2, y + 1),
        (x + size - 5, y + size / 2 - 4),
        (x + size / 2, y + size - 9),
        (x + 5, y + size / 2 - 4),
    ];
    let letters = ['N', 'E', 'S', 'W'];
    std::array::from_fn(|i| {
        let (cx, ty) = at[(i + 4 - turns % 4) % 4];
        let letter = letters[i];
        let w = advance(letter);
        let left = cx - w / 2;
        Letter {
            letter,
            x: left,
            y: ty,
            argb: if i == 0 { NORTH } else { LETTER },
            back: [left - 2, ty - 1, left + w + 1, ty + 8],
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(q: &Quad) -> f32 {
        (0..4)
            .map(|k| {
                let (a, b) = (q[k], q[(k + 1) % 4]);
                a[0] * b[1] - b[0] * a[1]
            })
            .sum::<f32>()
            / 2.0
    }

    #[test]
    fn square_frame_hugs_the_map() {
        let (x, y, size) = (10, 20, 64);
        let rects = square(x, y, size);
        let mut grid = vec![0u32; 100 * 100];
        for ([x0, y0, x1, y1], argb) in &rects {
            assert!(x0 < x1 && y0 < y1);
            for yy in *y0..*y1 {
                for xx in *x0..*x1 {
                    let i = (yy * 100 + xx) as usize;
                    assert_eq!(grid[i], 0, "overlap at {xx}, {yy}");
                    grid[i] = *argb;
                    // Never over the map itself.
                    assert!(!(x..x + size).contains(&xx) || !(y..y + size).contains(&yy));
                }
            }
        }
        let count = |c: u32| grid.iter().filter(|&&g| g == c).count();
        assert_eq!(count(GREY), ((size + 2) * 4 - 4) as usize);
        assert_eq!(count(INK), ((size + 4) * 4 - 4) as usize);
        // Grey right against the map, ink one further out, corners included.
        assert_eq!(grid[((y - 1) * 100 + x - 1) as usize], GREY);
        assert_eq!(grid[((y - 2) * 100 + x - 2) as usize], INK);
        assert_eq!(grid[((y + size) * 100 + x + size) as usize], GREY);
        assert_eq!(grid[((y + size + 1) * 100 + x + size + 1) as usize], INK);
    }

    #[test]
    fn ring_covers_the_band() {
        let (cx, cy, r, w) = (50.0, 40.0, 30.0, 2.0);
        let quads = ring(cx, cy, r, w, SEGMENTS);
        assert_eq!(quads.len(), SEGMENTS);
        let mut total = 0.0;
        for q in &quads {
            for p in q {
                let d = ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt();
                assert!(d > r - w - 1e-3 && d < r + 1e-3);
            }
            // Every quad turns the same way.
            assert!(area(q) < 0.0);
            total += -area(q);
        }
        let band = std::f32::consts::PI * (r * r - (r - w) * (r - w));
        assert!((total - band).abs() / band < 0.01, "{total} vs {band}");
        // Closed: the last quad ends where the first starts.
        assert!((quads[SEGMENTS - 1][1][0] - quads[0][0][0]).abs() < 1e-3);
        let frame = round(cx, cy, r);
        assert_eq!(frame.len(), SEGMENTS * 2);
        assert_eq!(frame[0].1, INK);
        assert_eq!(frame[SEGMENTS].1, GREY);
    }

    #[test]
    fn compass_letters() {
        let (x, y, size) = (100, 4, 64);
        let letters = compass(x, y, size, 0, |_| 6);
        assert_eq!(letters[0].letter, 'N');
        assert_eq!(letters[0].argb, NORTH);
        assert_eq!((letters[0].x, letters[0].y), (129, 5));
        assert_eq!(letters[0].back, [127, 4, 136, 13]);
        assert_eq!(letters[1].argb, LETTER);
        for l in &letters {
            // The box holds the glyph, 7 tall, and stays on the map.
            assert!(
                l.back[0] < l.x && l.back[2] >= l.x + 5 && l.back[1] < l.y && l.back[3] >= l.y + 8
            );
            assert!(
                l.back[0] >= x && l.back[2] <= x + size && l.back[1] >= y && l.back[3] <= y + size
            );
        }
        // Facing east, north is on the left.
        let east = compass(x, y, size, 1, |_| 6);
        assert_eq!((east[0].x, east[0].y), (x + 5 - 3, y + size / 2 - 4));
        assert_eq!(compass(x, y, size, 4, |_| 6), letters);
    }
}
