//! Waypoint pins as pixel art, one texel per GUI pixel: a round badge with a dark rim and a bevel
//! lit from the top left, either standing on a point that touches the spot or, at the edge of a
//! view, with a caret pointing the way. Pure pixels, so the glue can draw them as runs of filled
//! rectangles.
//!
//! Ported from Diorama's `render/PinArt.java`; the sizes and caret steps come from
//! `render/Pins.java`.

use std::f64::consts::TAU;

/// The rim and the initials on light faces.
pub const INK: u32 = 0x101418;
/// Deaths are near-black so the headstone stands out.
pub const DEATH_FACE: u32 = 0x2B2D31;
/// Head sizes: the map and the world use the large pin, the minimap the small one (Pins.java:22).
pub const LARGE: i32 = 15;
pub const SMALL: i32 = 13;
/// How far a standing pin's shadow reaches below the spot it marks (Pins.java:27).
pub const SHADOW: i32 = 3;
/// Directions a caret at the edge of a view can point, around the full turn (Pins.java:25).
pub const CARET_STEPS: i32 = 32;
const RIM_ALPHA: f32 = 0.94;
/// The white round an icon, a touch off so it doesn't glare on snow.
const STICKER_WHITE: u32 = 0xF6F6F6;
/// The tip under an icon pin's head, as long as the large pin's.
const ICON_TIP: i32 = 6;

/// Kenney's 8x8 headstone (Micro Roguelike, CC0) cut to 7x7 and made mirror-symmetric. Pin heads
/// are an odd number of pixels across both ways, so an odd-sized icon centres exactly.
const HEADSTONE: [&str; 7] = [
    "..gWg..", ".gWWWg.", ".WgdgW.", ".WWWWW.", ".WgdgW.", "ggggggg", "gWWWWWg",
];

/// The two standing pins' outlines, pixel for pixel as they were designed: a disc of 15 (or 13)
/// raised on a point 6 (or 5) long, mirrored so both sides match (PinArt.java:35-77).
const LARGE_PIN: [&str; 21] = [
    "........###........",
    ".....#########.....",
    "....###########....",
    "...#############...",
    "...#############...",
    "...#############...",
    "..###############..",
    "..###############..",
    "..###############..",
    "...#############...",
    "...#############...",
    "...#############...",
    "....###########....",
    ".....#########.....",
    "......#######......",
    ".......#####.......",
    ".......#####.......",
    "........###........",
    "........###........",
    ".........#.........",
    ".........#.........",
];
const SMALL_PIN: [&str; 18] = [
    ".......###.......",
    ".....#######.....",
    "....#########....",
    "...###########...",
    "...###########...",
    "..#############..",
    "..#############..",
    "..#############..",
    "...###########...",
    "...###########...",
    "....#########....",
    ".....#######.....",
    "......#####......",
    "......#####......",
    ".......###.......",
    ".......###.......",
    ".......###.......",
    "........#........",
];

/// A drawn pin: its pixels as 0xAARRGGBB row by row (fully clear pixels are 0), the anchor (the
/// spot it marks, in texels from the top left, on a half texel when the head is an odd width), and
/// the box x0, y0, x1, y1 its initials are centred in (all zero when it shows none).
#[derive(Clone, Debug, PartialEq)]
pub struct PinImage {
    pub w: usize,
    pub h: usize,
    pub argb: Vec<u32>,
    pub anchor: [f32; 2],
    pub label: [i32; 4],
}

impl PinImage {
    /// Pixel (x, y) as 0xAARRGGBB.
    pub fn at(&self, x: usize, y: usize) -> u32 {
        self.argb[y * self.w + x]
    }
}

/// Horizontal runs of one colour, skipping clear pixels: (x, y, width, argb). Drawn as filled
/// one-pixel-tall rectangles they reproduce the image.
pub fn runs(img: &PinImage) -> Vec<(i32, i32, i32, u32)> {
    let mut out = Vec::new();
    for y in 0..img.h {
        let row = &img.argb[y * img.w..(y + 1) * img.w];
        let mut x = 0;
        while x < img.w {
            let c = row[x];
            let start = x;
            while x < img.w && row[x] == c {
                x += 1;
            }
            if c >> 24 != 0 {
                out.push((start as i32, y as i32, (x - start) as i32, c));
            }
        }
    }
    out
}

