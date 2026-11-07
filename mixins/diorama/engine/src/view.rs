//! Draws a rectangle of the map into an RGBA image on the CPU: baked tiles over a backdrop, then
//! fog over everything the map has no data for. Horizon gives native mods one image in the HUD atlas, so the
//! minimap and the fullscreen map are composed here and uploaded whole when they change.

use crate::fog::FogLayer;
use crate::projection::{COVERAGE_CELLS, PIXELS_PER_UNIT, TILE_PIXELS};
use crate::tile::{self, TileImage, TileKey};
use std::collections::HashMap;
use std::sync::Arc;

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
    /// The tile's mip chain with each colour premultiplied by its alpha, as [`compose`] sums it.
    mips: Vec<Vec<u32>>,
    used: u64,
}

/// ABGR with its colour multiplied by its alpha.
fn premultiply(p: u32) -> u32 {
    let a = p >> 24;
    if a == 255 {
        return p;
    }
    let ch = |shift: u32| (((p >> shift) & 0xFF) * a + 127) / 255;
    (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Premultiplied ABGR back to plain colour.
fn unpremultiply(p: u32) -> u32 {
    let a = p >> 24;
    if a == 255 || a == 0 {
        return p;
    }
    let ch = |shift: u32| ((((p >> shift) & 0xFF) * 255 + a / 2) / a).min(255);
    (a << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Coverage kept for the fog, per tile: many more than the tiles held, at 4 KiB each.
const COVERAGE_TILES: usize = 2048;

/// The mip level whose alpha is a tile's coverage: [`COVERAGE_CELLS`] a side.
const COVERAGE_LEVEL: usize = (TILE_PIXELS / COVERAGE_CELLS).trailing_zeros() as usize;

/// Tiles held for drawing, with their mip chains; the least recently drawn go first. Besides, the
/// coverage of many more tiles, which the fog reads (MapTile.coverage).
pub struct TileCache {
    tiles: HashMap<TileKey, Cached>,
    /// Each tile's known share per coverage cell, 0 to 255, and when it came.
    coverage: HashMap<TileKey, (Arc<[u8]>, u64)>,
    clock: u64,
    capacity: usize,
    /// Goes up whenever a tile or its coverage is added, replaced or dropped.
    pub version: u64,
}

impl TileCache {
    pub fn new(capacity: usize) -> TileCache {
        TileCache {
            tiles: HashMap::new(),
            coverage: HashMap::new(),
            clock: 0,
            capacity,
            version: 0,
        }
    }

    /// The known share of each of a tile's coverage cells, from its pixels.
    pub fn coverage_of(pixels: &[u32]) -> Arc<[u8]> {
        let mut level = pixels.to_vec();
        let mut size = TILE_PIXELS as usize;
        while size > COVERAGE_CELLS as usize {
            level = tile::shrink(&level, size);
            size /= 2;
        }
        level.iter().map(|p| (p >> 24) as u8).collect()
    }

    /// Keeps a tile's coverage for the fog without its pixels: for tiles round the view, which
    /// the clouds' climb reads.
    pub fn insert_coverage(&mut self, key: TileKey, coverage: Arc<[u8]>) {
        self.clock += 1;
        self.version += 1;
        self.coverage.insert(key, (coverage, self.clock));
        if self.coverage.len() > COVERAGE_TILES {
            let mut by_age: Vec<(u64, TileKey)> = self
                .coverage
                .iter()
                .filter(|(k, _)| !self.tiles.contains_key(k))
                .map(|(k, c)| (c.1, *k))
                .collect();
            by_age.sort_unstable();
            let extra = self.coverage.len() - COVERAGE_TILES;
            for (_, k) in by_age.into_iter().take(extra) {
                self.coverage.remove(&k);
            }
        }
    }

    pub fn has_coverage(&self, key: &TileKey) -> bool {
        self.coverage.contains_key(key)
    }

    /// The coverage held of tiles of `layer` that meet the map rectangle from (u0, v0) to
    /// (u1, v1), at any level.
    pub fn coverage_in(
        &self,
        layer: i32,
        u0: f64,
        v0: f64,
        u1: f64,
        v1: f64,
    ) -> Vec<(TileKey, Arc<[u8]>)> {
        self.coverage
            .iter()
            .filter(|(k, _)| {
                k.layer == layer
                    && k.min_u() < u1
                    && k.min_u() + k.units() > u0
                    && k.min_v() < v1
                    && k.min_v() + k.units() > v0
            })
            .map(|(k, c)| (*k, c.0.clone()))
            .collect()
    }

    pub fn insert(&mut self, image: &TileImage) {
        self.insert_pixels(image.key, &image.pixels);
    }

    pub fn insert_pixels(&mut self, key: TileKey, pixels: &[u32]) {
        self.clock += 1;
        let mips: Vec<Vec<u32>> = tile::mips(pixels, TILE_PIXELS as usize)
            .into_iter()
            .map(|mut level| {
                level.iter_mut().for_each(|p| *p = premultiply(*p));
                level
            })
            .collect();
        let coverage: Arc<[u8]> = mips[COVERAGE_LEVEL]
            .iter()
            .map(|p| (p >> 24) as u8)
            .collect();
        self.tiles.insert(
            key,
            Cached {
                mips,
                used: self.clock,
            },
        );
        self.insert_coverage(key, coverage);
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
    pub fn pixels(&self, key: &TileKey) -> Option<Vec<u32>> {
        self.tiles
            .get(key)
            .map(|c| c.mips[0].iter().map(|p| unpremultiply(*p)).collect())
    }

    /// Drops a held tile. Its coverage stays for the fog until a new one replaces it.
    pub fn remove(&mut self, key: &TileKey) {
        if self.tiles.remove(key).is_some() {
            self.version += 1;
        }
    }

    /// Drops the held tiles `drop` picks; their coverage stays, as [`Self::remove`].
    pub fn remove_where(&mut self, drop: impl Fn(&TileKey) -> bool) {
        self.tiles.retain(|k, _| !drop(k));
        self.version += 1;
    }

    pub fn clear(&mut self) {
        self.tiles.clear();
        self.coverage.clear();
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

/// What the map is drawn on and how: the colour behind the tiles, the night, and how opaque.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// 0xRRGGBB behind the tiles, so the ragged edge of known ground blends into it.
    pub backdrop: u32,
    /// How far into the night, 0 to 1: the backdrop darkens with it (NightLook.darken).
    pub night: f32,
    /// How opaque the tiles and fog draw, 0 to 1. Below 1 there is no backdrop: drawn under
    /// see-through tiles it would show through them and tint the game behind
    /// (MapViewport.java:168-171).
    pub opacity: f32,
}

impl Look {
    /// Behind the open sky's tiles: the clouds' colour (MapViewport.java:162-165).
    pub const SURFACE: u32 = 0x8C99AB;
    /// Behind the tiles of a dimension drawn from under its roof: the smoke's colour.
    pub const ROOFED: u32 = 0x29211F;
    /// Behind cave slices: the dark of caves.
    pub const CAVES: u32 = 0x0C0E12;

    /// The look of a cave slice or the sky view of a dimension, with or without a roof; by day
    /// and opaque.
    pub fn new(cave: bool, roofed: bool) -> Look {
        Look {
            backdrop: if cave {
                Look::CAVES
            } else if roofed {
                Look::ROOFED
            } else {
                Look::SURFACE
            },
            night: 0.0,
            opacity: 1.0,
        }
    }

    /// The backdrop as ABGR channels [r, g, b], darkened for the night as the tiles are under the
    /// moon (NightLook.java:111-116).
    fn backdrop_rgb(&self) -> [u64; 3] {
        const MOON: [f32; 3] = [0.27, 0.33, 0.49];
        let night = self.night.clamp(0.0, 1.0);
        std::array::from_fn(|i| {
            let c = ((self.backdrop >> (16 - 8 * i)) & 0xFF) as f32;
            (c * (1.0 + (MOON[i] - 1.0) * night)).round() as u64
        })
    }
}

/// Fixed-point weight of a whole image pixel along one axis.
const AXIS_ONE: u32 = 256;
/// Fixed-point weight of a whole image pixel: one axis times the other.
const PIXEL_ONE: u64 = (AXIS_ONE * AXIS_ONE) as u64;
/// Image pixels per tile texel from which tiles draw crisp: magnified by less, crisp texels
/// would come out uneven, one pixel wide here and two there (MapViewport.java:113, 226-235).
const CRISP_FROM: f64 = 1.5;
/// The finest mip level read: a tile has levels from 512 texels down to 1.
const LAST_LEVEL: u32 = 9;
/// A row of nothing, read where a tile is missing.
static UNKNOWN_ROW: [u32; TILE_PIXELS as usize] = [0; TILE_PIXELS as usize];

/// One texel read along an axis: which tile (counted from the first the view reads), which
/// texel of it, and how much of the image pixel lies on it, in [`AXIS_ONE`]ths.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tap {
    tile: u32,
    texel: u32,
    weight: u32,
}

/// The reads of each image column, or each row: `ranges[i]` are pixel `i`'s taps.
#[derive(Debug)]
struct Taps {
    /// The tile index of `Tap::tile` 0.
    first_tile: i64,
    tiles: usize,
    taps: Vec<Tap>,
    ranges: Vec<(u32, u32)>,
}

impl Taps {
    /// The reads for `n` image pixels along one axis. `origin` is where the first pixel's
    /// leading edge falls and `step` how wide a pixel is, both in texels of the level read;
    /// `size` texels make a tile. Each pixel averages the texels under it, weighed by how much
    /// of the pixel lies on each, as tile.fsh `reads` does; crisp pixels read the one texel
    /// under their middle.
    fn new(n: usize, origin: f64, step: f64, size: usize, crisp: bool) -> Taps {
        let size = size as i64;
        let mut global: Vec<(i64, u32)> = Vec::with_capacity(n * 5);
        let mut ranges = Vec::with_capacity(n);
        let mut weights = [0f64; 8];
        for i in 0..n {
            let at = origin + (i as f64 + 0.5) * step;
            let start = global.len() as u32;
            if crisp {
                global.push((at.floor() as i64, AXIS_ONE));
            } else {
                let (lo, hi) = (at - step * 0.5, at + step * 0.5);
                let first = lo.floor();
                let mut count = 0;
                while count < weights.len() && first + (count as f64) < hi {
                    let t = first + count as f64;
                    weights[count] = ((t + 1.0).min(hi) - t.max(lo)).max(0.0) / step;
                    count += 1;
                }
                // Whole fixed-point weights adding up to one pixel: the rounding goes to the
                // texel the pixel lies on most.
                let mut fixed = [0u32; 8];
                let mut sum = 0i64;
                let mut most = 0;
                for k in 0..count {
                    fixed[k] = (weights[k] * f64::from(AXIS_ONE)).round() as u32;
                    sum += i64::from(fixed[k]);
                    if weights[k] > weights[most] {
                        most = k;
                    }
                }
                fixed[most] = (i64::from(fixed[most]) + i64::from(AXIS_ONE) - sum).max(0) as u32;
                for (k, &w) in fixed.iter().enumerate().take(count) {
                    if w > 0 {
                        global.push((first as i64 + k as i64, w));
                    }
                }
            }
            ranges.push((start, global.len() as u32));
        }
        let first_tile = global
            .iter()
            .map(|g| g.0.div_euclid(size))
            .min()
            .unwrap_or(0);
        let last_tile = global
            .iter()
            .map(|g| g.0.div_euclid(size))
            .max()
            .unwrap_or(0);
        let taps = global
            .into_iter()
            .map(|(g, weight)| Tap {
                tile: (g.div_euclid(size) - first_tile) as u32,
                texel: g.rem_euclid(size) as u32,
                weight,
            })
            .collect();
        Taps {
            first_tile,
            tiles: (last_tile - first_tile + 1) as usize,
            taps,
            ranges,
        }
    }

    fn of(&self, i: usize) -> &[Tap] {
        let (a, b) = self.ranges[i];
        &self.taps[a as usize..b as usize]
    }
}

/// How a view reads its tiles: the mip level, and each column's and row's taps.
struct Sampling {
    level: usize,
    size: usize,
    columns: Taps,
    rows: Taps,
}

impl Sampling {
    /// Shrunk tiles average the area under each image pixel from a mip level two to four
    /// texels finer than the pixel, so the average falls where the pixel does and doesn't
    /// sparkle while panning (tile.fsh). Magnified by [`CRISP_FROM`] or more they are crisp;
    /// magnified by less, each pixel still averages the texels under it, which leaves their
    /// edges a pixel soft (MapViewport.tileSampling: TILE_SMOOTH draws as TILE_AREA).
    fn new(view: &View) -> Sampling {
        let units = TileKey {
            layer: view.layer,
            u: 0,
            v: 0,
            lod: view.lod,
        }
        .units();
        // Texels of the tile's own level per image pixel.
        let t = f64::from(PIXELS_PER_UNIT) / (view.zoom * f64::from(1u32 << view.lod));
        let crisp = t * CRISP_FROM <= 1.0;
        let level = if crisp {
            0
        } else {
            (t.max(1.0).log2().floor() - 1.0).clamp(0.0, f64::from(LAST_LEVEL)) as usize
        };
        let size = (TILE_PIXELS as usize) >> level;
        let per_unit = size as f64 / units;
        let step = per_unit / view.zoom;
        Sampling {
            level,
            size,
            columns: Taps::new(view.width, view.min_u() * per_unit, step, size, crisp),
            rows: Taps::new(view.height, view.min_v() * per_unit, step, size, crisp),
        }
    }

    /// Every tile some pixel reads.
    fn keys(&self, view: &View) -> Vec<TileKey> {
        let mut out = Vec::with_capacity(self.columns.tiles * self.rows.tiles);
        for j in 0..self.rows.tiles as i64 {
            for i in 0..self.columns.tiles as i64 {
                out.push(TileKey {
                    layer: view.layer,
                    u: (self.columns.first_tile + i) as i32,
                    v: (self.rows.first_tile + j) as i32,
                    lod: view.lod,
                });
            }
        }
        out
    }
}

/// Images at least this many pixels are composed on several threads.
const THREADED_FROM: usize = 1 << 17;
/// The most threads one image is composed on.
const MAX_THREADS: usize = 6;

/// Threads to compose a large image on: half the machine's, leaving the rest to the game and
/// the bakers, up to [`MAX_THREADS`].
fn compose_threads() -> usize {
    static THREADS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THREADS.get_or_init(|| {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get() / 2)
            .clamp(1, MAX_THREADS)
    })
}

/// Draws `view` into `out` (ABGR, `view.width * view.height`): the tiles in `cache` over the
/// backdrop of `look`, and the `fog` layer over that, as fog.fsh's colour blends over the map:
/// `clamp(rgb / a) * a + ground * (1 - a)`. Where the layer doesn't reach, its flat fog colour
/// covers the unknown share. Tiles the view reads that are not in `cache` are returned, so they
/// can be loaded or baked.
pub fn compose(
    view: &View,
    cache: &mut TileCache,
    look: &Look,
    fog: Option<&FogLayer>,
    out: &mut [u32],
) -> Vec<TileKey> {
    let (w, h) = (view.width, view.height);
    assert!(out.len() >= w * h);
    let sampling = Sampling::new(view);
    let keys = sampling.keys(view);
    cache.touch(&keys);
    let missing: Vec<TileKey> = keys
        .iter()
        .copied()
        .filter(|k| !cache.contains(k))
        .collect();
    let images: Vec<Option<&[u32]>> = keys
        .iter()
        .map(|k| {
            cache
                .tiles
                .get(k)
                .and_then(|c| c.mips.get(sampling.level))
                .map(Vec::as_slice)
        })
        .collect();
    let fog = fog.map(|layer| FogRead::new(layer, view));
    let painter = Painter {
        view,
        sampling: &sampling,
        images: &images,
        fog: fog.as_ref(),
        opacity: look.opacity.clamp(0.0, 1.0),
        backdrop: look.backdrop_rgb(),
    };
    let out = &mut out[..w * h];
    let threads = if w * h >= THREADED_FROM {
        compose_threads()
    } else {
        1
    };
    if threads <= 1 || h < 2 * threads {
        painter.rows(0, out);
    } else {
        let band = h.div_ceil(threads);
        std::thread::scope(|scope| {
            let mut bands = out.chunks_mut(band * w).enumerate();
            let first = bands.next();
            for (i, rows) in bands {
                let painter = &painter;
                scope.spawn(move || painter.rows(i * band, rows));
            }
            if let Some((_, rows)) = first {
                painter.rows(0, rows);
            }
        });
    }
    missing
}

/// How a view reads a fog layer: for each column and row, the layer texel before the pixel's
/// middle and how far past it, or none outside the layer.
struct FogRead<'a> {
    layer: &'a FogLayer,
    columns: Vec<Option<(usize, f32)>>,
    rows: Vec<Option<(usize, f32)>>,
    /// The flat fog colour, 0 to 1.
    flat: [f32; 3],
}

impl FogRead<'_> {
    fn new<'a>(layer: &'a FogLayer, view: &View) -> FogRead<'a> {
        let g = &layer.grid;
        let axis = |n: usize, min: f64, start: f64, texels: usize| -> Vec<Option<(usize, f32)>> {
            (0..n)
                .map(|i| {
                    let f = (min + (i as f64 + 0.5) / view.zoom - start) / g.texel - 0.5;
                    let last = texels as f64 - 1.0;
                    (texels > 0 && f >= 0.0 && f <= last).then(|| {
                        let i0 = f.floor().min(last - 1.0).max(0.0);
                        (i0 as usize, (f - i0) as f32)
                    })
                })
                .collect()
        };
        FogRead {
            layer,
            columns: axis(view.width, view.min_u(), g.start_u, g.w),
            rows: axis(view.height, view.min_v(), g.start_v, g.h),
            flat: std::array::from_fn(|c| ((layer.flat >> (8 * c)) & 0xFF) as f32 / 255.0),
        }
    }

    /// The flat colour over the `unknown` share (0 to 1), premultiplied, 0 to 255: the fog
    /// where the layer doesn't reach.
    #[inline]
    fn flat(&self, unknown: f32) -> [f32; 4] {
        let a = unknown * 255.0;
        [self.flat[0] * a, self.flat[1] * a, self.flat[2] * a, a]
    }

    /// The layer over image column `x` of a row, filtered between its texels, premultiplied, 0
    /// to 255; none outside it.
    #[inline]
    fn at(&self, x: usize, row: Option<(usize, f32)>) -> Option<[f32; 4]> {
        let (Some((i, tx)), Some((j, ty))) = (self.columns[x], row) else {
            return None;
        };
        let g = &self.layer.grid;
        let p = &self.layer.pixels;
        let i1 = (i + 1).min(g.w - 1);
        let j1 = (j + 1).min(g.h - 1);
        let (a, b, c, d) = (
            p[j * g.w + i],
            p[j * g.w + i1],
            p[j1 * g.w + i],
            p[j1 * g.w + i1],
        );
        if a | b | c | d == 0 {
            return Some([0.0; 4]);
        }
        let (wa, wb) = ((1.0 - tx) * (1.0 - ty), tx * (1.0 - ty));
        let (wc, wd) = ((1.0 - tx) * ty, tx * ty);
        Some(std::array::from_fn(|ch| {
            let s = 8 * ch;
            let t = |p: u32| ((p >> s) & 0xFF) as f32;
            t(a) * wa + t(b) * wb + t(c) * wc + t(d) * wd
        }))
    }
}

