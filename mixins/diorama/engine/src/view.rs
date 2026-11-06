//! Draws a rectangle of the map into an RGBA image on the CPU: baked tiles, then fog over
//! everything the map has no data for. Horizon gives native mods one image in the HUD atlas, so the
//! minimap and the fullscreen map are composed here and uploaded whole when they change.

use crate::projection::{PIXELS_PER_UNIT, TILE_PIXELS};
use crate::tile::{self, TileImage, TileKey};
use std::collections::HashMap;

/// Which part of the map an image shows, and how large.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    /// The map point at the image's middle.
    pub center_u: f64,
    pub center_v: f64,
    /// Image pixels per map unit (block).
    pub zoom: f64,
    pub layer: i32,
    /// The overview level of the tiles drawn.
    pub lod: u8,
    pub width: usize,
    pub height: usize,
    /// Cut the image to the largest circle that fits.
    pub round: bool,
}

impl View {
    pub fn min_u(&self) -> f64 {
        self.center_u - self.width as f64 / 2.0 / self.zoom
    }

    pub fn min_v(&self) -> f64 {
        self.center_v - self.height as f64 / 2.0 / self.zoom
    }

    /// Where map point (u, v) lands in the image.
    pub fn image_xy(&self, u: f64, v: f64) -> (f64, f64) {
        (
            self.width as f64 / 2.0 + (u - self.center_u) * self.zoom,
            self.height as f64 / 2.0 + (v - self.center_v) * self.zoom,
        )
    }

    /// The map point at image point (x, y).
    pub fn map_uv(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.center_u + (x - self.width as f64 / 2.0) / self.zoom,
            self.center_v + (y - self.height as f64 / 2.0) / self.zoom,
        )
    }

    /// The tiles the view covers.
    pub fn tiles(&self) -> Vec<TileKey> {
        let units = TileKey {
            layer: self.layer,
            u: 0,
            v: 0,
            lod: self.lod,
        }
        .units();
        let u0 = (self.min_u() / units).floor() as i32;
        let v0 = (self.min_v() / units).floor() as i32;
        let u1 = ((self.min_u() + self.width as f64 / self.zoom) / units).floor() as i32;
        let v1 = ((self.min_v() + self.height as f64 / self.zoom) / units).floor() as i32;
        let mut out = Vec::with_capacity(((u1 - u0 + 1) * (v1 - v0 + 1)).max(0) as usize);
        for v in v0..=v1 {
            for u in u0..=u1 {
                out.push(TileKey {
                    layer: self.layer,
                    u,
                    v,
                    lod: self.lod,
                });
            }
        }
        out
    }
}

/// Below this zoom (image pixels per block) views draw overview tiles; at and above it the live
/// tiles' own mipmaps look the same and never lag behind recent changes.
pub const DETAILED_ZOOM: f64 = 0.5;

/// The overview level to draw a view at: level 0 from [`DETAILED_ZOOM`] (GUI pixels per block)
/// up and for cave slices, which have no overviews; further out, one level finer than the
/// coarsest that keeps a tile pixel per image pixel at `image_zoom` (image pixels per block).
pub fn lod_for(gui_zoom: f64, image_zoom: f64, cave: bool) -> u8 {
    if cave || gui_zoom >= DETAILED_ZOOM {
        return 0;
    }
    let mut lod = 0u8;
    while lod < TileKey::MAX_LOD
        && f64::from(PIXELS_PER_UNIT) / f64::from(2u32 << lod) >= image_zoom
    {
        lod += 1;
    }
    if lod > 1 { lod - 1 } else { lod }
}

struct Cached {
    mips: Vec<Vec<u32>>,
    used: u64,
}

/// Tiles held for drawing, with their mip chains; the least recently drawn go first.
pub struct TileCache {
    tiles: HashMap<TileKey, Cached>,
    clock: u64,
    capacity: usize,
    /// Goes up whenever a tile is added or replaced.
    pub version: u64,
}

impl TileCache {
    pub fn new(capacity: usize) -> TileCache {
        TileCache {
            tiles: HashMap::new(),
            clock: 0,
            capacity,
            version: 0,
        }
    }

    pub fn insert(&mut self, image: &TileImage) {
        self.insert_pixels(image.key, &image.pixels);
    }

    pub fn insert_pixels(&mut self, key: TileKey, pixels: &[u32]) {
        self.clock += 1;
        self.version += 1;
        self.tiles.insert(
            key,
            Cached {
                mips: tile::mips(pixels, TILE_PIXELS as usize),
                used: self.clock,
            },
        );
        if self.tiles.len() > self.capacity {
            let mut by_age: Vec<(u64, TileKey)> =
                self.tiles.iter().map(|(k, c)| (c.used, *k)).collect();
            by_age.sort_unstable();
            for (_, k) in by_age.into_iter().take(self.tiles.len() - self.capacity) {
                self.tiles.remove(&k);
            }
        }
    }

