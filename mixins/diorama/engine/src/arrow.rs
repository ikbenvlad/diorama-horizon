//! The player arrow as a little voxel model: a dart built from one-unit blocks on the world grid,
//! seen through the map's own camera and lit by its sun, with its shadow on the ground and a dark
//! rim. One block is one GUI pixel.
//!
//! Ported from Diorama's `render/ArrowArt.java`. Upstream rasterises the model 16 times
//! supersampled into a 36-GUI-pixel texture; here the same model comes out as flat polygons the
//! glue fills in order, so it needs no texture and stays sharp at any GUI scale. Under the camera
//! every block shows as axis-aligned rectangles (its top and its south side), so a row of blocks
//! merges into a few rectangles, and the rim round the shape is the exact union of the blocks'
//! outlines grown by a pixel.

use crate::baker::{SUN_AZIMUTH_X, SUN_AZIMUTH_Z};
use crate::pin_art::{inside, mix};
use crate::style::MapStyle;

/// GUI pixels across the drawing upstream; the arrow stays within half of it from its centre.
pub const GUI_SIZE: f32 = 36.0;
/// The accent colours the settings offer, 0xRRGGBB (Settings.java:258).
pub const ACCENTS: [u32; 8] = [
    0x3FA9F5, 0xEF4B4B, 0x62D96B, 0xFFC53D, 0xF76B15, 0x8E4EC6, 0xD6409F, 0xF2F2F2,
];
/// The accents' names, for the settings (Settings.java:259).
pub const ACCENT_NAMES: [&str; 8] = [
    "blue", "red", "green", "yellow", "orange", "purple", "pink", "white",
];
const WHITE: u32 = 0xFFFFFF;
const LIT_SIDE: u32 = 0xE8EEF4;
const DARK: u32 = 0x101418;
/// The ground shadow's alpha (ArrowArt.java:84).
const SHADOW_ALPHA: f32 = 0.28;
/// The rim's alpha and reach in GUI pixels (ArrowArt.java:84, 313).
const RIM_ALPHA: f32 = 0.9;
const RIM: f64 = 1.0;

/// A filled quad in GUI pixels from the arrow's centre, its corners turning counter-clockwise on
/// screen; a triangle repeats its last corner. 0xAARRGGBB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Poly {
    pub pts: [[f32; 2]; 4],
    pub argb: u32,
}

