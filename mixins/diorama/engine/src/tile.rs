//! Tiles of the map and their pixels.
//!
//! Pixels are `u32` ABGR, which is RGBA in memory on little-endian machines, alpha meaning how much
//! of the pixel's samples hit known ground: 0 is nothing known (fog), 255 fully known.

use crate::projection::{COVERAGE_CELLS, PIXELS_PER_UNIT, TILE_PIXELS, TILE_UNITS};

/// One tile of the map. `layer` is [`TileKey::SURFACE`] for the sky view, or the cut height for a
/// cave slice. `lod` 0 is a baked tile; each level above covers twice the distance in the same
/// pixels, made by shrinking its four children, for zooming far out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub layer: i32,
    pub u: i32,
    pub v: i32,
    pub lod: u8,
}

impl TileKey {
    pub const SURFACE: i32 = i32::MIN;
    /// Coarsest overview level: one tile covers 4096 blocks across.
    pub const MAX_LOD: u8 = 5;

    pub fn new(layer: i32, u: i32, v: i32) -> TileKey {
        TileKey {
            layer,
            u,
            v,
            lod: 0,
        }
    }

    pub fn is_cave(&self) -> bool {
        self.layer != Self::SURFACE
    }

    /// Map units across this tile.
    pub fn units(&self) -> f64 {
        f64::from(TILE_UNITS) * f64::from(1u32 << self.lod)
    }

    pub fn min_u(&self) -> f64 {
        f64::from(self.u) * self.units()
    }

    pub fn min_v(&self) -> f64 {
        f64::from(self.v) * self.units()
    }

    /// The overview tile one level up that contains this one.
    pub fn parent(&self) -> TileKey {
        TileKey {
            layer: self.layer,
            u: self.u.div_euclid(2),
            v: self.v.div_euclid(2),
            lod: self.lod + 1,
        }
    }

    /// The four tiles one level down, in the order (0, 0), (1, 0), (0, 1), (1, 1).
    pub fn children(&self) -> [TileKey; 4] {
        let c = |du: i32, dv: i32| TileKey {
            layer: self.layer,
            u: self.u * 2 + du,
            v: self.v * 2 + dv,
            lod: self.lod - 1,
        };
        [c(0, 0), c(1, 0), c(0, 1), c(1, 1)]
    }
}

pub const PIXELS: usize = (TILE_PIXELS * TILE_PIXELS) as usize;

/// Pixels per side of the squares a tile is baked again in, when only part of it changed.
pub const PATCH: usize = 32;
/// Patches per row of a tile.
pub const PATCHES: usize = TILE_PIXELS as usize / PATCH;
const PATCH_WORDS: usize = PATCHES * PATCHES / 64;

/// Which patches of a tile are to be baked again: patch `(x, y)` is bit `y * PATCHES + x`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Patches([u64; PATCH_WORDS]);

impl Patches {
    pub const ALL: Patches = Patches([u64::MAX; PATCH_WORDS]);
    pub const NONE: Patches = Patches([0; PATCH_WORDS]);

    /// The patches from `(x0, y0)` to `(x1, y1)`, both included; the parts outside the tile are
    /// left out.
    pub fn rect(x0: i64, y0: i64, x1: i64, y1: i64) -> Patches {
        let mut p = Patches::NONE;
        let last = PATCHES as i64 - 1;
        let (x0, x1) = (x0.max(0), x1.min(last));
        let (y0, y1) = (y0.max(0), y1.min(last));
        for y in y0..=y1 {
            for x in x0..=x1 {
                let i = y as usize * PATCHES + x as usize;
                p.0[i / 64] |= 1 << (i % 64);
            }
        }
        p
    }

    pub fn contains(&self, x: usize, y: usize) -> bool {
        let i = y * PATCHES + x;
        self.0[i / 64] >> (i % 64) & 1 == 1
    }