/// A badge raised on a point, with a small shadow where it touches. The pin comes in the two
/// designed sizes, a head of 15 or 13 pixels; other sizes get the nearer one. `stretch` widens the
/// head into a pill by that many pixels (see [`stretch_for`]), for initials too wide for the round
/// one (PinArt.java:158-175).
pub fn standing(colour: u32, death: bool, head: i32, stretch: i32) -> PinImage {
    let colour = colour & 0xFF_FFFF;
    let stretch = stretch.max(0);
    let d = standing_head(head);
    let tip = if d == SMALL { 5 } else { 6 };
    let face = if death { DEATH_FACE } else { colour };
    let mut img = Canvas::new((d + 4 + stretch) as usize, (d + tip + 3) as usize);
    let ox = 2;
    let ax = ox as f32 + (d + stretch) as f32 / 2.0;
    let ay = (d + tip) as f32;
    let shadow = img.ellipse(ax, ay - 0.5, d as f32 * 0.26 + 0.6, 1.3);
    img.over(&shadow, 0x000000, 0.28);
    let rows = stretched(if d == SMALL { &SMALL_PIN } else { &LARGE_PIN }, d, stretch);
    let mask = img.outline(&rows);
    img.face(&mask, face);
    let label = [ox + 1, 1, ox + d - 1 + stretch, d - 1];
    if death {
        img.headstone(label);
    }
    img.finish([ax, ay], label)
}

/// A round badge of diameter `d`; with `caret` (an angle in radians, screen space, 0 is right, and
/// a size) a caret beside it points that way. With an `icon`, the icon sits on it as on an icon
/// pin, and `d` should be its [`icon_head`]. `stretch` widens it into a pill, as for [`standing`]
/// (PinArt.java:216-259).
pub fn badge(
    colour: u32,
    death: bool,
    d: i32,
    stretch: i32,
    caret: Option<(f64, i32)>,
    icon: Option<&PinImage>,
) -> PinImage {
    let colour = colour & 0xFF_FFFF;
    let stretch = stretch.max(0);
    let face = if death { DEATH_FACE } else { colour };
    let pad = if caret.is_some() { 6 } else { 0 };
    let mut img = Canvas::new((d + stretch + pad * 2) as usize, (d + pad * 2) as usize);
    let cx = pad as f32 + (d + stretch) as f32 / 2.0;
    let cy = pad as f32 + d as f32 / 2.0;
    let r = d as f32 / 2.0;
    let half = stretch as f32 / 2.0;
    let mask = img.pill(cx, cy, r, half);
    img.face(&mask, face);
    let label = [pad + 1, pad + 1, pad + d + stretch - 1, pad + d - 1];
    if death {
        img.headstone(label);
    } else if let Some(icon) = icon {
        img.sticker(icon, cx, cy);
    }
    if let Some((angle, size)) = caret {
        let fx = angle.cos() as f32;
        let fy = angle.sin() as f32;
        let (px, py) = (-fy, fx);
        let reach = pill_reach(r, half, fx, fy);
        let x = cx + fx * (reach + 1.5);
        let y = cy + fy * (reach + 1.5);
        let s = size as f32;
        let m = img.polygon(&[
            x + fx * s * 0.55,
            y + fy * s * 0.55,
            x - fx * s * 0.4 + px * s * 0.5,
            y - fy * s * 0.4 + py * s * 0.5,
            x - fx * s * 0.4 - px * s * 0.5,
            y - fy * s * 0.4 - py * s * 0.5,
        ]);
        let mut rim = img.dilate(&m);
        for (i, v) in rim.iter_mut().enumerate() {
            *v &= !m[i] && img.px[i][3] < 1.0;
        }
        img.put(&rim, INK, 0.9);
        // Upstream colours the caret with the waypoint's colour even on a death badge.
        img.put(&m, colour, 1.0);
    }
    if icon.is_some() && !death {
        // No initials: an empty label box.
        return img.finish([cx, cy], [0; 4]);
    }
    img.finish([cx, cy], label)
}