    pub fn contains(&self, key: &TileKey) -> bool {
        self.tiles.contains_key(key)
    }

    /// A held tile's full-size pixels.
    pub fn pixels(&self, key: &TileKey) -> Option<&[u32]> {
        self.tiles.get(key).map(|c| c.mips[0].as_slice())
    }

    pub fn remove(&mut self, key: &TileKey) {
        if self.tiles.remove(key).is_some() {
            self.version += 1;
        }
    }

    pub fn remove_where(&mut self, drop: impl Fn(&TileKey) -> bool) {
        self.tiles.retain(|k, _| !drop(k));
        self.version += 1;
    }

    pub fn clear(&mut self) {
        self.tiles.clear();
        self.version += 1;
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Marks tiles as drawn now, so they're kept.
    pub fn touch(&mut self, keys: &[TileKey]) {
        self.clock += 1;
        for k in keys {
            if let Some(c) = self.tiles.get_mut(k) {
                c.used = self.clock;
            }
        }
    }
}

/// The fog drawn over unknown ground: drifting clouds, in map space so they move with the map.
#[derive(Clone, Copy, Debug)]
pub struct Fog {
    /// 0xRRGGBB of the clouds' light and shadow.
    pub light: u32,
    pub shade: u32,
    /// Seconds, for the drift.
    pub time: f64,
}

impl Default for Fog {
    fn default() -> Self {
        Fog {
            light: 0xE4E9EE,
            shade: 0xA9B3BF,
            time: 0.0,
        }
    }
}

impl Fog {
    pub const CAVE: Fog = Fog {
        light: 0x2A2B30,
        shade: 0x141519,
        time: 0.0,
    };