    pub fn union(self, other: Patches) -> Patches {
        Patches(std::array::from_fn(|i| self.0[i] | other.0[i]))
    }

    pub fn count(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    pub fn is_all(&self) -> bool {
        *self == Patches::ALL
    }

    /// The patches as runs along each row: `(row, first column, one past the last)`.
    pub fn runs(&self) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        for y in 0..PATCHES {
            let mut x = 0;
            while x < PATCHES {
                if !self.contains(x, y) {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < PATCHES && self.contains(x, y) {
                    x += 1;
                }
                out.push((y, start, x));
            }
        }
        out
    }
}

/// A tile's pixels, `TILE_PIXELS` square, and what is saved with them.
#[derive(Clone, Debug)]
pub struct TileImage {
    pub key: TileKey,
    pub pixels: Vec<u32>,
    /// The sky view's block light, [`light::EDGE`] square (see [`light`]); none in cave slices,
    /// under a roof, and in tiles saved before it was kept.
    pub light: Option<Vec<u8>>,
    /// Where the ground is under each sample of a level-0 sky view tile (see [`spots`]); none
    /// elsewhere, and in tiles saved before they were kept.
    pub spots: Option<Vec<u32>>,
}

impl TileImage {
    pub fn blank(key: TileKey) -> TileImage {
        TileImage {
            key,
            pixels: vec![0; PIXELS],
            light: None,
            spots: None,
        }
    }

    /// A tile of these pixels with nothing saved besides.
    pub fn of_pixels(key: TileKey, pixels: Vec<u32>) -> TileImage {
        TileImage {
            key,
            pixels,
            light: None,
            spots: None,
        }
    }

    /// Whether any pixel is known.
    pub fn any_known(&self) -> bool {
        self.pixels.iter().any(|p| p >> 24 != 0)
    }

    /// Keeps the saved pixel wherever the fresh one is less known.
    pub fn merge_under(&mut self, saved: &[u32]) {
        for (p, &old) in self.pixels.iter_mut().zip(saved) {
            if old >> 24 > *p >> 24 {
                *p = old;
            }
        }
    }

    /// How much of each coverage cell is known, 0..255.
    pub fn coverage(&self) -> Vec<u8> {
        let cell = (TILE_PIXELS / COVERAGE_CELLS) as usize;
        let size = TILE_PIXELS as usize;
        let cells = COVERAGE_CELLS as usize;
        let mut out = vec![0u8; cells * cells];
        for cy in 0..cells {
            for cx in 0..cells {
                let mut sum = 0u32;
                for y in 0..cell {
                    let row = (cy * cell + y) * size + cx * cell;
                    for p in &self.pixels[row..row + cell] {
                        sum += p >> 24;
                    }
                }
                out[cy * cells + cx] = (sum / (cell * cell) as u32) as u8;
            }
        }
        out
    }
}

/// Alpha-weighted average of four ABGR pixels, so unknown pixels don't darken known ones.
pub fn average4(p: [u32; 4]) -> u32 {
    let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
    for c in p {
        let ca = c >> 24;
        r += (c & 0xFF) * ca;
        g += ((c >> 8) & 0xFF) * ca;
        b += ((c >> 16) & 0xFF) * ca;
        a += ca;
    }
    if a == 0 {
        return 0;
    }
    let half = a / 2;
    ((a / 4) << 24) | (((b + half) / a) << 16) | (((g + half) / a) << 8) | ((r + half) / a)
}

/// Halves a square image of `size` pixels.
pub fn shrink(src: &[u32], size: usize) -> Vec<u32> {
    let half = size / 2;
    let mut out = vec![0u32; half * half];
    for y in 0..half {
        for x in 0..half {
            let i = 2 * y * size + 2 * x;
            out[y * half + x] = average4([src[i], src[i + 1], src[i + size], src[i + size + 1]]);
        }
    }
    out
}

/// The mip chain of a tile: level 0 is the tile itself, each next one half as large, down to 1.
pub fn mips(base: &[u32], size: usize) -> Vec<Vec<u32>> {
    let mut levels = vec![base.to_vec()];
    let mut s = size;
    while s > 1 {
        let next = shrink(levels.last().expect("non-empty"), s);
        levels.push(next);
        s /= 2;
    }
    levels
}

/// An overview tile from its four children, each shrunk to a quarter; children missing count as
/// unknown. Pixels only: see [`light::overview`] for the light.
pub fn overview(key: TileKey, children: [Option<&[u32]>; 4]) -> TileImage {
    let size = TILE_PIXELS as usize;
    let half = size / 2;
    let mut out = TileImage::blank(key);
    for (i, child) in children.into_iter().enumerate() {
        let Some(child) = child else { continue };
        let small = shrink(child, size);
        let (ox, oy) = ((i & 1) * half, (i >> 1) * half);
        for y in 0..half {
            out.pixels[(oy + y) * size + ox..(oy + y) * size + ox + half]
                .copy_from_slice(&small[y * half..(y + 1) * half]);
        }
    }
    out
}

/// The block light of sky view tiles: torches, lamps and lava, which light the map at night
/// (upstream `BlockLight`). Kept at a quarter of the tile's resolution, a value per block across:
/// the block light level (0..15) where the samples of the pixels under it landed, times 17, so
/// 255 is level 15 everywhere; averaged weighted by how much of each pixel is known.
pub mod light {
    use super::{PIXELS_PER_UNIT, TILE_PIXELS};