/// The standing pin for a waypoint with an icon: the round pin in the waypoint's colour, its head
/// grown to hold the icon, the icon on it like a sticker with a white edge round its shape
/// (PinArt.java:304-320).
pub fn icon_pin(icon: &PinImage, colour: u32) -> PinImage {
    let colour = colour & 0xFF_FFFF;
    let d = icon_head(icon);
    let mut img = Canvas::new((d + 4) as usize, (d + ICON_TIP + 3) as usize);
    let ax = 2.0 + d as f32 / 2.0;
    let ay = (d + ICON_TIP) as f32;
    let shadow = img.ellipse(ax, ay - 0.5, d as f32 * 0.26 + 0.6, 1.3);
    img.over(&shadow, 0x000000, 0.28);
    let df = d as f32;
    let mut mask = img.pill(ax, df / 2.0, df / 2.0, 0.0);
    let point = img.polygon(&[ax - df * 0.32, df * 0.75, ax + df * 0.32, df * 0.75, ax, ay]);
    for (m, p) in mask.iter_mut().zip(&point) {
        *m |= *p;
    }
    img.face(&mask, colour);
    img.sticker(icon, ax, df / 2.0);
    img.finish([ax, ay], [0; 4])
}

/// The head of a pin or badge showing an icon: the icon's tile, its white edge, then room for the
/// tile's corners to clear the bevel and rim. Odd, so the tile centres (PinArt.java:289).
pub fn icon_head(icon: &PinImage) -> i32 {
    let d = icon.w.max(icon.h) as i32 + 10;
    if d % 2 == 0 { d + 1 } else { d }
}

/// How far above the marked spot an icon pin's head is centred.
pub fn icon_head_above(icon: &PinImage) -> f32 {
    icon_head(icon) as f32 / 2.0 + ICON_TIP as f32
}

/// How many pixels wider than round a head of `head` has to be to hold initials whose ink is `ink`
/// wide. Two letters fit the round head; wider ones get a pill whose round ends stay clear of the
/// letters. Even, so the point stays under the middle (PinArt.java:182).
pub fn stretch_for(head: i32, ink: i32) -> i32 {
    let d = standing_head(head);
    if ink <= d - 4 {
        return 0;
    }
    let extra = ink - (d - 8);
    extra + (extra & 1)
}

/// The designed standing pin nearest a head size: 15 or 13.
pub fn standing_head(head: i32) -> i32 {
    if head >= 14 { LARGE } else { SMALL }
}

/// How far above the marked spot a standing pin's head is centred, in GUI pixels (Pins.java:129).
pub fn head_above(size: i32) -> f32 {
    let head = standing_head(size);
    (if head == SMALL { 5.0 } else { 6.0 }) + head as f32 / 2.0
}

/// Where something `size` wide starts so it is centred in x0..x1; a half pixel rounds toward the
/// bottom right, where the bevel is darker.
pub fn centred(x0: i32, x1: i32, size: i32) -> i32 {
    x0 + (x1 - x0 - size + 1).div_euclid(2)
}

/// Dark letters on light colours, white on dark ones (the mod's rule).
pub fn light(rgb: u32) -> bool {
    let r = ((rgb >> 16) & 0xFF) as f64;
    let g = ((rgb >> 8) & 0xFF) as f64;
    let b = (rgb & 0xFF) as f64;
    0.299 * r + 0.587 * g + 0.114 * b > 160.0
}

/// Which of the [`CARET_STEPS`] directions an angle (radians, screen space) snaps to
/// (Pins.java:96).
pub fn caret_step(angle: f64) -> i32 {
    java_round((angle / TAU * CARET_STEPS as f64) as f32).rem_euclid(CARET_STEPS)
}

/// The angle a caret step points along, in radians.
pub fn caret_angle(step: i32) -> f64 {
    step as f64 * TAU / CARET_STEPS as f64
}