/// The arrow facing `yaw_on_map_degrees` (Minecraft's yaw turned to the map: 0 south, 90 west),
/// `size` GUI pixels from the centre to the tip of upstream's flat arrow, under a camera tilted so
/// north-south distances shrink by `sin` and heights by `cos`. The polygons are drawn in order:
/// the shadow, the rim, then the blocks back to front (ArrowArt.java:33-85).
pub fn voxel(yaw_on_map_degrees: f64, size: f32, sin: f64, cos: f64, accent: u32) -> Vec<Poly> {
    let accent = accent & 0xFF_FFFF;
    let yaw = yaw_on_map_degrees.rem_euclid(360.0);
    // The sides in shade take a darker accent.
    let shade = mix(accent, 0x000000, 0.33);
    let s = size * 1.15;
    let outer = turn(&dart(s), yaw);
    let inner = turn(&dart(s * 0.5), yaw);
    let range = (size * 1.3) as i32 + 2;
    // (i, j, in the inner dart), by rows north to south, each west to east.
    let mut cells = Vec::new();
    for j in -range..=range {
        for i in -range..=range {
            if inside(&outer, i as f32 + 0.5, j as f32 + 0.5) {
                cells.push((i, j, inside(&inner, i as f32 + 0.5, j as f32 + 0.5)));
            }
        }
    }
    if cells.is_empty() {
        return Vec::new();
    }
    let project = |x: f64, y: f64, z: f64| [x, z * sin - y * cos];
    // Centre the drawn blocks, so the arrow sits on the player's spot at every heading.
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    let mut corners = Vec::with_capacity(cells.len() * 8);
    for &(i, j, _) in &cells {
        for (dx, dz) in [(0, 0), (1, 0), (1, 1), (0, 1)] {
            for y in [0.0, 1.0] {
                let (x, z) = ((i + dx) as f64, (j + dz) as f64);
                corners.push([x, y, z]);
                let p = project(x, y, z);
                for k in 0..2 {
                    lo[k] = lo[k].min(p[k]);
                    hi[k] = hi[k].max(p[k]);
                }
            }
        }
    }
    let centre = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0];
    let at = |p: [f64; 2]| [(p[0] - centre[0]) as f32, (p[1] - centre[1]) as f32];
    let mut out = Vec::new();

    // The shadow: every corner pushed down to the ground along the sunlight, then their outline.
    let sun = sun();
    let flat: Vec<[f64; 2]> = corners
        .iter()
        .map(|p| {
            let t = p[1] / sun[1];
            [p[0] - sun[0] * t, p[2] - sun[2] * t]
        })
        .collect();
    let shadow: Vec<[f32; 2]> = hull(flat)
        .into_iter()
        .map(|p| at([p[0], p[1] * sin]))
        .collect();
    fan(&mut out, &shadow, alpha(0x000000, SHADOW_ALPHA));

    // The rim: each block's outline on screen grown by a pixel, as one union of rectangles.
    let outlines: Vec<[f64; 4]> = cells
        .iter()
        .map(|&(i, j, _)| {
            let ys = [
                project(0.0, 0.0, j as f64)[1],
                project(0.0, 1.0, j as f64)[1],
                project(0.0, 0.0, (j + 1) as f64)[1],
                project(0.0, 1.0, (j + 1) as f64)[1],
            ];
            let y0 = ys.iter().copied().fold(f64::MAX, f64::min);
            let y1 = ys.iter().copied().fold(f64::MIN, f64::max);
            [i as f64 - RIM, y0 - RIM, (i + 1) as f64 + RIM, y1 + RIM]
        })
        .collect();
    let rim = alpha(DARK, RIM_ALPHA);
    for r in union(&outlines) {
        push(
            &mut out,
            [
                at([r[0], r[1]]),
                at([r[0], r[3]]),
                at([r[2], r[3]]),
                at([r[2], r[1]]),
            ],
            rim,
        );
    }

    // The blocks, a row at a time from the back; in a row they never overlap, so each face of a
    // run of blocks is one quad, its top split where the inner dart starts and ends.
    let faces = faces(sin, cos);
    let mut row_start = 0;
    while row_start < cells.len() {
        let j = cells[row_start].1;
        let row_end = row_start + cells[row_start..].iter().take_while(|c| c.1 == j).count();
        let row = &cells[row_start..row_end];
        for face in &faces {
            // Tops take the dart's colours; sides the sun's light on the shade.
            let colour = (face.normal[1] <= 0.0).then(|| material(face.normal, shade));
            // Spans of blocks this face is one quad over: runs of neighbours of one top colour.
            let mut k = 0;
            while k < row.len() {
                let (i0, _, accented) = row[k];
                let mut n = 1;
                while k + n < row.len()
                    && row[k + n].0 == i0 + n as i32
                    && (colour.is_some() || row[k + n].2 == accented)
                    && face.normal[0] == 0.0
                {
                    n += 1;
                }
                let argb = colour.unwrap_or(if accented { accent } else { WHITE });
                let pts = face.pts.map(|p| {
                    let x = if p[0] == 0.0 {
                        i0 as f64
                    } else {
                        (i0 + n as i32) as f64
                    };
                    at(project(x, p[1], p[2] + j as f64))
                });
                push(&mut out, pts, 0xFF00_0000 | argb);
                k += n;
            }
        }
        row_start = row_end;
    }
    out
}

/// The map's sun for the default style: from the west and a little south, 38 degrees up, the
/// light the tiles are baked with (ArrowArt.java:23).
fn sun() -> [f64; 3] {
    let e = MapStyle::DEFAULT.sun_elevation.to_radians();
    normalise([SUN_AZIMUTH_X * e.cos(), e.sin(), SUN_AZIMUTH_Z * e.cos()])
}

/// The arrow in the ground plane pointing +x: tip, right wing, notch, left wing. Its tip reaches
/// well past its wings, so it never reads sideways.
fn dart(s: f32) -> [f32; 8] {
    [
        1.3 * s,
        0.0,
        -0.7 * s,
        0.55 * s,
        -0.3 * s,
        0.0,
        -0.7 * s,
        -0.55 * s,
    ]
}