    /// Pixels per light value each way.
    pub const SCALE: usize = PIXELS_PER_UNIT as usize;
    /// Light values per side of a tile.
    pub const EDGE: usize = TILE_PIXELS as usize / SCALE;
    pub const LEN: usize = EDGE * EDGE;

    /// A pixel's value from the sum of its known samples' levels.
    pub fn encode(sum: u32, known: u32) -> u8 {
        if known == 0 {
            0
        } else {
            ((sum * 17 + known / 2) / known) as u8
        }
    }

    /// The light of `width` x `height` freshly baked pixels at `(px0, py0)` of the tile (each a
    /// multiple of [`SCALE`]), from their own light `pixel_light`: where the saved pixel is
    /// better known and stays, as the merge after keeps it, the saved light under it (none if
    /// the saved tile has no light), else the pixel's own; shrunk [`SCALE`] times each way,
    /// weighted by how much of each pixel is known. Call before the merge, while `pixels` is the
    /// fresh bake.
    #[allow(clippy::too_many_arguments)]
    pub fn shrink(
        pixel_light: &[u8],
        pixels: &[u32],
        saved: Option<&[u32]>,
        saved_light: Option<&[u8]>,
        px0: usize,
        py0: usize,
        width: usize,
        height: usize,
    ) -> Vec<u8> {
        let size = TILE_PIXELS as usize;
        let (ow, oh) = (width / SCALE, height / SCALE);
        let mut out = vec![0u8; ow * oh];
        for ly in 0..oh {
            for lx in 0..ow {
                let (mut sum, mut weight) = (0u32, 0u32);
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let (x, y) = (lx * SCALE + dx, ly * SCALE + dy);
                        let i = y * width + x;
                        let mut alpha = pixels[i] >> 24;
                        let mut value = u32::from(pixel_light[i]);
                        if let Some(saved) = saved {
                            let saved_alpha = saved[(py0 + y) * size + px0 + x] >> 24;
                            if saved_alpha > alpha {
                                alpha = saved_alpha;
                                value = saved_light.map_or(0, |l| {
                                    u32::from(l[(py0 + y) / SCALE * EDGE + (px0 + x) / SCALE])
                                });
                            }
                        }
                        sum += value * alpha;
                        weight += alpha;
                    }
                }
                out[ly * ow + lx] = if weight == 0 {
                    0
                } else {
                    ((sum + weight / 2) / weight) as u8
                };
            }
        }
        out
    }

    /// Puts the light of an area made by [`shrink`] into a tile's.
    pub fn place(plane: &mut [u8], area: &[u8], px0: usize, py0: usize, width: usize) {
        let ow = width / SCALE;
        let (lx0, ly0) = (px0 / SCALE, py0 / SCALE);
        for (y, row) in area.chunks_exact(ow).enumerate() {
            let at = (ly0 + y) * EDGE + lx0;
            plane[at..at + ow].copy_from_slice(row);
        }
    }

    /// An overview tile's light from its four children's (their light and pixels), each shrunk
    /// to a quarter; none if no child has light. Each value is weighted by how much of the
    /// child's pixels under it are known, so unknown ground doesn't darken the glow at its edge.
    pub fn overview(children: [Option<(&[u8], &[u32])>; 4]) -> Option<Vec<u8>> {
        let mut out: Option<Vec<u8>> = None;
        let half = EDGE / 2;
        for (c, child) in children.into_iter().enumerate() {
            let Some((l, p)) = child else { continue };
            let out = out.get_or_insert_with(|| vec![0; LEN]);
            let (ox, oy) = ((c & 1) * half, (c >> 1) * half);
            for y in 0..half {
                for x in 0..half {
                    let (mut sum, mut weight) = (0u32, 0u32);
                    for d in 0..4 {
                        let (lx, ly) = (2 * x + (d & 1), 2 * y + (d >> 1));
                        let w = known(p, lx, ly);
                        sum += u32::from(l[ly * EDGE + lx]) * w;
                        weight += w;
                    }
                    out[(oy + y) * EDGE + ox + x] = if weight == 0 {
                        0
                    } else {
                        ((sum + weight / 2) / weight) as u8
                    };
                }
            }
        }
        out
    }

    /// How much of the pixels under light value `(lx, ly)` is known: their alphas added up.
    fn known(pixels: &[u32], lx: usize, ly: usize) -> u32 {
        let size = TILE_PIXELS as usize;
        (0..SCALE)
            .map(|dy| {
                let row = (ly * SCALE + dy) * size + lx * SCALE;
                pixels[row..row + SCALE].iter().map(|p| p >> 24).sum::<u32>()
            })
            .sum()
    }
}