    /// ABGR fog colour at map point (u, v).
    fn at(&self, u: f64, v: f64) -> u32 {
        let drift = self.time * 0.6;
        let n = 0.6 * value_noise(u / 24.0 + drift / 24.0, v / 24.0)
            + 0.3 * value_noise(u / 9.0 - drift / 15.0, v / 9.0 + 3.7)
            + 0.1 * value_noise(u / 3.5, v / 3.5 + drift / 10.0);
        let t = (n as f32).clamp(0.0, 1.0);
        let mix = |shift: u32| {
            let a = ((self.shade >> shift) & 0xFF) as f32;
            let b = ((self.light >> shift) & 0xFF) as f32;
            (a + (b - a) * t).round() as u32
        };
        0xFF00_0000 | (mix(0) << 16) | (mix(8) << 8) | mix(16)
    }
}

fn hash2(x: i32, y: i32) -> f64 {
    let mut h = (x as u32).wrapping_mul(0x8DA6_B343) ^ (y as u32).wrapping_mul(0xD816_3841);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    f64::from(h & 0xFFFF) / 65535.0
}

/// Smooth noise 0..1 on a unit lattice.
fn value_noise(x: f64, y: f64) -> f64 {
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (ix, iy) = (x0 as i32, y0 as i32);
    let a = hash2(ix, iy);
    let b = hash2(ix + 1, iy);
    let c = hash2(ix, iy + 1);
    let d = hash2(ix + 1, iy + 1);
    let top = a + (b - a) * sx;
    let bottom = c + (d - c) * sx;
    top + (bottom - top) * sy
}

/// Blends ABGR `over` onto opaque `under` by `over`'s alpha.
#[inline]
fn over(top: u32, under: u32) -> u32 {
    let a = top >> 24;
    if a == 255 {
        return top;
    }
    if a == 0 {
        return under;
    }
    let mix = |shift: u32| {
        let t = (top >> shift) & 0xFF;
        let u = (under >> shift) & 0xFF;
        ((t * a + u * (255 - a) + 127) / 255) << shift
    };
    0xFF00_0000 | mix(16) | mix(8) | mix(0)
}

/// Draws `view` into `out` (ABGR, `view.width * view.height`). Tiles not in `cache` draw as fog;
/// the ones that were missing are returned, so they can be loaded or baked.
pub fn compose(view: &View, cache: &mut TileCache, fog: &Fog, out: &mut [u32]) -> Vec<TileKey> {
    let (w, h) = (view.width, view.height);
    assert!(out.len() >= w * h);
    let keys = view.tiles();
    cache.touch(&keys);
    let missing: Vec<TileKey> = keys
        .iter()
        .copied()
        .filter(|k| !cache.contains(k))
        .collect();
    let probe = TileKey {
        layer: view.layer,
        u: 0,
        v: 0,
        lod: view.lod,
    };
    let units = probe.units();
    // Tile pixels per image pixel at the tile's own level, and the mip level to read.
    let texels_per_pixel = f64::from(PIXELS_PER_UNIT) / (view.zoom * f64::from(1u32 << view.lod));
    let level = if texels_per_pixel > 1.0 {
        (texels_per_pixel.log2().floor() as usize).min(9)
    } else {
        0
    };
    let size = (TILE_PIXELS as usize) >> level;
    let texels_per_unit = size as f64 / units;
    let (min_u, min_v) = (view.min_u(), view.min_v());
    let (cx, cy, r2) = (
        w as f64 / 2.0,
        h as f64 / 2.0,
        (w.min(h) as f64 / 2.0).powi(2),
    );
    let tiles = &cache.tiles;
    for y in 0..h {
        let v = min_v + (y as f64 + 0.5) / view.zoom;
        let tv = (v / units).floor() as i32;
        let ty = (((v - f64::from(tv) * units) * texels_per_unit) as usize).min(size - 1);
        let row = &mut out[y * w..(y + 1) * w];
        let mut current: Option<(i32, Option<&[u32]>)> = None;
        for (x, px) in row.iter_mut().enumerate() {
            if view.round {
                let (dx, dy) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
                if dx * dx + dy * dy > r2 {
                    *px = 0;
                    continue;
                }
            }
            let u = min_u + (x as f64 + 0.5) / view.zoom;
            let tu = (u / units).floor() as i32;
            if current.as_ref().is_none_or(|(k, _)| *k != tu) {
                let key = TileKey {
                    layer: view.layer,
                    u: tu,
                    v: tv,
                    lod: view.lod,
                };
                let mip = tiles.get(&key).map(|c| {
                    let lvl = level.min(c.mips.len() - 1);
                    &c.mips[lvl][ty * size..(ty + 1) * size]
                });
                current = Some((tu, mip));
            }
            let tile_row = current.and_then(|(_, r)| r);
            let pixel = tile_row.map_or(0, |r| {
                let tx = (((u - f64::from(tu) * units) * texels_per_unit) as usize).min(size - 1);
                r[tx]
            });
            *px = if pixel >> 24 == 255 {
                pixel
            } else {
                over(pixel, fog.at(u, v))
            };
        }
    }
    missing
}

/// ABGR pixels as RGBA bytes.
pub fn to_rgba(pixels: &[u32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(pixels.len() * 4);
    for p in pixels {
        out.extend_from_slice(&p.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::PIXELS;

    fn view(zoom: f64) -> View {
        View {
            center_u: 64.0,
            center_v: 64.0,
            zoom,
            layer: TileKey::SURFACE,
            lod: lod_for(zoom, zoom, false),
            width: 64,
            height: 48,
            round: false,
        }
    }

    #[test]
    fn coarse_zoom_uses_overviews() {
        assert_eq!(lod_for(4.0, 4.0, false), 0);
        assert_eq!(lod_for(0.5, 0.5, false), 0);
        assert_eq!(lod_for(0.4, 0.4, false), 2);
        assert_eq!(lod_for(0.04, 0.04, false), 4);
        assert_eq!(lod_for(0.04, 0.04, true), 0);
        // A denser screen keeps finer tiles at the same GUI zoom.
        assert_eq!(lod_for(0.25, 0.75, false), 1);
    }

    #[test]
    fn draws_tiles_and_fogs_the_rest() {
        let mut cache = TileCache::new(8);
        let key = TileKey::new(TileKey::SURFACE, 0, 0);
        let mut img = TileImage::blank(key);
        img.pixels = vec![0xFF00_FF00; PIXELS];
        cache.insert(&img);
        let v = View {
            center_u: 128.0,
            ..view(2.0)
        };
        let mut out = vec![0u32; v.width * v.height];
        let missing = compose(&v, &mut cache, &Fog::default(), &mut out);
        // Left half is tile (0, 0), right half tile (1, 0), which is missing.
        assert_eq!(out[10 * v.width + 5], 0xFF00_FF00);
        assert_ne!(out[10 * v.width + 60], 0xFF00_FF00);
        assert_eq!(out[10 * v.width + 60] >> 24, 255, "fog is opaque");
        assert!(missing.contains(&TileKey::new(TileKey::SURFACE, 1, 0)));
        assert!(!missing.contains(&key));
    }

    #[test]
    fn round_views_are_clear_outside_the_circle() {
        let mut cache = TileCache::new(8);
        let v = View {
            round: true,
            width: 32,
            height: 32,
            ..view(2.0)
        };
        let mut out = vec![1u32; 32 * 32];
        compose(&v, &mut cache, &Fog::default(), &mut out);
        assert_eq!(out[0], 0);
        assert_ne!(out[16 * 32 + 16], 0);
    }

    #[test]
    fn image_and_map_points_invert() {
        let v = view(3.0);
        let (x, y) = v.image_xy(70.0, 50.0);
        let (u, w) = v.map_uv(x, y);
        assert!((u - 70.0).abs() < 1e-9 && (w - 50.0).abs() < 1e-9);
    }
}