/// Turns ground points (forward = +x) to face Minecraft yaw, as (x, z) pairs.
fn turn(pts: &[f32; 8], yaw_degrees: f64) -> [f32; 8] {
    let yaw = yaw_degrees.to_radians();
    let (fx, fz) = (-yaw.sin(), yaw.cos());
    let (px, pz) = (-fz, fx);
    let mut out = [0.0; 8];
    for i in (0..8).step_by(2) {
        let (a, b) = (pts[i] as f64, pts[i + 1] as f64);
        out[i] = (fx * a + px * b) as f32;
        out[i + 1] = (fz * a + pz * b) as f32;
    }
    out
}

/// A face of the block at the origin: its corners (x, y, z, each 0 or 1) and outward normal.
struct Face {
    pts: [[f64; 3]; 4],
    normal: [f64; 3],
}

/// The faces of a block the camera sees, far ones first (ArrowArt.java:108-136). The same for
/// every block, so worked out once.
fn faces(sin: f64, cos: f64) -> Vec<Face> {
    let base = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let mut faces = vec![Face {
        pts: [
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.0, 1.0, 1.0],
        ],
        normal: [0.0, 1.0, 0.0],
    }];
    for k in 0..4 {
        let ([x0, z0], [x1, z1]) = (base[k], base[(k + 1) % 4]);
        faces.push(Face {
            pts: [[x0, 0.0, z0], [x1, 0.0, z1], [x1, 1.0, z1], [x0, 1.0, z0]],
            normal: [z1 - z0, 0.0, -(x1 - x0)],
        });
    }
    // The camera looks along (0, -sin, -cos): a face shows when its normal points back up it.
    faces.retain(|f| f.normal[1] * sin + f.normal[2] * cos > 1e-9);
    let depth = |f: &Face| f.pts.iter().map(|p| -p[1] * sin - p[2] * cos).sum::<f64>();
    faces.sort_by(|a, b| depth(b).total_cmp(&depth(a)));
    faces
}

/// Warm light on faces toward the sun, the cool shade colour away from it, like the map's grade.
fn material(n: [f64; 3], shade: u32) -> u32 {
    let sun = sun();
    let u = normalise(n);
    let lit = (u[0] * sun[0] + u[1] * sun[1] + u[2] * sun[2]).max(0.0) / sun[1];
    mix(shade, LIT_SIDE, lit.min(1.0) as f32)
}

fn normalise(v: [f64; 3]) -> [f64; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / l, v[1] / l, v[2] / l]
}

fn alpha(rgb: u32, a: f32) -> u32 {
    (((a * 255.0 + 0.5).floor() as u32) << 24) | rgb
}

/// The convex hull, counter-clockwise in (x, z) (Andrew's monotone chain, as upstream).
fn hull(mut pts: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    let cross = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };
    let chain = |iter: &mut dyn Iterator<Item = &[f64; 2]>| {
        let mut out: Vec<[f64; 2]> = Vec::new();
        for &p in iter {
            while out.len() >= 2 && cross(out[out.len() - 2], out[out.len() - 1], p) <= 0.0 {
                out.pop();
            }
            out.push(p);
        }
        out.pop();
        out
    };
    let mut lower = chain(&mut pts.iter());
    lower.extend(chain(&mut pts.iter().rev()));
    lower
}

/// A convex polygon as a fan of quads from its first corner.
fn fan(out: &mut Vec<Poly>, pts: &[[f32; 2]], argb: u32) {
    let mut k = 1;
    while k + 1 < pts.len() {
        let last = pts[(k + 2).min(pts.len() - 1)];
        push(out, [pts[0], pts[k], pts[k + 1], last], argb);
        k += 2;
    }
}

/// Adds a quad, turned counter-clockwise on screen (negative area with y down) if it isn't.
fn push(out: &mut Vec<Poly>, mut pts: [[f32; 2]; 4], argb: u32) {
    if area(&pts) > 0.0 {
        pts.reverse();
    }
    out.push(Poly { pts, argb });
}