/// The caret's size for a badge head (Pins.java:98).
pub fn caret_size(head: i32) -> i32 {
    if head >= LARGE { 6 } else { 5 }
}

/// Mixes two 0xRRGGBB colours, `t` of the way to `other`, rounding as Java's `Math.round`.
pub(crate) fn mix(rgb: u32, other: u32, t: f32) -> u32 {
    let ch = |shift: u32| {
        let a = ((rgb >> shift) & 0xFF) as f32;
        let b = ((other >> shift) & 0xFF) as f32;
        java_round(a * (1.0 - t) + b * t).clamp(0, 255) as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Whether (x, y) is inside the polygon given as x, y pairs (even-odd rule).
pub(crate) fn inside(pts: &[f32], x: f32, y: f32) -> bool {
    let n = pts.len() / 2;
    let mut is_in = false;
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (xi, yi) = (pts[i * 2], pts[i * 2 + 1]);
        let (xj, yj) = (pts[j * 2], pts[j * 2 + 1]);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            is_in = !is_in;
        }
        j = i;
    }
    is_in
}

/// Java's `Math.round(float)`: halves round up.
fn java_round(x: f32) -> i32 {
    (x + 0.5).floor() as i32
}

/// A designed pin outline with its head pulled wider: the middle column of the head repeated
/// `stretch` times, the point left as it is under the middle (PinArt.java:195).
fn stretched(rows: &[&str], head: i32, stretch: i32) -> Vec<String> {
    let s = stretch as usize;
    rows.iter()
        .enumerate()
        .map(|(y, row)| {
            if s == 0 {
                return row.to_string();
            }
            let middle = row.len() / 2;
            if (y as i32) < head {
                let mid = &row[middle..middle + 1];
                format!("{}{}{}", &row[..middle], mid.repeat(s), &row[middle..])
            } else {
                let side = ".".repeat(s / 2);
                format!("{side}{row}{side}")
            }
        })
        .collect()
}

/// How far from a pill's centre its edge is along the direction (fx, fy), a unit vector.
fn pill_reach(r: f32, half: f32, fx: f32, fy: f32) -> f32 {
    let (mut lo, mut hi) = (0.0f32, r + half);
    for _ in 0..20 {
        let t = (lo + hi) / 2.0;
        let dx = ((fx * t).abs() - half).max(0.0);
        let dy = fy * t;
        if dx * dx + dy * dy < r * r {
            lo = t;
        } else {
            hi = t;
        }
    }
    lo
}