/// What every row of one composed image needs.
struct Painter<'a> {
    view: &'a View,
    sampling: &'a Sampling,
    /// Each tile's level as read, by row of tiles then column; none where it is missing.
    images: &'a [Option<&'a [u32]>],
    fog: Option<&'a FogRead<'a>>,
    opacity: f32,
    backdrop: [u64; 3],
}

impl Painter<'_> {
    /// Paints image rows from `y0` on into `out`, whole rows of the view's width.
    fn rows(&self, y0: usize, out: &mut [u32]) {
        let view = self.view;
        let w = view.width;
        let (columns, rows) = (&self.sampling.columns, &self.sampling.rows);
        let size = self.sampling.size;
        let unknown = &UNKNOWN_ROW[..size];
        let opaque = self.opacity >= 1.0;
        let full = 255 * PIXEL_ONE;
        let [br, bg, bb] = self.backdrop;
        let ground = 0xFF00_0000 | ((bb as u32) << 16) | ((bg as u32) << 8) | br as u32;
        let (cx, cy, r2) = (
            w as f64 / 2.0,
            view.height as f64 / 2.0,
            (w.min(view.height) as f64 / 2.0).powi(2),
        );
        // The texel rows each of a row's taps reads, for every column of tiles.
        let mut lines: Vec<&[u32]> = Vec::with_capacity(columns.tiles * 8);
        for (dy, row) in out.chunks_exact_mut(w).enumerate() {
            let y = y0 + dy;
            let row_taps = rows.of(y);
            let fog_row = self.fog.and_then(|f| f.rows[y]);
            lines.clear();
            for tap in row_taps {
                let at = tap.texel as usize * size;
                for i in 0..columns.tiles {
                    lines.push(
                        self.images[tap.tile as usize * columns.tiles + i]
                            .map_or(unknown, |m| &m[at..at + size]),
                    );
                }
            }
            // Each row tap's weight with its rows of texels.
            let reads: Vec<(u32, &[&[u32]])> = row_taps
                .iter()
                .enumerate()
                .map(|(j, t)| (t.weight, &lines[j * columns.tiles..(j + 1) * columns.tiles]))
                .collect();
            for (x, px) in row.iter_mut().enumerate() {
                if view.round {
                    let (dx, dy) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
                    if dx * dx + dy * dy > r2 {
                        *px = 0;
                        continue;
                    }
                }
                let layer = self.fog.and_then(|f| f.at(x, fog_row));
                if opaque && let Some(f) = layer.filter(|f| f[3] >= 254.5) {
                    // Under fog all over: the fog alone.
                    let ch = |c: f32| (c.min(255.0) + 0.5) as u32;
                    *px = 0xFF00_0000 | (ch(f[2]) << 16) | (ch(f[1]) << 8) | ch(f[0]);
                    continue;
                }
                let column_taps = columns.of(x);
                // Sums of premultiplied channels, each up to 255 * PIXEL_ONE. Along a row two
                // channels share a word: a column's weights add up to AXIS_ONE, so each half
                // stays below 255 * 256 and never carries into the other.
                let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
                for &(weight, line) in &reads {
                    let (mut rb, mut ga) = (0u32, 0u32);
                    for ct in column_taps {
                        let p = line[ct.tile as usize][ct.texel as usize];
                        rb += (p & 0x00FF_00FF) * ct.weight;
                        ga += ((p >> 8) & 0x00FF_00FF) * ct.weight;
                    }
                    r += (rb & 0xFFFF) * weight;
                    b += (rb >> 16) * weight;
                    g += (ga & 0xFFFF) * weight;
                    a += (ga >> 16) * weight;
                }
                if let Some(fog) = self.fog {
                    let f = layer
                        .unwrap_or_else(|| fog.flat(1.0 - (f64::from(a) / full as f64) as f32));
                    if f[3] > 0.5 {
                        *px = self.fogged([r, g, b, a], f);
                        continue;
                    }
                }
                *px = if a == 0 {
                    // Nothing known here: the backdrop.
                    if opaque { ground } else { 0 }
                } else if a == PIXEL_ONE as u32 * 255 {
                    // Known all over: the colour as it is.
                    let ch = |c: u32| (c + (PIXEL_ONE as u32 >> 1)) >> 16;
                    let alpha = if opaque {
                        255
                    } else {
                        (255.0 * self.opacity).round() as u32
                    };
                    (alpha << 24) | (ch(b) << 16) | (ch(g) << 8) | ch(r)
                } else if opaque {
                    // The known share's colour, and the backdrop under the rest.
                    let rest = full - u64::from(a);
                    let ch = |c: u32, bd: u64| {
                        ((u64::from(c) * 255 + bd * rest + full / 2) / full).min(255) as u32
                    };
                    0xFF00_0000 | (ch(b, bb) << 16) | (ch(g, bg) << 8) | ch(r, br)
                } else {
                    let ch = |c: u32| {
                        ((u64::from(c) * 255 + u64::from(a / 2)) / u64::from(a)).min(255) as u32
                    };
                    let alpha = (f64::from(a) / PIXEL_ONE as f64 * f64::from(self.opacity)).round();
                    ((alpha as u32) << 24) | (ch(b) << 16) | (ch(g) << 8) | ch(r)
                };
            }
        }
    }

    /// The ground's premultiplied sums `[r, g, b, a]` with `fog` (premultiplied, 0 to 255) over
    /// them: the tiles over the backdrop when opaque, else both as see-through as the look says.
    #[inline]
    fn fogged(&self, sums: [u32; 4], fog: [f32; 4]) -> u32 {
        let per = 1.0 / PIXEL_ONE as f32;
        let fa = fog[3].min(255.0);
        let byte = |c: f32| (c.clamp(0.0, 255.0) + 0.5) as u32;
        if self.opacity >= 1.0 {
            let rest = 1.0 - fa / 255.0;
            let unknown = 1.0 - sums[3] as f32 * per / 255.0;
            let ch = |c: usize| {
                let ground = sums[c] as f32 * per + self.backdrop[c] as f32 * unknown;
                byte(fog[c].min(fa) + ground * rest)
            };
            return 0xFF00_0000 | (ch(2) << 16) | (ch(1) << 8) | ch(0);
        }
        let op = self.opacity;
        let rest = 1.0 - fa / 255.0 * op;
        let out: [f32; 4] =
            std::array::from_fn(|c| fog[c].min(fa) * op + sums[c] as f32 * per * op * rest);
        if out[3] <= 0.0 {
            return 0;
        }
        let scale = 255.0 / out[3];
        let ch = |c: usize| byte(out[c] * scale);
        (byte(out[3]) << 24) | (ch(2) << 16) | (ch(1) << 8) | ch(0)
    }
}