/// Where the ground is under a level-0 sky view tile, for every block column and pixel row,
/// saved with the tile (upstream `TileSpots`): the map keeps showing a saved tile after its chunks
/// are gone, and the right-click menu needs to know how high the ground is there.
///
/// One `u32` per sample, column by column within a row: [`UNKNOWN`], [`NOTHING`], or a spot. A
/// spot holds the y a player would stand at (one above the block) and how many blocks short of
/// the ray's crossing of that height its z is (more than none when the ray hit the side of a
/// block), so it reads back with the sample's own row. Positions are turned, as the baker's are.
pub mod spots {
    use super::{PATCH, PIXELS_PER_UNIT, TILE_PIXELS, TILE_UNITS, TileKey};
    use crate::projection::Projection;

    pub const COLUMNS: usize = TILE_UNITS as usize;
    pub const ROWS: usize = TILE_PIXELS as usize;
    pub const LEN: usize = COLUMNS * ROWS;
    /// No answer: the chunks weren't there when the tile was baked.
    pub const UNKNOWN: u32 = 0;
    /// Nothing to stand on.
    pub const NOTHING: u32 = 1 << 24;
    const SPOT: u32 = 2 << 24;
    const TAG: u32 = 0xFF00_0000;
    /// Columns under a patch.
    pub const PATCH_COLUMNS: usize = PATCH / PIXELS_PER_UNIT as usize;

    /// The map v a sample answers for: the middle of its pixel row.
    pub fn row_v(key: &TileKey, row: usize) -> f64 {
        key.min_v() + (row as f64 + 0.5) / f64::from(PIXELS_PER_UNIT)
    }