/// Straight-alpha RGBA, 0..255 per channel, as PinArt.Image.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<[f32; 4]>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Canvas {
        Canvas {
            w,
            h,
            px: vec![[0.0; 4]; w * h],
        }
    }

    fn put(&mut self, mask: &[bool], rgb: u32, alpha: f32) {
        let c = channels(rgb);
        for (p, _) in self.px.iter_mut().zip(mask).filter(|(_, m)| **m) {
            *p = [c[0], c[1], c[2], alpha * 255.0];
        }
    }

    /// Blends a colour over what is there, for the ground shadow under the pin.
    fn over(&mut self, mask: &[bool], rgb: u32, alpha: f32) {
        let c = channels(rgb);
        for (p, _) in self.px.iter_mut().zip(mask).filter(|(_, m)| **m) {
            let dst = p[3] / 255.0;
            let out = alpha + dst * (1.0 - alpha);
            for k in 0..3 {
                p[k] = (c[k] * alpha + p[k] * dst * (1.0 - alpha)) / out;
            }
            p[3] = out * 255.0;
        }
    }

    fn set(&mut self, x: i32, y: i32, rgb: [f32; 3]) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            self.px[y as usize * self.w + x as usize] = [rgb[0], rgb[1], rgb[2], 255.0];
        }
    }

    fn at(&self, m: &[bool], x: i32, y: i32) -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < self.w
            && (y as usize) < self.h
            && m[y as usize * self.w + x as usize]
    }

    fn mask(&self, f: impl Fn(f32, f32) -> bool) -> Vec<bool> {
        let mut m = vec![false; self.w * self.h];
        for y in 0..self.h {
            for x in 0..self.w {
                m[y * self.w + x] = f(x as f32 + 0.5, y as f32 + 0.5);
            }
        }
        m
    }

    /// A disc of radius r drawn out sideways by `half` each way from (cx, cy): pixels whose centre
    /// is inside, a quarter pixel in from the edge (PinArt.java:444-469).
    fn pill(&self, cx: f32, cy: f32, r: f32, half: f32) -> Vec<bool> {
        let rr = (r - 0.25) * (r - 0.25);
        self.mask(|x, y| {
            let dx = ((x - cx).abs() - half).max(0.0);
            let dy = y - cy;
            dx * dx + dy * dy <= rr
        })
    }

    fn ellipse(&self, cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<bool> {
        self.mask(|x, y| {
            let dx = (x - cx) / rx;
            let dy = (y - cy) / ry;
            dx * dx + dy * dy <= 1.0
        })
    }

    /// Pixels covered at least half by the polygon (x, y pairs), measured on a 4x4 grid inside each.
    fn polygon(&self, pts: &[f32]) -> Vec<bool> {
        let mut m = vec![false; self.w * self.h];
        for y in 0..self.h {
            for x in 0..self.w {
                let mut hits = 0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let px = x as f32 + (sx as f32 + 0.5) / 4.0;
                        let py = y as f32 + (sy as f32 + 0.5) / 4.0;
                        hits += inside(pts, px, py) as i32;
                    }
                }
                m[y * self.w + x] = hits >= 8;
            }
        }
        m
    }

    fn outline(&self, rows: &[String]) -> Vec<bool> {
        let mut m = vec![false; self.w * self.h];
        for (y, row) in rows.iter().enumerate().take(self.h) {
            for (x, c) in row.bytes().enumerate().take(self.w) {
                m[y * self.w + x] = c == b'#';
            }
        }
        m
    }

    /// The outermost pixels of a shape: those with a 4-neighbour outside it, or on the canvas edge.
    fn ring(&self, m: &[bool]) -> Vec<bool> {
        let mut out = vec![false; m.len()];
        for y in 0..self.h as i32 {
            for x in 0..self.w as i32 {
                let i = y as usize * self.w + x as usize;
                if m[i] {
                    out[i] = !self.at(m, x - 1, y)
                        || !self.at(m, x + 1, y)
                        || !self.at(m, x, y - 1)
                        || !self.at(m, x, y + 1);
                }
            }
        }
        out
    }

    /// The shape grown by a pixel up, down, left and right, so its corners come out cut.
    fn grow4(&self, m: &[bool]) -> Vec<bool> {
        let mut out = vec![false; m.len()];
        for y in 0..self.h as i32 {
            for x in 0..self.w as i32 {
                out[y as usize * self.w + x as usize] = self.at(m, x, y)
                    || self.at(m, x - 1, y)
                    || self.at(m, x + 1, y)
                    || self.at(m, x, y - 1)
                    || self.at(m, x, y + 1);
            }
        }
        out
    }

    fn dilate(&self, m: &[bool]) -> Vec<bool> {
        let mut out = vec![false; m.len()];
        for y in 0..self.h as i32 {
            for x in 0..self.w as i32 {
                out[y as usize * self.w + x as usize] =
                    (-1..=1).any(|dy| (-1..=1).any(|dx| self.at(m, x + dx, y + dy)));
            }
        }
        out
    }

    /// A dark rim, the colour inside, a one-pixel bevel lit from the top left (PinArt.java:383).
    fn face(&mut self, mask: &[bool], colour: u32) {
        let ring = self.ring(mask);
        let inner: Vec<bool> = mask.iter().zip(&ring).map(|(m, r)| *m && !*r).collect();
        self.put(&ring, INK, RIM_ALPHA);
        self.put(&inner, colour, 1.0);
        let mut top = vec![false; mask.len()];
        let mut bottom = vec![false; mask.len()];
        for y in 0..self.h as i32 {
            for x in 0..self.w as i32 {
                let i = y as usize * self.w + x as usize;
                if !inner[i] {
                    continue;
                }
                let above_or_left = self.at(&ring, x, y - 1) || self.at(&ring, x - 1, y);
                let below_or_right = self.at(&ring, x, y + 1) || self.at(&ring, x + 1, y);
                top[i] = above_or_left && !below_or_right;
                bottom[i] = below_or_right && !above_or_left;
            }
        }
        self.put(&top, mix(colour, 0xFFFFFF, 0.35), 1.0);
        self.put(&bottom, mix(colour, 0x000000, 0.25), 1.0);
    }

    /// The headstone centred in the label box x0, y0, x1, y1.
    fn headstone(&mut self, b: [i32; 4]) {
        let size = HEADSTONE.len() as i32;
        let left = centred(b[0], b[2], size);
        let top = centred(b[1], b[3], size);
        for (y, row) in HEADSTONE.iter().enumerate() {
            for (x, c) in row.bytes().enumerate() {
                let rgb = match c {
                    b'W' => 0xC2C2D1,
                    b'g' => 0x7E7E8F,
                    b'd' => 0x606070,
                    _ => continue,
                };
                self.set(left + x as i32, top + y as i32, channels(rgb));
            }
        }
    }

    /// Puts an icon centred on (cx, cy), with a white edge round its shape (PinArt.java:323).
    fn sticker(&mut self, icon: &PinImage, cx: f32, cy: f32) {
        let ix = java_round(cx - icon.w as f32 / 2.0);
        let iy = java_round(cy - icon.h as f32 / 2.0);
        let mut shape = vec![false; self.w * self.h];
        for y in 0..icon.h {
            for x in 0..icon.w {
                let (tx, ty) = (ix + x as i32, iy + y as i32);
                if icon.at(x, y) >> 24 >= 128
                    && tx >= 0
                    && ty >= 0
                    && (tx as usize) < self.w
                    && (ty as usize) < self.h
                {
                    shape[ty as usize * self.w + tx as usize] = true;
                }
            }
        }
        let mut edge = self.grow4(&shape);
        for (e, s) in edge.iter_mut().zip(&shape) {
            *e &= !*s;
        }
        self.put(&edge, STICKER_WHITE, 1.0);
        for y in 0..icon.h {
            for x in 0..icon.w {
                let c = icon.at(x, y);
                if c >> 24 >= 128 {
                    self.set(ix + x as i32, iy + y as i32, channels(c));
                }
            }
        }
    }

    fn finish(self, anchor: [f32; 2], label: [i32; 4]) -> PinImage {
        let argb = self
            .px
            .iter()
            .map(|p| {
                let a = java_round(p[3]).clamp(0, 255) as u32;
                if a == 0 {
                    return 0;
                }
                let ch = |v: f32| java_round(v).clamp(0, 255) as u32;
                (a << 24) | (ch(p[0]) << 16) | (ch(p[1]) << 8) | ch(p[2])
            })
            .collect();
        PinImage {
            w: self.w,
            h: self.h,
            argb,
            anchor,
            label,
        }
    }
}

