//! Tiles of the map and their pixels.
//!
//! Pixels are `u32` ABGR, which is RGBA in memory on little-endian machines, alpha meaning how much
//! of the pixel's samples hit known ground: 0 is nothing known (fog), 255 fully known.

use crate::projection::{COVERAGE_CELLS, TILE_PIXELS, TILE_UNITS};

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

/// A tile's pixels, `TILE_PIXELS` square.
#[derive(Clone, Debug)]
pub struct TileImage {
    pub key: TileKey,
    pub pixels: Vec<u32>,
}

impl TileImage {
    pub fn blank(key: TileKey) -> TileImage {
        TileImage {
            key,
            pixels: vec![0; PIXELS],
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

/// An overview tile from its four children, each shrunk to a quarter; children missing count as unknown.
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