    /// A spot standing at height `y` on turned row `z`, for the sample at map row `v`;
    /// [`UNKNOWN`] if it can't be kept.
    pub fn encode(p: &Projection, y: i32, z: i32, v: f64) -> u32 {
        let back = i64::from(p.z_at(v, f64::from(y)).floor() as i32) - i64::from(z);
        if !(0..=0xFF).contains(&back) || i32::from(y as i16) != y {
            return UNKNOWN;
        }
        SPOT | (back as u32) << 16 | (y as u32 & 0xFFFF)
    }

    /// The sample at map point `(u, v)` of a level-0 tile.
    pub fn index(key: &TileKey, u: f64, v: f64) -> usize {
        let column = (u - key.min_u()).floor().clamp(0.0, (COLUMNS - 1) as f64) as usize;
        let row = ((v - key.min_v()) * f64::from(PIXELS_PER_UNIT))
            .floor()
            .clamp(0.0, (ROWS - 1) as f64) as usize;
        row * COLUMNS + column
    }

    /// Whether the spots answer for map point `(u, v)`: the chunks were there when that part
    /// was baked.
    pub fn known(key: &TileKey, spots: &[u32], u: f64, v: f64) -> bool {
        spots[index(key, u, v)] != UNKNOWN
    }

    /// Where a player would stand at map point `(u, v)` of a level-0 tile, turned: none where
    /// nothing was saved or there's nothing to stand on.
    pub fn spot(p: &Projection, key: &TileKey, spots: &[u32], u: f64, v: f64) -> Option<[i32; 3]> {
        let i = index(key, u, v);
        let value = spots[i];
        if value & TAG != SPOT {
            return None;
        }
        let (column, row) = (i % COLUMNS, i / COLUMNS);
        let y = i32::from(value as u16 as i16);
        let z = p.z_at(row_v(key, row), f64::from(y)).floor() as i32 - ((value >> 16) & 0xFF) as i32;
        Some([key.min_u().floor() as i32 + column as i32, y, z])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_nest() {
        let k = TileKey::new(TileKey::SURFACE, -3, 5);
        let p = k.parent();
        assert_eq!((p.u, p.v, p.lod), (-2, 2, 1));
        assert!(p.children().contains(&k));
        assert_eq!(p.units(), 256.0);
        assert_eq!(p.min_u(), -512.0);
    }

    #[test]
    fn unknown_pixels_do_not_darken() {
        let white = 0xFFFF_FFFF;
        assert_eq!(average4([white, 0, 0, 0]), 0x3FFF_FFFF);
        assert_eq!(average4([0; 4]), 0);
        let m = mips(&[white; 16], 4);
        assert_eq!(m.len(), 3);
        assert_eq!(m[2], vec![white]);
    }

    #[test]
    fn patches_clip_and_run() {
        let p = Patches::rect(-3, 2, 1, 3);
        assert_eq!(p.count(), 4);
        assert_eq!(p.runs(), vec![(2, 0, 2), (3, 0, 2)]);
        let q = p.union(Patches::rect(14, 15, 99, 99));
        assert_eq!(q.runs(), vec![(2, 0, 2), (3, 0, 2), (15, 14, 16)]);
        assert!(Patches::rect(0, 0, 99, 99).is_all());
        assert!(Patches::rect(20, 0, 30, 5).is_empty());
    }

    #[test]
    fn overview_places_children() {
        let key = TileKey::new(TileKey::SURFACE, 0, 0).parent();
        let red = vec![0xFF00_00FFu32; PIXELS];
        let img = overview(key, [None, Some(&red), None, None]);
        let size = TILE_PIXELS as usize;
        assert_eq!(img.pixels[0], 0);
        assert_eq!(img.pixels[size - 1], 0xFF00_00FF);
        assert_eq!(img.pixels[(size - 1) * size + size - 1], 0);
    }
}