fn channels(rgb: u32) -> [f32; 3] {
    [
        ((rgb >> 16) & 0xFF) as f32,
        ((rgb >> 8) & 0xFF) as f32,
        (rgb & 0xFF) as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLUE: u32 = 0x0090FF;

    fn alpha(img: &PinImage, x: usize, y: usize) -> u32 {
        img.at(x, y) >> 24
    }

    #[test]
    fn disc_is_symmetric() {
        for d in [LARGE, SMALL] {
            let img = badge(BLUE, false, d, 0, None, None);
            assert_eq!((img.w, img.h), (d as usize, d as usize));
            for y in 0..img.h {
                for x in 0..img.w {
                    assert_eq!(alpha(&img, x, y), alpha(&img, img.w - 1 - x, y));
                    assert_eq!(alpha(&img, x, y), alpha(&img, x, img.h - 1 - y));
                }
            }
            assert_eq!(img.anchor, [d as f32 / 2.0; 2]);
        }
        // The standing pin mirrors left to right, shadow included, about its anchor.
        let pin = standing(BLUE, false, LARGE, 0);
        assert_eq!(pin.anchor, [9.5, 21.0]);
        for y in 0..pin.h {
            for x in 0..pin.w {
                assert_eq!(alpha(&pin, x, y), alpha(&pin, pin.w - 1 - x, y));
            }
        }
    }

    #[test]
    fn rim_is_ink_at_094() {
        let img = badge(BLUE, false, LARGE, 0, None, None);
        // The middle of the top row is rim.
        let rim = img.at(7, 0);
        assert_eq!(rim & 0xFF_FFFF, INK);
        assert_eq!(rim >> 24, 240);
        // Inside: the face, lit just under the rim at the top, darker at the bottom.
        assert_eq!(img.at(7, 7), 0xFF00_0000 | BLUE);
        assert_eq!(img.at(7, 1), 0xFF00_0000 | mix(BLUE, 0xFFFFFF, 0.35));
        assert_eq!(img.at(7, 13), 0xFF00_0000 | mix(BLUE, 0, 0.25));
    }

    #[test]
    fn standing_shadow_and_label() {
        let pin = standing(BLUE, false, SMALL, 0);
        assert_eq!((pin.w, pin.h), (17, 21));
        assert_eq!(pin.label, [3, 1, 14, 12]);
        // The shadow row under the point: black at 0.28.
        let shadow = pin.at(pin.anchor[0] as usize, pin.h - 3);
        assert_eq!(shadow, 71 << 24);
    }

    #[test]
    fn pill_widens_with_stretch() {
        assert_eq!(stretch_for(LARGE, 11), 0);
        assert_eq!(stretch_for(LARGE, 12), 6);
        assert_eq!(stretch_for(SMALL, 9), 0);
        assert_eq!(stretch_for(SMALL, 10), 6);
        assert_eq!(stretch_for(LARGE, 13) % 2, 0);
        let round = badge(BLUE, false, LARGE, 0, None, None);
        let pill = badge(BLUE, false, LARGE, 6, None, None);
        assert_eq!(pill.w, round.w + 6);
        let span = |img: &PinImage| (0..img.w).filter(|&x| alpha(img, x, img.h / 2) > 0).count();
        assert_eq!(span(&pill), span(&round) + 6);
        assert_eq!(pill.label, [1, 1, 20, 14]);
        let tall = standing(BLUE, false, LARGE, 4);
        assert_eq!(tall.w, 23);
        assert_eq!(tall.anchor[0], 11.5);
        // The point stays under the middle: rim there, only shadow beside it.
        assert_eq!(tall.at(11, 20), 240 << 24 | INK);
        assert_eq!((alpha(&tall, 10, 20), alpha(&tall, 12, 20)), (71, 71));
    }

    #[test]
    fn caret_points_toward_the_angle() {
        for step in 0..CARET_STEPS {
            let angle = caret_angle(step);
            for stretch in [0, 6] {
                let img = badge(
                    BLUE,
                    false,
                    LARGE,
                    stretch,
                    Some((angle, caret_size(LARGE))),
                    None,
                );
                let [cx, cy] = img.anchor;
                let (r, half) = (LARGE as f32 / 2.0, stretch as f32 / 2.0);
                let (mut sx, mut sy, mut n) = (0.0f32, 0.0f32, 0);
                for y in 0..img.h {
                    for x in 0..img.w {
                        let (px, py) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                        let dx = (px.abs() - half).max(0.0);
                        if img.at(x, y) == 0xFF00_0000 | BLUE && dx * dx + py * py > r * r {
                            sx += px;
                            sy += py;
                            n += 1;
                        }
                    }
                }
                assert!(n > 4, "caret at step {step} has {n} pixels");
                let got = (sy as f64).atan2(sx as f64);
                let diff =
                    (got - angle + std::f64::consts::PI).rem_euclid(TAU) - std::f64::consts::PI;
                let allowed = if stretch == 0 { 0.2 } else { 0.6 };
                assert!(
                    diff.abs() < allowed,
                    "step {step} stretch {stretch}: {got} vs {angle}"
                );
            }
        }
        assert_eq!(caret_step(0.0), 0);
        assert_eq!(caret_step(-TAU / 32.0), 31);
        assert_eq!(caret_step(TAU * 0.25), 8);
    }

    #[test]
    fn death_shows_a_headstone() {
        let pin = standing(BLUE, true, LARGE, 0);
        assert!(pin.argb.contains(&0xFFC2_C2D1));
        assert!(pin.argb.contains(&(0xFF00_0000 | DEATH_FACE)));
        assert!(!pin.argb.contains(&(0xFF00_0000 | BLUE)));
    }

    #[test]
    fn icon_sits_on_its_head() {
        let icon = PinImage {
            w: 11,
            h: 11,
            argb: vec![0xFF80_4020; 121],
            anchor: [0.0; 2],
            label: [0; 4],
        };
        assert_eq!(icon_head(&icon), 21);
        let pin = icon_pin(&icon, BLUE);
        assert_eq!((pin.w, pin.h), (25, 30));
        assert_eq!(pin.at(12, 10), 0xFF80_4020);
        assert_eq!(pin.at(12, 4), 0xFF00_0000 | STICKER_WHITE);
        let b = badge(BLUE, false, icon_head(&icon), 0, None, Some(&icon));
        assert_eq!(b.label, [0; 4]);
    }

    #[test]
    fn runs_rebuild_the_image() {
        let img = badge(0xE5484D, false, LARGE, 4, Some((1.0, 6)), None);
        let mut back = vec![0u32; img.w * img.h];
        for (x, y, w, c) in runs(&img) {
            assert!(w > 0 && c >> 24 > 0);
            for k in 0..w {
                back[y as usize * img.w + (x + k) as usize] = c;
            }
        }
        assert_eq!(back, img.argb);
    }

    #[test]
    fn helpers() {
        assert_eq!(centred(0, 10, 7), 2);
        assert_eq!(centred(1, 14, 7), 4);
        assert!(light(0xFFFFFF) && !light(0x101418));
        assert_eq!(head_above(15), 13.5);
        assert_eq!(head_above(13), 11.5);
    }

    /// Writes pins into the temp directory as PNGs, eight times up, to look at.
    #[test]
    #[ignore]
    fn pins_png() {
        let pins = [
            standing(0xE5484D, false, LARGE, 0),
            standing(0x46A758, false, SMALL, 0),
            standing(0x0090FF, false, LARGE, 6),
            standing(0x60646C, true, LARGE, 0),
            badge(0xFFC53D, false, LARGE, 0, Some((0.6, 6)), None),
            badge(0x8E4EC6, false, SMALL, 2, Some((2.5, 5)), None),
        ];
        let k = 8;
        let w: usize = pins.iter().map(|p| p.w + 2).sum::<usize>() * k;
        let h = pins.iter().map(|p| p.h).max().unwrap() * k;
        let mut rgba = vec![0u8; w * h * 4];
        for px in rgba.chunks_mut(4) {
            px.copy_from_slice(&[0x8C, 0x99, 0xAB, 255]);
        }
        let mut ox = 0;
        for p in &pins {
            for (x, y, rw, c) in runs(p) {
                let a = (c >> 24) as f32 / 255.0;
                for yy in y as usize * k..(y as usize + 1) * k {
                    for xx in (ox + x as usize) * k..(ox + (x + rw) as usize) * k {
                        let o = (yy * w + xx) * 4;
                        for (ch, shift) in [16, 8, 0].iter().enumerate() {
                            let s = ((c >> shift) & 0xFF) as f32;
                            rgba[o + ch] = (s * a + rgba[o + ch] as f32 * (1.0 - a)) as u8;
                        }
                    }
                }
            }
            ox += p.w + 2;
        }
        let path = std::env::temp_dir().join("diorama_pins.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgba).unwrap();
        eprintln!("wrote {}", path.display());
    }
}