/// The chunk grid over a view, as the ground at height `y` shows it: the image x of each line
/// between chunk columns and the image y of each line between chunk rows. Chunk edges fall on
/// multiples of 16 in turned space whichever way the map faces.
pub fn chunk_grid(
    view: &View,
    projection: &crate::projection::Projection,
    y: f64,
) -> (Vec<f64>, Vec<f64>) {
    let (w, h) = (view.width as f64, view.height as f64);
    let (u0, v0) = view.map_uv(0.0, 0.0);
    let (u1, v1) = view.map_uv(w, h);
    let columns = ((u0 / 16.0).ceil() as i64..=(u1 / 16.0).floor() as i64)
        .map(|k| view.image_xy(k as f64 * 16.0, 0.0).0)
        .collect();
    // Turned z where the view's top and bottom rows meet the ground at `y`.
    let (z0, z1) = (projection.z_at(v0, y), projection.z_at(v1, y));
    let rows = ((z0 / 16.0).ceil() as i64..=(z1 / 16.0).floor() as i64)
        .map(|k| {
            view.image_xy(0.0, projection.v_turned(y, k as f64 * 16.0))
                .1
        })
        .collect();
    (columns, rows)
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

    const LOOK: Look = Look {
        backdrop: Look::SURFACE,
        night: 0.0,
        opacity: 1.0,
    };

    /// Fog with no layer painted: the flat colour over whatever is unknown.
    fn flat() -> FogLayer {
        FogLayer::flat(crate::fog::FogKind::Clouds, TileKey::SURFACE, 0.0)
    }

    /// A cache holding tiles (u, v) for u in `us` and v in `vs`, each painted by `paint(x, y)`
    /// over its own texels.
    fn filled(
        us: std::ops::Range<i32>,
        vs: std::ops::Range<i32>,
        paint: impl Fn(usize, usize) -> u32,
    ) -> TileCache {
        let mut cache = TileCache::new(256);
        let size = TILE_PIXELS as usize;
        let pixels: Vec<u32> = (0..PIXELS).map(|i| paint(i % size, i / size)).collect();
        for v in vs {
            for u in us.clone() {
                cache.insert_pixels(TileKey::new(TileKey::SURFACE, u, v), &pixels);
            }
        }
        cache
    }

    fn channel(p: u32, shift: u32) -> i32 {
        ((p >> shift) & 0xFF) as i32
    }

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
        let missing = compose(&v, &mut cache, &LOOK, Some(&flat()), &mut out);
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
        compose(&v, &mut cache, &LOOK, Some(&flat()), &mut out);
        assert_eq!(out[0], 0);
        assert_ne!(out[16 * 32 + 16], 0);
    }

    #[test]
    fn chunk_lines_fall_on_chunk_edges() {
        use crate::projection::{Facing, Projection};
        let p = Projection::new(50.0, Facing::North);
        let v = View {
            center_u: 0.0,
            center_v: p.v(0.0, 64.0, 0.0),
            zoom: 1.0,
            layer: TileKey::SURFACE,
            lod: 0,
            width: 64,
            height: 64,
            round: false,
        };
        let (columns, rows) = chunk_grid(&v, &p, 64.0);
        assert_eq!(columns, vec![0.0, 16.0, 32.0, 48.0, 64.0]);
        // Rows are 16 * sin apart on the ground, and one is at the centre (z = 0).
        assert!(rows.iter().any(|r| (r - 32.0).abs() < 1e-9));
        assert!(
            rows.windows(2)
                .all(|w| (w[1] - w[0] - 16.0 * p.sin).abs() < 1e-9)
        );
    }

    #[test]
    fn flat_tiles_look_the_same_at_any_zoom() {
        let colour = 0xFF30_80C0;
        let mut cache = filled(-2..3, -2..3, |_, _| colour);
        for zoom in [16.0, 5.0, 4.0, 3.0, 1.6, 1.0, 0.7, 0.5, 0.3] {
            let v = View {
                center_u: 1.3,
                center_v: -7.9,
                lod: 0,
                ..view(zoom)
            };
            let mut out = vec![0u32; v.width * v.height];
            compose(&v, &mut cache, &LOOK, None, &mut out);
            assert!(out.iter().all(|&p| p == colour), "zoom {zoom}");
        }
    }

    #[test]
    fn shrunk_tiles_do_not_sparkle_while_panning() {
        // One-texel checkerboard, 2.5 texels to an image pixel.
        let mut cache = filled(-2..3, -2..3, |x, y| {
            if (x + y) % 2 == 0 {
                0xFFFF_FFFF
            } else {
                0xFF00_0000
            }
        });
        let zoom = f64::from(PIXELS_PER_UNIT) / 2.5;
        let base = View {
            center_u: 3.0,
            center_v: 5.0,
            lod: 0,
            ..view(zoom)
        };
        let moved = View {
            center_u: base.center_u + 0.1 / zoom,
            center_v: base.center_v + 0.1 / zoom,
            ..base
        };
        let mut a = vec![0u32; base.width * base.height];
        let mut b = a.clone();
        compose(&base, &mut cache, &LOOK, None, &mut a);
        compose(&moved, &mut cache, &LOOK, None, &mut b);
        let mean = |img: &[u32]| {
            img.iter().map(|p| f64::from(channel(*p, 0))).sum::<f64>() / img.len() as f64
        };
        assert!(
            (mean(&a) - mean(&b)).abs() < 2.0,
            "{} {}",
            mean(&a),
            mean(&b)
        );
        // Each pixel changes a little: a box this wide leaves a fifth of a texel's contrast
        // each way, where one tap would flip pixels from black to white.
        let diff: i64 = a
            .iter()
            .zip(&b)
            .map(|(p, q)| i64::from((channel(*p, 0) - channel(*q, 0)).abs()))
            .sum();
        let change = diff as f64 / a.len() as f64;
        assert!(change < 6.0, "mean change {change} of 255");
        // And the board averages to grey.
        assert!(a.iter().all(|&p| (channel(p, 0) - 128).abs() < 16));
    }

    #[test]
    fn half_known_pixels_blend_half_with_the_backdrop() {
        let blue = 0xFFFF_0000;
        let expect = |shift: u32| {
            let bd = ((Look::SURFACE >> (16 - shift)) & 0xFF) as i32;
            (channel(blue, shift) + bd + 1) / 2
        };
        // Pixels on a tile known at half strength.
        let mut cache = filled(-1..2, -1..2, |_, _| 0x80FF_0000);
        let v = View {
            lod: 0,
            ..view(8.0)
        };
        let mut out = vec![0u32; v.width * v.height];
        compose(&v, &mut cache, &LOOK, None, &mut out);
        // Pixels lying on two known texels and two unknown, two texels to a pixel each way.
        let mut cache = filled(-1..2, -1..2, |x, _| if x % 2 == 0 { blue } else { 0 });
        let v = View {
            lod: 0,
            ..view(2.0)
        };
        let mut out2 = vec![0u32; v.width * v.height];
        compose(&v, &mut cache, &LOOK, None, &mut out2);
        for p in [out[100], out2[100], out2[777]] {
            assert_eq!(p >> 24, 255);
            for shift in [0, 8, 16] {
                assert!((channel(p, shift) - expect(shift)).abs() <= 1, "{p:08x}");
            }
        }
    }

    #[test]
    fn see_through_maps_scale_alpha_and_have_no_backdrop() {
        let look = Look {
            opacity: 0.6,
            ..LOOK
        };
        let mut cache = filled(0..1, 0..1, |_, _| 0xFF12_3456);
        let v = View {
            center_u: 128.0,
            lod: 0,
            ..view(2.0)
        };
        let mut out = vec![0u32; v.width * v.height];
        compose(&v, &mut cache, &look, None, &mut out);
        assert_eq!(out[10 * v.width + 5], 0x9912_3456);
        // Tile (1, 0) is missing: nothing behind it.
        assert_eq!(out[10 * v.width + 60], 0);
        // With fog, the fog is as see-through as the tiles.
        compose(&v, &mut cache, &look, Some(&flat()), &mut out);
        assert_eq!(out[10 * v.width + 60] >> 24, 153);
    }

    #[test]
    fn fog_layers_blend_over_the_ground() {
        let mut cache = filled(-1..2, -1..2, |_, _| 0xFF00_8000);
        let v = View {
            lod: 0,
            ..view(2.0)
        };
        // Half-see-through grey (premultiplied 0x40 of 0x80) over the left half of the view;
        // the layer stops at the middle.
        let mut grid = crate::fog::LayerGrid::for_view(&v);
        grid.w = ((64.0 - grid.start_u) / grid.texel) as usize;
        let fog = FogLayer {
            grid,
            pixels: vec![0x8040_4040; grid.w * grid.h],
            ..flat()
        };
        let mut out = vec![0u32; v.width * v.height];
        compose(&v, &mut cache, &LOOK, Some(&fog), &mut out);
        // clamp(rgb / a) * a + ground * (1 - a): 0x40 + 0x80 * 0.5 in green, 0x40 elsewhere.
        let p = out[10 * v.width + 5];
        assert!((channel(p, 8) - 0x80).abs() <= 1, "{p:08x}");
        assert!((channel(p, 0) - 0x40).abs() <= 1, "{p:08x}");
        // Past the layer, known ground is as it is; unknown ground gets the flat fog.
        assert_eq!(out[10 * v.width + 60], 0xFF00_8000);
        let mut cache = TileCache::new(4);
        compose(&v, &mut cache, &LOOK, Some(&fog), &mut out);
        assert_eq!(out[10 * v.width + 60], fog.flat);
    }

    #[test]
    fn backdrops_follow_the_layer() {
        assert_eq!(Look::new(false, false).backdrop, 0x8C99AB);
        assert_eq!(Look::new(false, true).backdrop, 0x29211F);
        assert_eq!(Look::new(true, true).backdrop, 0x0C0E12);
        let mut cache = TileCache::new(4);
        let v = view(2.0);
        let mut out = vec![0u32; v.width * v.height];
        compose(&v, &mut cache, &Look::new(true, false), None, &mut out);
        assert_eq!(out[0], 0xFF12_0E0C);
    }

    #[test]
    fn held_tiles_give_back_their_colours() {
        let mut cache = TileCache::new(4);
        let key = TileKey::new(TileKey::SURFACE, 0, 0);
        let pixels: Vec<u32> = (0..PIXELS as u32)
            .map(|i| [0xFF12_3456, 0x80FF_8040, 0][i as usize % 3])
            .collect();
        cache.insert_pixels(key, &pixels);
        let back = cache.pixels(&key).unwrap();
        for (p, q) in pixels.iter().zip(&back).take(3) {
            assert_eq!(p >> 24, q >> 24);
            for shift in [0, 8, 16] {
                assert!(
                    (channel(*p, shift) - channel(*q, shift)).abs() <= 1,
                    "{p:08x} {q:08x}"
                );
            }
        }
    }

    #[test]
    fn taps_cover_each_pixel_once() {
        for (step, crisp) in [(2.5, false), (3.9, false), (0.7, false), (0.5, true)] {
            let taps = Taps::new(40, -17.3, step, 64, crisp);
            for i in 0..40 {
                let t = taps.of(i);
                assert_eq!(t.iter().map(|t| t.weight).sum::<u32>(), AXIS_ONE);
                assert!(t.len() <= 5);
            }
            assert_eq!(taps.first_tile, -1);
        }
    }

    /// Composes the fullscreen map's largest image: `cargo test --release -- --ignored
    /// compose_speed --nocapture`.
    #[test]
    #[ignore]
    fn compose_speed() {
        let mut cache = filled(-8..9, -6..7, |x, y| {
            let h = (x as u32).wrapping_mul(2_654_435_761) ^ (y as u32).wrapping_mul(40_503);
            0xFF00_0000 | (h & 0x00FF_FFFF)
        });
        for zoom in [8.0, 4.0, 2.0, 1.4, 1.0] {
            let v = View {
                center_u: 3.3,
                center_v: -1.7,
                zoom,
                layer: TileKey::SURFACE,
                lod: 0,
                width: 1024,
                height: 576,
                round: false,
            };
            let mut out = vec![0u32; v.width * v.height];
            // A layer clear over a third of the view, see-through over a third and opaque over
            // the rest, as over a map explored in the middle.
            let grid = crate::fog::LayerGrid::for_view(&v);
            let fog = FogLayer {
                grid,
                pixels: (0..grid.w * grid.h)
                    .map(|i| [0, 0x8060_6060, 0xFFE0_E0E0][(i % grid.w) * 3 / grid.w])
                    .collect(),
                ..flat()
            };
            compose(&v, &mut cache, &LOOK, Some(&fog), &mut out);
            let runs = 20;
            let start = std::time::Instant::now();
            for _ in 0..runs {
                compose(&v, &mut cache, &LOOK, Some(&fog), &mut out);
            }
            let ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
            println!("compose 1024x576 at {zoom} px per block: {ms:.2} ms");
        }
    }

    #[test]
    fn image_and_map_points_invert() {
        let v = view(3.0);
        let (x, y) = v.image_xy(70.0, 50.0);
        let (u, w) = v.map_uv(x, y);
        assert!((u - 70.0).abs() < 1e-9 && (w - 50.0).abs() < 1e-9);
    }
}