fn area(pts: &[[f32; 2]; 4]) -> f32 {
    (0..4)
        .map(|k| {
            let (a, b) = (pts[k], pts[(k + 1) % 4]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum::<f32>()
        / 2.0
}

/// The union of rectangles (x0, y0, x1, y1) as rectangles that don't overlap: cut into bands at
/// every top and bottom, the spans in each band merged, and bands that match the one above joined.
fn union(rects: &[[f64; 4]]) -> Vec<[f64; 4]> {
    let mut ys: Vec<f64> = rects.iter().flat_map(|r| [r[1], r[3]]).collect();
    ys.sort_by(f64::total_cmp);
    ys.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let mut out: Vec<[f64; 4]> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    for band in ys.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        let mid = (y0 + y1) / 2.0;
        let mut spans: Vec<[f64; 2]> = rects
            .iter()
            .filter(|r| r[1] < mid && r[3] > mid)
            .map(|r| [r[0], r[2]])
            .collect();
        spans.sort_by(|a, b| a[0].total_cmp(&b[0]));
        let mut merged: Vec<[f64; 2]> = Vec::new();
        for s in spans {
            match merged.last_mut() {
                Some(m) if s[0] <= m[1] + 1e-9 => m[1] = m[1].max(s[1]),
                _ => merged.push(s),
            }
        }
        let same = merged.len() == open.len()
            && merged.iter().zip(&open).all(|(s, &o)| {
                (out[o][0] - s[0]).abs() < 1e-9
                    && (out[o][2] - s[1]).abs() < 1e-9
                    && (out[o][3] - y0).abs() < 1e-9
            });
        if same {
            for &o in &open {
                out[o][3] = y1;
            }
        } else {
            open.clear();
            for s in merged {
                open.push(out.len());
                out.push([s[0], y0, s[1], y1]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default style's camera: 50 degrees.
    fn camera() -> (f64, f64) {
        let p = MapStyle::DEFAULT.pitch_degrees.to_radians();
        (p.sin(), p.cos())
    }

    #[test]
    fn a_turn_is_the_same_arrow() {
        let (sin, cos) = camera();
        for yaw in [0.0, 37.0, 90.0, 181.5, 300.0] {
            let a = voxel(yaw, 5.0, sin, cos, ACCENTS[0]);
            assert_eq!(a, voxel(yaw + 360.0, 5.0, sin, cos, ACCENTS[0]));
            assert_eq!(a, voxel(yaw - 720.0, 5.0, sin, cos, ACCENTS[0]));
        }
    }

    #[test]
    fn stays_inside_its_canvas() {
        let (sin, cos) = camera();
        for size in [5.0, 7.0] {
            for step in 0..72 {
                let polys = voxel(step as f64 * 5.0, size, sin, cos, ACCENTS[1]);
                assert!(polys.len() > 4);
                for p in &polys {
                    for q in p.pts {
                        assert!(
                            q[0].abs() < GUI_SIZE / 2.0 && q[1].abs() < GUI_SIZE / 2.0,
                            "{q:?} at {step}"
                        );
                    }
                    // Every quad turns the same way.
                    assert!(area(&p.pts) <= 0.0);
                }
            }
        }
    }

    #[test]
    fn layers_in_order() {
        let (sin, cos) = camera();
        let accent = ACCENTS[2];
        let polys = voxel(30.0, 5.0, sin, cos, accent);
        // The shadow first, black at 0.28; then the rim, dark at 0.9; then opaque blocks.
        assert_eq!(polys[0].argb, 71 << 24);
        let rim = polys.iter().position(|p| p.argb != 71 << 24).unwrap();
        assert_eq!(polys[rim].argb, 0xE610_1418);
        let blocks = polys.iter().position(|p| p.argb >> 24 == 0xFF).unwrap();
        assert!(polys[rim..blocks].iter().all(|p| p.argb == 0xE610_1418));
        assert!(polys[blocks..].iter().all(|p| p.argb >> 24 == 0xFF));
        // Tops white and the accent's inner dart; sides lit by the sun from the south-west.
        assert!(has(&polys, 0xFFFF_FFFF));
        assert!(has(&polys, 0xFF00_0000 | accent));
        let south = 0xFF00_0000 | material([0.0, 0.0, 1.0], mix(accent, 0, 0.33));
        assert!(has(&polys, south));
        assert_ne!(south, 0xFF00_0000 | accent);
    }

    #[test]
    fn tops_are_white_at_the_back() {
        let (sin, cos) = camera();
        let polys = voxel(0.0, 5.0, sin, cos, ACCENTS[0]);
        // The topmost block face on screen is a top, so white or accent, never a side.
        let blocks: Vec<&Poly> = polys.iter().filter(|p| p.argb >> 24 == 0xFF).collect();
        let top = blocks
            .iter()
            .min_by(|a, b| {
                let ya = a.pts.iter().map(|q| q[1]).fold(f32::MAX, f32::min);
                let yb = b.pts.iter().map(|q| q[1]).fold(f32::MAX, f32::min);
                ya.total_cmp(&yb)
            })
            .unwrap();
        assert_eq!(top.argb, 0xFFFF_FFFF);
    }

    #[test]
    fn union_does_not_overlap() {
        let rects = [
            [0.0, 0.0, 2.0, 2.0],
            [1.0, 1.0, 3.0, 3.0],
            [5.0, 0.0, 6.0, 1.0],
        ];
        let u = union(&rects);
        let total: f64 = u.iter().map(|r| (r[2] - r[0]) * (r[3] - r[1])).sum();
        assert!((total - (4.0 + 4.0 - 1.0 + 1.0)).abs() < 1e-9);
    }

    fn has(polys: &[Poly], argb: u32) -> bool {
        polys.iter().any(|p| p.argb == argb)
    }

    /// Fills the polygons, 8 samples a pixel each way, into an RGBA image `scale` pixels per GUI
    /// pixel over a grey backdrop.
    fn raster(polys: &[Poly], scale: usize) -> (usize, Vec<u8>) {
        let w = GUI_SIZE as usize * scale;
        let mut img = vec![[0x8C as f32, 0x99 as f32, 0xAB as f32]; w * w];
        for p in polys {
            let flat: Vec<f32> = p
                .pts
                .iter()
                .flat_map(|q| {
                    [
                        (q[0] + GUI_SIZE / 2.0) * scale as f32,
                        (q[1] + GUI_SIZE / 2.0) * scale as f32,
                    ]
                })
                .collect();
            let a = (p.argb >> 24) as f32 / 255.0;
            let c = [
                ((p.argb >> 16) & 0xFF) as f32,
                ((p.argb >> 8) & 0xFF) as f32,
                (p.argb & 0xFF) as f32,
            ];
            for y in 0..w {
                for x in 0..w {
                    let mut hits = 0;
                    for s in 0..64 {
                        let (sx, sy) = (
                            x as f32 + (s % 8) as f32 / 8.0 + 1.0 / 16.0,
                            y as f32 + (s / 8) as f32 / 8.0 + 1.0 / 16.0,
                        );
                        hits += inside(&flat, sx, sy) as i32;
                    }
                    let k = a * hits as f32 / 64.0;
                    let px = &mut img[y * w + x];
                    for ch in 0..3 {
                        px[ch] = c[ch] * k + px[ch] * (1.0 - k);
                    }
                }
            }
        }
        (
            w,
            img.iter()
                .flat_map(|p| [p[0] as u8, p[1] as u8, p[2] as u8, 255])
                .collect(),
        )
    }

    /// Writes arrows at eight headings into the temp directory as a PNG, to look at.
    #[test]
    #[ignore]
    fn arrows_png() {
        let (sin, cos) = camera();
        let scale = 4;
        let tiles: Vec<(usize, Vec<u8>)> = (0..8)
            .map(|k| raster(&voxel(k as f64 * 45.0, 7.0, sin, cos, ACCENTS[k]), scale))
            .collect();
        let w = tiles[0].0;
        let mut rgba = vec![0u8; w * 8 * w * 4];
        for (k, (_, img)) in tiles.iter().enumerate() {
            for y in 0..w {
                let row = &img[y * w * 4..(y + 1) * w * 4];
                let o = (y * w * 8 + k * w) * 4;
                rgba[o..o + w * 4].copy_from_slice(row);
            }
        }
        let path = std::env::temp_dir().join("diorama_arrows.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), (w * 8) as u32, w as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgba).unwrap();
        eprintln!("wrote {}", path.display());
    }
}
