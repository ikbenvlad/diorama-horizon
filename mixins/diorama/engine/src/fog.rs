//! The fog over unknown ground: Diorama's terraced, sunlit clouds over the never-explored land, a
//! soft frontier with a halo round the revealed map and the clouds' shadow on it; warm smoke under
//! a roof; a dark low bank in caves.
//!
//! A CPU port of upstream's fog.fsh, by way of its line-for-line CPU copy render/ExportFog.java,
//! with the shader's slow drift in time. The fog is painted into a layer at about half the image's
//! resolution on a grid fixed to the map (MapViewport.drawFogLayer), off the game thread by a
//! [`FogWorker`], and [`crate::view::compose`] blends the layer over the ground.
//!
//! Map units here are blocks along the map's u (east) and v (south, tilted); a mask texel is
//! `FogMask::texel` units, a layer texel `LayerGrid::texel` units.

use crate::projection::COVERAGE_CELLS;
use crate::tile::TileKey;
use crate::view::{DETAILED_ZOOM, View};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// fog.fsh's constants; see there for what each does.
const NOISE: usize = 256;
const NOISE_PERIOD: f64 = 512.0;
const COS_PITCH: f64 = 0.643;
const LAYERS: usize = 8;
const NO_FOG_KNOWN: f32 = 0.95;
const FOG_MIX: f32 = 0.5;
const LOWEST: f32 = 0.12;
const LAYER_ALPHA: f32 = 0.45;
const SHADOW: f32 = 1.6;
const FOG_SHADOW: f32 = 0.7;
const CLOUD_CELL: f64 = 12.8;
const CLOUD_LEVELS: f32 = 5.0;
const LEVEL_THICK: f64 = 0.9;
const STAIR_NOISE: f32 = 0.6;
const ROUND: f32 = 0.45;
const EDGE_SOFT: f32 = 0.5;
const CLOUD_ALPHA: f32 = 0.95;
const WISP: f32 = 1.5;
const STRANDS: f32 = 1.0;
const WARP: f64 = 2.0;
const HAZE: f32 = 0.5;
const HALO: f32 = 0.25;
const DEPTH_DARK: f32 = 0.65;
const PILLOW: f32 = 1.0;
const GROUND_SHADOW: f32 = 0.3;
const SKY: [f32; 3] = [0.56, 0.65, 0.77];
const SUNLIGHT: [f32; 3] = [1.0, 0.93, 0.82];
const HAZE_COLOUR: [f32; 3] = [0.64, 0.75, 0.9];
/// normalize(-1, -0.4): toward the sun, on the map.
const SUN_U: f64 = -0.928_477;
const SUN_V: f64 = -0.371_391;
/// How far from explored land the clouds reach full height, in cells a level (MapViewport).
const CELLS_PER_LEVEL: f64 = 5.0;
/// The drift's speeds are whole noise periods per this many seconds, so it loops without a jump.
const DRIFT_LOOP: f64 = 1200.0;
/// Upstream's mask is this many texels across the view (MapViewport.MASK), with this margin.
const MASK_TEXELS: f64 = 256.0;
const MASK_MARGIN: f64 = 24.0;
/// The most mask texels a side here: the margin for the clouds' climb makes it wider than the view.
const MASK_MOST: f64 = 768.0;
/// Cells read around the mask's edge (MapViewport.CELL_MARGIN), and one more.
const CELL_MARGIN: i64 = 8;
/// Cells each side of a cell a point in it can read (MapViewport.SPAN_BEFORE/SPAN_AFTER).
const SPAN_BEFORE: i64 = 2;
const SPAN_AFTER: i64 = 3;
/// The moon's tint at full night (fog.fsh `moonlit`, NightLook).
const MOON: [f32; 3] = [0.27, 0.33, 0.49];
/// A layer is at most about this many texels: half of the largest image (1024 x 576).
const LAYER_MOST: f64 = 512.0 * 288.0;
/// Layer texels painted past each side of the view, so a short pan stays inside the layer.
const LAYER_MARGIN: i64 = 8;
/// Threads a layer is painted on.
pub const PAINT_THREADS: usize = 2;
/// The fog is painted again for its drift at most this often...
pub const REFRESH: Duration = Duration::from_millis(125);

// ---------------------------------------------------------------------------------------------
// Noise

/// The noise every fog paint shares: tileable value noise, a coarser scale in each channel from
/// red to alpha (FogTextures.noisePixels), kept to bytes like the texture.
struct Noise {
    texels: Vec<[f32; 4]>,
}

/// java.util.Random, so the noise is upstream's to the byte.
struct JavaRandom(u64);

impl JavaRandom {
    const MULTIPLIER: u64 = 0x5_DEEC_E66D;
    const MASK: u64 = (1 << 48) - 1;

    fn new(seed: i64) -> JavaRandom {
        JavaRandom((seed as u64 ^ Self::MULTIPLIER) & Self::MASK)
    }

    fn next(&mut self, bits: u32) -> u32 {
        self.0 = (self.0.wrapping_mul(Self::MULTIPLIER).wrapping_add(0xB)) & Self::MASK;
        (self.0 >> (48 - bits)) as u32
    }

    fn next_float(&mut self) -> f32 {
        self.next(24) as f32 / (1u32 << 24) as f32
    }
}

fn quintic(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn value_noise(size: usize, cells: usize, seed: i64) -> Vec<f32> {
    let mut random = JavaRandom::new(seed);
    let lattice: Vec<f32> = (0..cells * cells).map(|_| random.next_float()).collect();
    let mut out = vec![0f32; size * size];
    for y in 0..size {
        let fy = y as f32 * cells as f32 / size as f32;
        let y0 = fy as usize;
        let ty = quintic(fy - y0 as f32);
        for x in 0..size {
            let fx = x as f32 * cells as f32 / size as f32;
            let x0 = fx as usize;
            let tx = quintic(fx - x0 as f32);
            let at = |i: usize, j: usize| lattice[(j % cells) * cells + i % cells];
            let top = at(x0, y0) + (at(x0 + 1, y0) - at(x0, y0)) * tx;
            let bottom = at(x0, y0 + 1) + (at(x0 + 1, y0 + 1) - at(x0, y0 + 1)) * tx;
            out[y * size + x] = top + (bottom - top) * ty;
        }
    }
    out
}

fn fractal(size: usize, cells: usize, seed: i64) -> Vec<f32> {
    let a = value_noise(size, cells, seed);
    let b = value_noise(size, cells * 2, seed * 31 + 7);
    a.iter().zip(&b).map(|(a, b)| a * 0.7 + b * 0.3).collect()
}

fn noise() -> &'static Noise {
    static NOISE_TEXELS: OnceLock<Noise> = OnceLock::new();
    NOISE_TEXELS.get_or_init(|| {
        let channels = [
            fractal(NOISE, 6, 1),
            fractal(NOISE, 12, 2),
            fractal(NOISE, 24, 3),
            fractal(NOISE, 48, 4),
        ];
        let byte = |c: f32| (c * 255.0).round() / 255.0;
        Noise {
            texels: (0..NOISE * NOISE)
                .map(|i| std::array::from_fn(|c| byte(channels[c][i])))
                .collect(),
        }
    })
}

impl Noise {
    /// Channel `c` at noise point (nu, nv), repeating every 1 and filtered linearly, as the
    /// texture reads.
    #[inline]
    fn at(&self, c: usize, nu: f64, nv: f64) -> f32 {
        let fx = nu * NOISE as f64 - 0.5;
        let fy = nv * NOISE as f64 - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
        let wrap = NOISE as i64 - 1;
        let (xa, ya) = ((x0 as i64 & wrap) as usize, (y0 as i64 & wrap) as usize);
        let (xb, yb) = ((xa + 1) & wrap as usize, (ya + 1) & wrap as usize);
        let t = &self.texels;
        let top = mix(t[ya * NOISE + xa][c], t[ya * NOISE + xb][c], tx);
        let bottom = mix(t[yb * NOISE + xa][c], t[yb * NOISE + xb][c], tx);
        mix(top, bottom, ty)
    }
}

// ---------------------------------------------------------------------------------------------
// Kinds and scale

/// Which fog a view has: clouds over the open sky, smoke under a roof, a low bank in caves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FogKind {
    Clouds,
    Smoke,
    Cave,
}

impl FogKind {
    /// The fog of a cave slice, or of the sky view of a dimension with or without a roof.
    pub fn of(cave: bool, roofed: bool) -> FogKind {
        if cave {
            FogKind::Cave
        } else if roofed {
            FogKind::Smoke
        } else {
            FogKind::Clouds
        }
    }

    /// The colour of the fog's sunlit side (fog.fsh `lit`).
    pub fn lit(self) -> [f32; 3] {
        match self {
            FogKind::Clouds => [0.97, 0.97, 0.99],
            FogKind::Smoke => [0.26, 0.22, 0.21],
            FogKind::Cave => [0.24, 0.26, 0.33],
        }
    }

    /// The colour of its side away from the sun (fog.fsh `shade`).
    pub fn shade(self) -> [f32; 3] {
        match self {
            FogKind::Clouds => [0.48, 0.55, 0.67],
            FogKind::Smoke => [0.07, 0.055, 0.055],
            FogKind::Cave => [0.035, 0.04, 0.055],
        }
    }

    /// How tall the bank stands, in blocks: cave fog hugs the ground.
    fn height(self) -> f32 {
        if self == FogKind::Cave { 4.0 } else { 10.0 }
    }

    /// How far its edge is blurred, in blocks: cave fog stays tight to the tunnels explored.
    fn blur_units(self) -> f64 {
        if self == FogKind::Cave { 2.5 } else { 7.0 }
    }

    /// Whether its soft edge blends a near and a wide blur.
    fn wide(self) -> bool {
        self != FogKind::Cave
    }
}

/// The fog's scale at a zoom of `gui_zoom` GUI pixels per block: past [`DETAILED_ZOOM`] the noise
/// and the clouds grow by 2^(steps + fraction) so they keep their size on screen. The fraction
/// is in sixteenths, as upstream sends it.
pub fn fog_scale(gui_zoom: f64) -> (u32, f32) {
    let log = (DETAILED_ZOOM / gui_zoom.max(1e-9))
        .max(1.0)
        .log2()
        .min(7.99);
    let steps = log.floor();
    (steps as u32, ((log - steps) * 16.0).floor() as f32 / 16.0)
}

// ---------------------------------------------------------------------------------------------
// Layer grid

/// Where a fog layer's texels lie: a grid fixed to the map, whole texels from map 0, so a pan paints
/// the same fog at the same map points (MapViewport.drawFogLayer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerGrid {
    /// The map point at the first texel's corner.
    pub start_u: f64,
    pub start_v: f64,
    /// Map units a texel.
    pub texel: f64,
    pub w: usize,
    pub h: usize,
}

impl LayerGrid {
    /// The grid for a view: about two image pixels a texel (fewer for images past 1024 x 576),
    /// from the grid line a texel before the view's start, with [`LAYER_MARGIN`] more each side.
    pub fn for_view(view: &View) -> LayerGrid {
        let pixels = (view.width * view.height) as f64;
        let per_texel = (pixels / LAYER_MOST).sqrt().max(2.0);
        let texel = per_texel / view.zoom;
        let (min_u, min_v) = (view.min_u(), view.min_v());
        let start = |min: f64| ((min / texel).floor() - 1.0 - LAYER_MARGIN as f64) * texel;
        let (start_u, start_v) = (start(min_u), start(min_v));
        let span = |n: usize, min: f64, start: f64| {
            ((min - start) / texel + n as f64 / view.zoom / texel).ceil() as usize
                + 1
                + LAYER_MARGIN as usize
        };
        LayerGrid {
            start_u,
            start_v,
            texel,
            w: span(view.width, min_u, start_u),
            h: span(view.height, min_v, start_v),
        }
    }

    /// Whether every pixel of `view` reads this grid between texels it has, at its own scale.
    pub fn holds(&self, view: &View) -> bool {
        let fresh = LayerGrid::for_view(view);
        if (fresh.texel - self.texel).abs() > self.texel * 1e-9 {
            return false;
        }
        let (u0, v0) = (view.min_u(), view.min_v());
        let (u1, v1) = (
            u0 + view.width as f64 / view.zoom,
            v0 + view.height as f64 / view.zoom,
        );
        u0 >= self.start_u + self.texel
            && v0 >= self.start_v + self.texel
            && u1 <= self.start_u + (self.w as f64 - 1.0) * self.texel
            && v1 <= self.start_v + (self.h as f64 - 1.0) * self.texel
    }
}

// ---------------------------------------------------------------------------------------------
// Mask

/// The part of the map a mask covers and how finely, and which tiles it reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaskArea {
    pub origin_u: f64,
    pub origin_v: f64,
    /// Map units a mask texel.
    pub texel: f64,
    pub w: usize,
    pub h: usize,
    /// The tiles read: their layer and overview level.
    pub layer: i32,
    pub lod: u8,
}

impl MaskArea {
    /// The mask under a layer: texels as upstream's screen mask has them (two map units, doubling
    /// until 256 cover the view), past the layer by as far as the fog reads and, for clouds, as
    /// far as they climb, with its corner snapped so a short pan keeps the same mask.
    pub fn around(grid: &LayerGrid, layer: i32, lod: u8, kind: FogKind, steps: u32) -> MaskArea {
        let scale = f64::from(1u32 << steps);
        let snap = 128.0 * scale;
        let span = (grid.w as f64).max(grid.h as f64) * grid.texel;
        let mut margin = 64.0 + 48.0 * scale;
        if kind == FogKind::Clouds {
            margin += f64::from(CLOUD_LEVELS) * CELLS_PER_LEVEL * CLOUD_CELL * scale;
        }
        let mut texel = 2.0;
        while MASK_TEXELS * texel < span + MASK_MARGIN * 2.0 + snap
            || (span + 2.0 * margin + snap) / texel > MASK_MOST
        {
            texel *= 2.0;
        }
        let origin_u = ((grid.start_u - margin) / snap).floor() * snap;
        let origin_v = ((grid.start_v - margin) / snap).floor() * snap;
        let end =
            |start: f64, n: usize| ((start + n as f64 * grid.texel + margin) / snap).ceil() * snap;
        MaskArea {
            origin_u,
            origin_v,
            texel,
            w: ((end(grid.start_u, grid.w) - origin_u) / texel).ceil() as usize,
            h: ((end(grid.start_v, grid.h) - origin_v) / texel).ceil() as usize,
            layer,
            lod,
        }
    }

    /// The map rectangle covered: (u0, v0, u1, v1).
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        (
            self.origin_u,
            self.origin_v,
            self.origin_u + self.w as f64 * self.texel,
            self.origin_v + self.h as f64 * self.texel,
        )
    }
}

/// Each cloud cell's level over a mask, and the spans of levels around each (MapViewport.writeCells,
/// levelSpans), for cells `size` map units across.
#[derive(Debug)]
struct Cells {
    size: f64,
    first_i: i64,
    first_j: i64,
    w: usize,
    h: usize,
    level: Vec<u8>,
    low: Vec<u8>,
    high: Vec<u8>,
}

/// The fog's mask: the known share of each texel, how tall the clouds stand over it, its soft
/// known share, and where everything the fog reads is fully known (1) or fully unknown (2).
#[derive(Debug)]
pub struct FogMask {
    pub origin_u: f64,
    pub origin_v: f64,
    pub texel: f64,
    pub w: usize,
    pub h: usize,
    pub known: Vec<f32>,
    pub lift: Vec<f32>,
    /// The known share blurred as fog.fsh's knownSoft reads it, at each texel's middle.
    pub soft: Vec<f32>,
    pub flags: Vec<u8>,
    pub kind: FogKind,
    pub steps: u32,
    pub fraction: f32,
    /// The cells of the size drawn, and of twice it while the zoom hands over.
    cells: [Option<Cells>; 2],
}

/// The known share at a map point from tile coverage, as MapViewport.buildMask reads it: the tile
/// at the level drawn, else the nearest coarser one held, else a finer one up to two levels down.
struct Coverage<'a> {
    tiles: HashMap<TileKey, &'a [u8]>,
}

/// A tile held with its coverage, or none.
type Standing<'a> = Option<(TileKey, &'a [u8])>;

impl Coverage<'_> {
    fn cell(cov: &[u8], key: &TileKey, u: f64, v: f64) -> f32 {
        let span = key.units();
        let cell = span / f64::from(COVERAGE_CELLS);
        let last = COVERAGE_CELLS as i64 - 1;
        let x = (((u - key.min_u()) / cell).floor() as i64).clamp(0, last);
        let y = (((v - key.min_v()) / cell).floor() as i64).clamp(0, last);
        f32::from(cov[(y * COVERAGE_CELLS as i64 + x) as usize]) / 255.0
    }

    /// The tile at `key`'s level or the nearest coarser one held, with its coverage.
    fn standing(&self, key: TileKey) -> Standing<'_> {
        let mut up = key;
        loop {
            if let Some(c) = self.tiles.get(&up) {
                return Some((up, c));
            }
            if up.lod >= TileKey::MAX_LOD {
                return None;
            }
            up = up.parent();
        }
    }

    /// The known share at (u, v); `last` keeps the tile looked up last at the level drawn.
    fn known<'a>(
        &'a self,
        last: &mut Option<(TileKey, Standing<'a>)>,
        layer: i32,
        lod: u8,
        u: f64,
        v: f64,
    ) -> f32 {
        let units = TileKey {
            layer,
            u: 0,
            v: 0,
            lod,
        }
        .units();
        let key = TileKey {
            layer,
            u: (u / units).floor() as i32,
            v: (v / units).floor() as i32,
            lod,
        };
        if last.is_none_or(|(k, _)| k != key) {
            *last = Some((key, self.standing(key)));
        }
        if let Some((_, Some((k, c)))) = last {
            return Self::cell(c, k, u, v);
        }
        for down in 1..=2u8 {
            if lod < down {
                break;
            }
            let fine = lod - down;
            let span = units / f64::from(1u32 << down);
            let key = TileKey {
                layer,
                u: (u / span).floor() as i32,
                v: (v / span).floor() as i32,
                lod: fine,
            };
            if let Some(c) = self.tiles.get(&key) {
                return Self::cell(c, &key, u, v);
            }
        }
        0.0
    }
}

/// Builds the fog's mask over `area` from the coverage of tiles (64 x 64 known shares each, 0 to
/// 255), for fog of `kind` at scale `steps` + `fraction` ([`fog_scale`]). `rounded` rounds the
/// known area's edge off (MapStyle::rounded_fog); never in caves, whose narrow tunnels it would eat.
pub fn build_mask(
    coverage: &[(TileKey, Arc<[u8]>)],
    area: &MaskArea,
    kind: FogKind,
    rounded: bool,
    steps: u32,
    fraction: f32,
) -> FogMask {
    let cov = Coverage {
        tiles: coverage
            .iter()
            .filter(|(k, c)| {
                k.layer == area.layer && c.len() >= (COVERAGE_CELLS * COVERAGE_CELLS) as usize
            })
            .map(|(k, c)| (*k, &c[..]))
            .collect(),
    };
    let (w, h) = (area.w, area.h);
    let mut known = vec![0f32; w * h];
    let mut last = None;
    for j in 0..h {
        let v = area.origin_v + (j as f64 + 0.5) * area.texel;
        for i in 0..w {
            let u = area.origin_u + (i as f64 + 0.5) * area.texel;
            known[j * w + i] = cov.known(&mut last, area.layer, area.lod, u, v);
        }
    }
    let mut mask = FogMask {
        origin_u: area.origin_u,
        origin_v: area.origin_v,
        texel: area.texel,
        w,
        h,
        known,
        lift: vec![1.0; w * h],
        soft: vec![0.0; w * h],
        flags: vec![0; w * h],
        kind,
        steps,
        fraction,
        cells: [None, None],
    };
    if rounded && kind != FogKind::Cave && area.texel < 16.0 {
        mask.round_off(((10.0 / area.texel).round() as usize).max(1));
    }
    mask.write_soft();
    if kind == FogKind::Clouds {
        mask.climb();
    }
    mask.mark_reach();
    if kind == FogKind::Clouds {
        let base = CLOUD_CELL * f64::from(1u32 << steps);
        let handover = smoothstep(0.42, 0.58, fraction);
        let size = if handover >= 1.0 { base * 2.0 } else { base };
        mask.cells[0] = Some(mask.cells_of(size));
        if handover > 0.0 && handover < 1.0 {
            mask.cells[1] = Some(mask.cells_of(size * 2.0));
        }
    }
    mask
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn byte(x: f32) -> f32 {
    (x * 255.0).round() / 255.0
}

/// Distances in thirds of a texel to the nearest 0, by the 3-4 chamfer: two passes over the grid.
fn chamfer(d: &mut [u32], w: usize, h: usize) {
    for j in 0..h {
        for i in 0..w {
            let k = j * w + i;
            let mut v = d[k];
            if i > 0 {
                v = v.min(d[k - 1] + 3);
            }
            if j > 0 {
                v = v.min(d[k - w] + 3);
                if i > 0 {
                    v = v.min(d[k - w - 1] + 4);
                }
                if i + 1 < w {
                    v = v.min(d[k - w + 1] + 4);
                }
            }
            d[k] = v;
        }
    }
    for j in (0..h).rev() {
        for i in (0..w).rev() {
            let k = j * w + i;
            let mut v = d[k];
            if i + 1 < w {
                v = v.min(d[k + 1] + 3);
            }
            if j + 1 < h {
                v = v.min(d[k + w] + 3);
                if i + 1 < w {
                    v = v.min(d[k + w + 1] + 4);
                }
                if i > 0 {
                    v = v.min(d[k + w - 1] + 4);
                }
            }
            d[k] = v;
        }
    }
}

/// Counts, per rectangle from (0, 0), of the texels where `hit` fails: `(w + 1) * (h + 1)`.
fn summed(w: usize, h: usize, miss: impl Fn(usize) -> bool) -> Vec<u32> {
    let row = w + 1;
    let mut out = vec![0u32; row * (h + 1)];
    for j in 0..h {
        let mut line = 0;
        for i in 0..w {
            line += u32::from(miss(j * w + i));
            out[(j + 1) * row + i + 1] = out[j * row + i + 1] + line;
        }
    }
    out
}

fn box_count(sums: &[u32], w: usize, i0: usize, j0: usize, i1: usize, j1: usize) -> u32 {
    let row = w + 1;
    sums[j1 * row + i1] + sums[j0 * row + i0] - sums[j0 * row + i1] - sums[j1 * row + i0]
}

impl FogMask {
    /// A grid at mask texel (x, y), in texels, filtered between texel middles and clamped at the
    /// edge, as the graphics card reads a texture.
    #[inline]
    fn filtered(&self, grid: &[f32], x: f64, y: f64) -> f32 {
        let (fx, fy) = (x - 0.5, y - 0.5);
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
        let (lx, ly) = (self.w as i64 - 1, self.h as i64 - 1);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let xa = x0.clamp(0, lx) as usize;
        let xb = (x0 + 1).clamp(0, lx) as usize;
        let ya = y0.clamp(0, ly) as usize;
        let yb = (y0 + 1).clamp(0, ly) as usize;
        let w = self.w;
        let top = mix(grid[ya * w + xa], grid[ya * w + xb], tx);
        let bottom = mix(grid[yb * w + xa], grid[yb * w + xb], tx);
        mix(top, bottom, ty)
    }

    /// A grid at map point (u, v).
    #[inline]
    fn sample(&self, grid: &[f32], u: f64, v: f64) -> f32 {
        self.filtered(
            grid,
            (u - self.origin_u) / self.texel,
            (v - self.origin_v) / self.texel,
        )
    }

    /// Rounds the known area off so the fog edge follows a smooth line instead of chunk steps:
    /// blurred, kept where solidly known, and never more than it was (MapViewport.roundOff).
    fn round_off(&mut self, radius: usize) {
        let mut a = self.known.clone();
        let mut b = vec![0f32; a.len()];
        for _ in 0..2 {
            self.box_blur(&a, &mut b, radius, true);
            self.box_blur(&b, &mut a, radius, false);
        }
        for (k, blurred) in self.known.iter_mut().zip(&a) {
            let t = clamp01((blurred - 0.5) / 0.35);
            *k = k.min(byte(t * t * (3.0 - 2.0 * t)));
        }
    }

    fn box_blur(&self, from: &[f32], to: &mut [f32], radius: usize, horizontal: bool) {
        let (lines, length) = if horizontal {
            (self.h, self.w)
        } else {
            (self.w, self.h)
        };
        let w = self.w;
        let at = |line: usize, k: usize| {
            if horizontal {
                line * w + k
            } else {
                k * w + line
            }
        };
        let scale = 1.0 / (2 * radius + 1) as f32;
        let r = radius as i64;
        let last = length as i64 - 1;
        for line in 0..lines {
            let mut sum = 0f32;
            for k in -r..=r {
                sum += from[at(line, k.clamp(0, last) as usize)];
            }
            for k in 0..length {
                to[at(line, k)] = sum * scale;
                let add = (k as i64 + r + 1).min(last) as usize;
                let drop = (k as i64 - r).max(0) as usize;
                sum += from[at(line, add)] - from[at(line, drop)];
            }
        }
    }

    /// The known share blurred over `r` texels round (x, y), in texels (MapViewport.blurAt).
    fn blur_at(&self, x: f64, y: f64, r: f64) -> f32 {
        let d = r * 1.9;
        let k = |dx: f64, dy: f64| self.filtered(&self.known, x + dx, y + dy);
        k(0.0, 0.0) * 0.2
            + (k(r, 0.0) + k(-r, 0.0) + k(0.0, r) + k(0.0, -r)) * 0.1
            + (k(d, d) + k(-d, d) + k(d, -d) + k(-d, -d)) * 0.1
    }

    /// The soft known share at each texel's middle: one blur, or for the wide fog half of it and
    /// half of one 2.4 times as wide (MapViewport.writeSoft).
    fn write_soft(&mut self) {
        let units = self.kind.blur_units();
        let wide = self.kind.wide();
        let near = (units / self.texel).max(1.0);
        let far = (units * 2.4 / self.texel).max(1.0);
        let reach = (1.9 * if wide { far } else { near }).ceil() as usize + 2;
        let (w, h) = (self.w, self.h);
        let not_full = summed(w, h, |k| self.known[k] < 1.0);
        let not_empty = summed(w, h, |k| self.known[k] > 0.0);
        let mut soft = vec![0f32; w * h];
        for j in 0..h {
            let (t, b) = (j.saturating_sub(reach), (j + reach).min(h - 1) + 1);
            for i in 0..w {
                let (l, r) = (i.saturating_sub(reach), (i + reach).min(w - 1) + 1);
                soft[j * w + i] = if box_count(&not_full, w, l, t, r, b) == 0 {
                    1.0
                } else if box_count(&not_empty, w, l, t, r, b) == 0 {
                    0.0
                } else {
                    let (x, y) = (i as f64 + 0.5, j as f64 + 0.5);
                    let s = self.blur_at(x, y, near);
                    byte(if wide {
                        0.5 * s + 0.5 * self.blur_at(x, y, far)
                    } else {
                        s
                    })
                };
            }
        }
        self.soft = soft;
    }

    /// The clouds' climb over never-explored land: a level every few cells away from explored
    /// land, full height with none (ExportFog.climb).
    fn climb(&mut self) {
        let (w, h) = (self.w, self.h);
        let far = u32::MAX / 4;
        let mut d: Vec<u32> = self
            .known
            .iter()
            .map(|&k| if k >= 0.5 { 0 } else { far })
            .collect();
        if d.iter().all(|&x| x != 0) {
            self.lift.fill(1.0);
            return;
        }
        chamfer(&mut d, w, h);
        let thirds = 3.0
            * f64::from(CLOUD_LEVELS)
            * CELLS_PER_LEVEL
            * CLOUD_CELL
            * f64::from(1u32 << self.steps)
            / self.texel;
        for (lift, d) in self.lift.iter_mut().zip(&d) {
            *lift = byte((f64::from(*d) / thirds).min(1.0) as f32);
        }
    }

    /// How far, in mask texels, the fog reads from a pixel: west, east, north, south
    /// (MapViewport.fogReach and cloudReach).
    fn reach(&self) -> [usize; 4] {
        let kind = self.kind;
        let texel = self.texel;
        let scale = f64::from(1u32 << self.steps);
        let mut blur_units = kind.blur_units();
        if kind.wide() {
            blur_units *= 2.4;
        }
        let blur = 1.9 * (blur_units / texel).max(1.0);
        let shift = f64::from(kind.height()) * COS_PITCH / texel;
        let sun = 9.0 * scale * 2.0 / texel;
        let slack = 1.5;
        let mut clouds = [0usize; 4];
        if kind == FogKind::Clouds {
            let cell_scale = if self.fraction * 16.0 >= 7.0 {
                2.0
            } else {
                1.0
            };
            let cell = CLOUD_CELL * scale * cell_scale / texel;
            let around = 2.45 * cell + 1.5;
            let lean = f64::from(CLOUD_LEVELS) * LEVEL_THICK * cell * COS_PITCH;
            let side = around.ceil() as usize;
            clouds = [side, side, side, (around + lean).ceil() as usize];
        }
        [
            ((blur.max(sun) + slack).ceil() as usize).max(clouds[0]),
            ((blur + slack).ceil() as usize).max(clouds[1]),
            ((blur.max(sun * 0.4) + slack).ceil() as usize).max(clouds[2]),
            ((blur + shift + slack).ceil() as usize).max(clouds[3]),
        ]
    }

    /// Marks texels where everything the fog reads from a pixel in them is fully known (1), or
    /// fully unknown with the clouds at full height (2), so those pixels skip what they needn't
    /// read (MapViewport.markReach).
    fn mark_reach(&mut self) {
        let [west, east, north, south] = self.reach();
        let (w, h) = (self.w, self.h);
        let clouds = self.kind == FogKind::Clouds;
        let not_known = summed(w, h, |k| self.known[k] < 1.0);
        let not_unknown = summed(w, h, |k| {
            !(self.known[k] <= 0.0 && (!clouds || self.lift[k] >= 1.0))
        });
        for j in 0..h {
            let (t, b) = (j.saturating_sub(north), (j + south).min(h - 1) + 1);
            for i in 0..w {
                let (l, r) = (i.saturating_sub(west), (i + east).min(w - 1) + 1);
                self.flags[j * w + i] = if box_count(&not_known, w, l, t, r, b) == 0 {
                    1
                } else if box_count(&not_unknown, w, l, t, r, b) == 0 {
                    2
                } else {
                    0
                };
            }
        }
    }

    /// fog.fsh's cellLevel: 0 where the cell's middle is half known, else the climb in levels with
    /// a little noise, 1 to 5.
    fn cell_level(&self, ci: i64, cj: i64, size: f64) -> u8 {
        let (pu, pv) = ((ci as f64 + 0.5) * size, (cj as f64 + 0.5) * size);
        if self.sample(&self.known, pu, pv) >= 0.5 {
            return 0;
        }
        let climb = self.sample(&self.lift, pu, pv) * CLOUD_LEVELS;
        let n = noise().at(0, pu / (size * 20.0) + 0.21, pv / (size * 20.0) + 0.21);
        (climb + (n - 0.5) * STAIR_NOISE)
            .ceil()
            .clamp(1.0, CLOUD_LEVELS) as u8
    }

    fn cells_of(&self, size: f64) -> Cells {
        let (u0, v0, u1, v1) = (
            self.origin_u,
            self.origin_v,
            self.origin_u + self.w as f64 * self.texel,
            self.origin_v + self.h as f64 * self.texel,
        );
        let first_i = (u0 / size).floor() as i64 - CELL_MARGIN;
        let first_j = (v0 / size).floor() as i64 - CELL_MARGIN;
        let w = ((u1 / size).ceil() as i64 + CELL_MARGIN - first_i + 1) as usize;
        let h = ((v1 / size).ceil() as i64 + CELL_MARGIN - first_j + 1) as usize;
        let mut level = vec![0u8; w * h];
        for j in 0..h {
            for i in 0..w {
                level[j * w + i] = self.cell_level(first_i + i as i64, first_j + j as i64, size);
            }
        }
        // Lowest and highest from SPAN_BEFORE before to SPAN_AFTER after, along rows then down
        // columns; near the grid's edge, any level.
        let (mut row_low, mut row_high) = (vec![0u8; w * h], vec![5u8; w * h]);
        for j in 0..h {
            for i in 0..w {
                let (a, b) = (i as i64 - SPAN_BEFORE, i as i64 + SPAN_AFTER);
                if a < 0 || b >= w as i64 {
                    continue;
                }
                let run = &level[j * w + a as usize..=j * w + b as usize];
                row_low[j * w + i] = *run.iter().min().expect("run");
                row_high[j * w + i] = *run.iter().max().expect("run");
            }
        }
        let (mut low, mut high) = (vec![0u8; w * h], vec![5u8; w * h]);
        for j in 0..h {
            let (a, b) = (j as i64 - SPAN_BEFORE, j as i64 + SPAN_AFTER);
            if a < 0 || b >= h as i64 {
                continue;
            }
            for i in 0..w {
                let (mut lo, mut hi) = (5u8, 0u8);
                for jj in a..=b {
                    lo = lo.min(row_low[jj as usize * w + i]);
                    hi = hi.max(row_high[jj as usize * w + i]);
                }
                low[j * w + i] = lo;
                high[j * w + i] = hi;
            }
        }
        Cells {
            size,
            first_i,
            first_j,
            w,
            h,
            level,
            low,
            high,
        }
    }
}

impl Cells {
    #[inline]
    fn index(&self, ci: i64, cj: i64) -> Option<usize> {
        let (i, j) = (ci - self.first_i, cj - self.first_j);
        (i >= 0 && j >= 0 && (i as usize) < self.w && (j as usize) < self.h)
            .then(|| j as usize * self.w + i as usize)
    }
}

// ---------------------------------------------------------------------------------------------
// Painting

/// What a layer is painted with besides the mask.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FogParams {
    /// How far into the night, 0 to 1: the fog dims toward moonlight.
    pub night: f32,
    /// Seconds, for the drift.
    pub seconds: f64,
}

/// The fog painted over a [`LayerGrid`]: premultiplied ABGR per texel.
#[derive(Clone, Debug)]
pub struct FogLayer {
    pub grid: LayerGrid,
    pub pixels: Vec<u32>,
    /// The fog over plain unknown ground, opaque ABGR: drawn where the layer doesn't reach.
    pub flat: u32,
    pub kind: FogKind,
    /// The map layer (surface or cave slice) it was painted for.
    pub layer: i32,
    /// Goes up with every layer painted.
    pub version: u64,
    /// The version of the tiles held it was painted from ([`FogJob::coverage_version`]).
    pub coverage_version: u64,
}

static NEXT_VERSION: AtomicU64 = AtomicU64::new(1);

impl FogLayer {
    /// No painted fog: the flat fog colour everywhere nothing is known.
    pub fn flat(kind: FogKind, layer: i32, night: f32) -> FogLayer {
        FogLayer {
            grid: LayerGrid {
                start_u: 0.0,
                start_v: 0.0,
                texel: 1.0,
                w: 0,
                h: 0,
            },
            pixels: Vec::new(),
            flat: flat_colour(kind, night),
            kind,
            layer,
            version: 0,
            coverage_version: 0,
        }
    }
}

fn moonlit(night: f32) -> [f32; 3] {
    let night = night.clamp(0.0, 1.0);
    std::array::from_fn(|i| mix(1.0, MOON[i], night))
}

fn pack(rgba: [f32; 4]) -> u32 {
    let c = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u32;
    (c(rgba[3]) << 24) | (c(rgba[2]) << 16) | (c(rgba[1]) << 8) | c(rgba[0])
}

/// The fog over plain never-explored ground, opaque ABGR: the average of a patch painted over a
/// mask that knows nothing.
fn flat_colour(kind: FogKind, night: f32) -> u32 {
    static DAY: OnceLock<[[f32; 3]; 3]> = OnceLock::new();
    let day = DAY.get_or_init(|| {
        [FogKind::Clouds, FogKind::Smoke, FogKind::Cave].map(|kind| {
            let grid = LayerGrid {
                start_u: 0.0,
                start_v: 0.0,
                texel: 1.0,
                w: 16,
                h: 16,
            };
            let area = MaskArea::around(&grid, TileKey::SURFACE, 0, kind, 0);
            let mask = build_mask(&[], &area, kind, false, 0, 0.0);
            let pixels = paint_pixels(
                &mask,
                &FogParams {
                    night: 0.0,
                    seconds: 0.0,
                },
                &grid,
                1,
            );
            let mut sum = [0f32; 4];
            for p in &pixels {
                for (c, s) in sum.iter_mut().enumerate() {
                    *s += ((p >> (8 * c)) & 0xFF) as f32;
                }
            }
            std::array::from_fn(|c| sum[c] / sum[3].max(1.0))
        })
    });
    let rgb = day[kind as usize];
    let moon = moonlit(night);
    pack([rgb[0] * moon[0], rgb[1] * moon[1], rgb[2] * moon[2], 1.0])
}

/// fog.fsh's hash.
fn hash(x: f64, y: f64) -> f32 {
    let s = (x * 12.9898 + y * 78.233).sin() * 43758.5453;
    (s - s.floor()) as f32
}

/// A random number for the map square of side `q` that holds (u, v), repeating every 1024 units.
fn square_hash(u: f64, v: f64, q: f64) -> f32 {
    let period = (2.0 * NOISE_PERIOD / q).max(1.0);
    hash(
        (u / q).floor().rem_euclid(period),
        (v / q).floor().rem_euclid(period),
    )
}

/// The march's jitter, -0.5 to 0.5, from the map point, so it stays with the map as it pans
/// (fog.fsh mapJitter): squares of the two powers of two round a texel's size, blended by where
/// the size falls between them.
struct Jitter {
    q: f64,
    t: f32,
    spread: f32,
}

impl Jitter {
    /// For texels `pixel` map units across.
    fn new(pixel: f64) -> Jitter {
        let size = pixel.max(1e-4).log2();
        let t = (size - size.floor()) as f32;
        Jitter {
            q: size.floor().exp2(),
            t,
            spread: ((1.0 - t) * (1.0 - t) + t * t).sqrt(),
        }
    }

    fn at(&self, u: f64, v: f64) -> f32 {
        let j = mix(
            square_hash(u, v, self.q),
            square_hash(u, v, self.q * 2.0),
            self.t,
        ) - 0.5;
        (j / self.spread).clamp(-0.5, 0.5)
    }
}

/// Where the fog looks for a texel: the pixel's flags, like the shader's globals.
#[derive(Clone, Copy)]
struct Here {
    clear: bool,
    deep: bool,
}

/// What every texel of one paint reads.
struct Painter<'a> {
    mask: &'a FogMask,
    noise: &'static Noise,
    kind: FogKind,
    lit: [f32; 3],
    shade: [f32; 3],
    height: f32,
    fog_scale: f64,
    drift: f64,
    moonlit: [f32; 3],
    /// The fog's noise worked out once on a grid of the fine noise's texels round the layer,
    /// read filtered between them: the noise is smooth over many texels, so this reads as the
    /// shader's three filtered reads (two scales' worth) do, for a fraction of the work.
    field: Field,
}

/// Values on a grid of map points, from (u0, v0), `step` units apart, filtered between them.
struct Field {
    u0: f64,
    v0: f64,
    step: f64,
    w: usize,
    h: usize,
    values: Vec<f32>,
}

impl Field {
    fn new(
        u0: f64,
        v0: f64,
        u1: f64,
        v1: f64,
        step: f64,
        value: impl Fn(f64, f64) -> f32,
    ) -> Field {
        let (u0, v0) = ((u0 / step).floor() * step, (v0 / step).floor() * step);
        let w = ((u1 - u0) / step).ceil() as usize + 2;
        let h = ((v1 - v0) / step).ceil() as usize + 2;
        let mut values = Vec::with_capacity(w * h);
        for j in 0..h {
            for i in 0..w {
                values.push(value(u0 + i as f64 * step, v0 + j as f64 * step));
            }
        }
        Field {
            u0,
            v0,
            step,
            w,
            h,
            values,
        }
    }

    #[inline]
    fn at(&self, u: f64, v: f64) -> f32 {
        let fx = ((u - self.u0) / self.step).clamp(0.0, (self.w - 1) as f64);
        let fy = ((v - self.v0) / self.step).clamp(0.0, (self.h - 1) as f64);
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
        let (i, j) = (x0 as usize, y0 as usize);
        let (i1, j1) = ((i + 1).min(self.w - 1), (j + 1).min(self.h - 1));
        let f = &self.values;
        let top = mix(f[j * self.w + i], f[j * self.w + i1], tx);
        let bottom = mix(f[j1 * self.w + i], f[j1 * self.w + i1], tx);
        mix(top, bottom, ty)
    }
}

/// Noise at the two whole scale steps round the fog's scale, blended (fog.fsh noiseAt).
fn noise_at(noise: &Noise, steps: u32, fraction: f32, drift: f64, u: f64, v: f64) -> f32 {
    let layer = |nu: f64, nv: f64| {
        noise.at(0, nu + 5.0 * drift, nv + 2.0 * drift) * 0.55
            + noise.at(1, nu - 8.0 * drift, nv + 3.0 * drift) * 0.3
            + noise.at(2, nu + 13.0 * drift, nv - 9.0 * drift) * 0.15
    };
    let coarse = NOISE_PERIOD * f64::from(1u32 << (steps + 1));
    let (cu, cv) = (u / coarse, v / coarse);
    let fine = layer(cu * 2.0, cv * 2.0);
    if fraction == 0.0 {
        return fine;
    }
    mix(fine, layer(cu, cv), fraction)
}

impl Painter<'_> {
    #[inline]
    fn known_at(&self, here: Here, u: f64, v: f64) -> f32 {
        if here.deep {
            0.0
        } else {
            self.mask.sample(&self.mask.known, u, v)
        }
    }

    #[inline]
    fn known_soft(&self, here: Here, u: f64, v: f64) -> f32 {
        if here.clear {
            1.0
        } else if here.deep {
            0.0
        } else {
            self.mask.sample(&self.mask.soft, u, v)
        }
    }

    #[inline]
    fn noise_at(&self, u: f64, v: f64) -> f32 {
        self.field.at(u, v)
    }

    fn thickness_from(&self, known: f32, n: f32, lift: f32) -> f32 {
        let unknown = 1.0 - known;
        let edge = smoothstep(0.0, 0.35, unknown);
        let amount = smoothstep(0.12, 0.75, unknown + (n - 0.5) * 1.5 * edge);
        self.height * mix(LOWEST, 1.0, lift) * amount * (0.45 + 0.75 * n)
    }

    fn thickness_at(&self, here: Here, u: f64, v: f64, known: f32) -> f32 {
        if known >= NO_FOG_KNOWN {
            return 0.0;
        }
        // Deep in the unknown the clouds stand at full height everywhere read.
        let lift = if self.kind == FogKind::Clouds && !here.deep {
            self.mask.sample(&self.mask.lift, u, v)
        } else {
            1.0
        };
        self.thickness_from(known, self.noise_at(u, v), lift)
    }

    // --- Clouds -------------------------------------------------------------------------------

    fn cells(&self, which: usize) -> &Cells {
        self.mask.cells[which].as_ref().expect("cloud cells")
    }

    /// A cell's level, from the grid or worked out where the grid doesn't reach.
    #[inline]
    fn level(&self, cells: &Cells, ci: i64, cj: i64) -> f32 {
        match cells.index(ci, cj) {
            Some(k) => f32::from(cells.level[k]),
            None => f32::from(self.mask.cell_level(ci, cj, cells.size)),
        }
    }

    /// 1 if every cell a point within reach of p reads is at level k or above, 0 if every one is
    /// under it, else none (fog.fsh sameCover).
    #[inline]
    fn same_cover(&self, cells: &Cells, pu: f64, pv: f64, k: f32) -> Option<f32> {
        let at = cells.index(
            (pu / cells.size - 0.5).floor() as i64,
            (pv / cells.size - 0.5).floor() as i64,
        )?;
        if k <= f32::from(cells.low[at]) {
            Some(1.0)
        } else if k > f32::from(cells.high[at]) {
            Some(0.0)
        } else {
            None
        }
    }

    fn level_cover(&self, which: usize, pu: f64, pv: f64, k: f32) -> f32 {
        let cells = self.cells(which);
        if let Some(same) = self.same_cover(cells, pu, pv, k) {
            return same;
        }
        let (gx, gy) = (pu / cells.size - 0.5, pv / cells.size - 0.5);
        let (ix, iy) = (gx.floor(), gy.floor());
        let fx = smoothstep(0.5 - ROUND, 0.5 + ROUND, (gx - ix) as f32);
        let fy = smoothstep(0.5 - ROUND, 0.5 + ROUND, (gy - iy) as f32);
        let (ix, iy) = (ix as i64, iy as i64);
        let over = |i: i64, j: i64| f32::from(u8::from(self.level(cells, i, j) >= k));
        mix(
            mix(over(ix, iy), over(ix + 1, iy), fx),
            mix(over(ix, iy + 1), over(ix + 1, iy + 1), fx),
            fy,
        )
    }

    fn wisp_at(&self, pu: f64, pv: f64, size: f64) -> f32 {
        let (nu, nv) = (pu / (size * 4.0), pv / (size * 4.0));
        let d = self.drift;
        self.noise.at(1, nu + 5.0 * d, nv + 2.0 * d) * 0.6
            + self.noise.at(2, nu * 2.0 - 7.0 * d, nv * 2.0 + 4.0 * d) * 0.4
    }

    fn warped(&self, pu: f64, pv: f64, size: f64, k: f32) -> (f64, f64) {
        let (nu, nv) = (pu / (size * 2.5), pv / (size * 2.5));
        let d = self.drift;
        let n = self.noise;
        let au = n.at(1, nu + 3.0 * d, nv + d) - 0.5;
        let av = n.at(2, nu + 0.37 - 2.0 * d, nv + 0.37 + 3.0 * d) - 0.5;
        let bu = n.at(3, nu * 3.0 + 0.11, nv * 3.0 + 0.11) - 0.5;
        let bv = n.at(1, nu * 3.0 + 0.71, nv * 3.0 + 0.71) - 0.5;
        let amount = WARP * size * if k <= 1.0 { 0.35 } else { 1.0 };
        (
            pu + f64::from(au + bu * 0.45) * amount,
            pv + f64::from(av + bv * 0.45) * amount,
        )
    }

    fn curled_cover(&self, which: usize, pu: f64, pv: f64, k: f32) -> f32 {
        let cells = self.cells(which);
        if let Some(same) = self.same_cover(cells, pu, pv, k) {
            return same;
        }
        let (wu, wv) = self.warped(pu, pv, cells.size, k);
        self.level_cover(which, wu, wv, k)
    }

    fn bitten(&self, cover: f32, pu: f64, pv: f64, size: f64) -> f32 {
        if cover <= 0.0 || cover >= 1.0 {
            return cover;
        }
        let bite = smoothstep(0.35, 0.75, self.wisp_at(pu, pv, size));
        cover - WISP * bite * (1.0 - cover) * cover * 2.0
    }

    fn terrace_inside(&self, which: usize, pu: f64, pv: f64, h: f64, level: f64) -> bool {
        let k = (h / level).ceil().clamp(1.0, f64::from(CLOUD_LEVELS)) as f32;
        let t = clamp01(((h - (f64::from(k) - 0.5) * level).abs() / (0.5 * level)) as f32);
        let t = t * t * t * t;
        let size = self.cells(which).size;
        let cover = self.bitten(self.curled_cover(which, pu, pv, k), pu, pv, size);
        cover - (0.25 + 0.5 * t) > 0.0
    }

    fn terrace_colour(
        h: f64,
        level: f64,
        top: f64,
        blocked: f32,
        sun_edge: f32,
        far_edge: f32,
    ) -> [f32; 3] {
        let k = (h / level).ceil().clamp(1.0, f64::from(CLOUD_LEVELS)) as f32;
        let up = clamp01((h / level) as f32 - (k - 1.0)).powf(0.8);
        let mut shading = (0.12 + 0.5 * up) * (2.0 - SHADOW * blocked * (1.0 - up * up));
        shading *= mix(1.0, 1.0 + PILLOW * (0.3 * sun_edge - 0.45 * far_edge), up);
        let depth = mix(DEPTH_DARK, 1.0, k / CLOUD_LEVELS);
        let haze = HAZE * (1.0 - (h / top) as f32).max(0.0).powf(1.5);
        std::array::from_fn(|i| {
            let c = (SKY[i] * (1.0 - 0.35 * shading) + SUNLIGHT[i] * 0.5 * (0.1 + shading)) * depth;
            mix(c.min(1.0), HAZE_COLOUR[i], haze)
        })
    }

    /// The cloud surface met at height h over p: colour and how solid it is.
    fn terrace_surface(
        &self,
        which: usize,
        pu: f64,
        pv: f64,
        h: f64,
        top: f64,
        jitter: f32,
    ) -> [f32; 4] {
        let size = self.cells(which).size;
        let level = size * LEVEL_THICK;
        let k = (h / level).ceil().clamp(1.0, f64::from(CLOUD_LEVELS)) as f32;
        let (tu, tv) = (SUN_U * size * 0.4, SUN_V * size * 0.4);
        let j = f64::from(jitter) * 0.5;
        let blocked = self.level_cover(which, pu + tu * (1.0 + j), pv + tv * (1.0 + j), k)
            + self.level_cover(which, pu + tu * (2.0 + j), pv + tv * (2.0 + j), k + 1.0);
        let (su, sv) = (SUN_U * size * 0.35, SUN_V * size * 0.35);
        let sun_edge = 1.0 - self.level_cover(which, pu + su, pv + sv, k);
        let far_edge = 1.0 - self.level_cover(which, pu - su, pv - sv, k);
        let colour = Self::terrace_colour(h, level, top, blocked, sun_edge, far_edge);
        let curled = self.curled_cover(which, pu, pv, k);
        let cover = self.bitten(curled, pu, pv, size);
        let edge = 1.0 - smoothstep(0.55, 0.95, curled);
        let mut alpha = CLOUD_ALPHA * smoothstep(0.0, EDGE_SOFT, cover - 0.25);
        if edge > 0.0 {
            let strands = smoothstep(
                0.25,
                0.75,
                self.wisp_at(pu * 0.35 + 53.0, pv * 1.4 + 53.0, size * 0.5),
            );
            alpha *= mix(1.0, strands, edge * STRANDS);
        }
        [colour[0], colour[1], colour[2], alpha]
    }

    /// The clouds over a ground point: premultiplied colour, and how much shows through. The line
    /// of sight goes on past the first surface to the next one down.
    fn clouds(&self, which: usize, u: f64, v: f64, jitter: f32, cloud_top: f32) -> [f32; 4] {
        let size = self.cells(which).size;
        let level = size * LEVEL_THICK;
        let top = f64::from(CLOUD_LEVELS) * level;
        let steps = CLOUD_LEVELS as usize * 3;
        let dh = top / steps as f64;
        let j = f64::from(jitter) * 0.9;
        let first = (CLOUD_LEVELS - cloud_top) as usize * 3;
        let mut last = if first == 0 {
            top
        } else {
            top - (first as f64 - 0.5 + j) * dh
        };
        let mut was_inside = false;
        let mut surfaces = 0;
        let mut through = 1f32;
        let mut glow = [0f32; 3];
        for s in first..steps {
            let h = top - (s as f64 + 0.5 + j) * dh;
            let inside = self.terrace_inside(which, u, v + h * COS_PITCH, h, level);
            if inside && !was_inside {
                let (mut above, mut below) = (last, h);
                for _ in 0..3 {
                    let mid = 0.5 * (above + below);
                    if self.terrace_inside(which, u, v + mid * COS_PITCH, mid, level) {
                        below = mid;
                    } else {
                        above = mid;
                    }
                }
                let hit = self.terrace_surface(which, u, v + below * COS_PITCH, below, top, jitter);
                for c in 0..3 {
                    glow[c] += through * hit[3] * hit[c];
                }
                through *= 1.0 - hit[3];
                surfaces += 1;
                if surfaces >= 2 || through < 0.1 {
                    break;
                }
            }
            was_inside = inside;
            last = h;
        }
        [glow[0], glow[1], glow[2], through]
    }

    /// What `clouds` gives over plain never-explored land: the top level's top, solid.
    fn flat_top(size: f64, jitter: f32) -> [f32; 4] {
        let level = size * LEVEL_THICK;
        let top = f64::from(CLOUD_LEVELS) * level;
        let dh = top / f64::from(CLOUD_LEVELS * 3.0);
        let h = top - (0.5 + f64::from(jitter) * 0.9) * dh / 8.0;
        let c = Self::terrace_colour(h, level, top, 1.0, 0.0, 0.0);
        [
            c[0] * CLOUD_ALPHA,
            c[1] * CLOUD_ALPHA,
            c[2] * CLOUD_ALPHA,
            1.0 - CLOUD_ALPHA,
        ]
    }

    fn ground_shadow(&self, which: usize, u: f64, v: f64) -> f32 {
        let d = self.cells(which).size * LEVEL_THICK * 1.5;
        smoothstep(
            0.3,
            0.6,
            self.level_cover(which, u + SUN_U * d, v + SUN_V * d, 1.0),
        )
    }

    /// The highest level a march from (u, v) can meet: from the spans of the cell under it and
    /// the one three south, which the line of sight leans into.
    fn cloud_top(&self, u: f64, v: f64) -> f32 {
        let cells = self.cells(0);
        let (ci, cj) = (
            (u / cells.size - 0.5).floor() as i64,
            (v / cells.size - 0.5).floor() as i64,
        );
        match (cells.index(ci, cj), cells.index(ci, cj + 3)) {
            (Some(a), Some(b)) => f32::from(cells.high[a].max(cells.high[b])),
            _ => CLOUD_LEVELS,
        }
    }

    /// The clouds' premultiplied glow over a point and how much they cover, haze and shadow
    /// included (fog.fsh main, the cloudy part).
    fn cloud_cover(
        &self,
        here: Here,
        u: f64,
        v: f64,
        jitter: f32,
        known: f32,
        soft: f32,
    ) -> ([f32; 3], f32) {
        let mut handover = smoothstep(0.42, 0.58, self.mask.fraction);
        let size = self.cells(0).size;
        let mut top = CLOUD_LEVELS;
        if !here.deep && (handover <= 0.0 || handover >= 1.0) {
            top = self.cloud_top(u, v);
        }
        let (mut c, mut shadow) = if here.clear || top < 0.5 {
            handover = 0.0;
            ([0.0, 0.0, 0.0, 1.0], 0.0)
        } else if here.deep {
            (Self::flat_top(size, jitter), GROUND_SHADOW)
        } else {
            (
                self.clouds(0, u, v, jitter, top),
                GROUND_SHADOW * self.ground_shadow(0, u, v),
            )
        };
        if handover > 0.0 && handover < 1.0 {
            let (c2, shadow2) = if here.deep {
                (Self::flat_top(size * 2.0, jitter), GROUND_SHADOW)
            } else {
                (
                    self.clouds(1, u, v, jitter, CLOUD_LEVELS),
                    GROUND_SHADOW * self.ground_shadow(1, u, v),
                )
            };
            for (a, b) in c.iter_mut().zip(c2) {
                *a = mix(*a, b, handover);
            }
            shadow = mix(shadow, shadow2, handover);
        }
        let mut through = c[3];
        let halo = HALO * (1.0 - smoothstep(0.55, 1.0, soft));
        let empty = 1.0 - smoothstep(0.3, 0.85, known);
        let mist = halo.max(empty);
        let glow = std::array::from_fn(|i| c[i] + through * mist * HAZE_COLOUR[i]);
        through *= 1.0 - mist;
        (glow, 1.0 - through * (1.0 - shadow))
    }

    /// One layer texel over map point (u, v): premultiplied RGBA, the night's tint included.
    fn texel(&self, u: f64, v: f64, jitter: f32) -> [f32; 4] {
        let mask = self.mask;
        let ti = ((u - mask.origin_u) / mask.texel).floor();
        let tj = ((v - mask.origin_v) / mask.texel).floor();
        let flag = if ti < 0.0 || tj < 0.0 || ti >= mask.w as f64 || tj >= mask.h as f64 {
            0
        } else {
            mask.flags[tj as usize * mask.w + ti as usize]
        };
        let here = Here {
            clear: flag == 1,
            deep: flag == 2,
        };
        let clouds = self.kind == FogKind::Clouds;
        let lift_here = if clouds && !here.deep {
            mask.sample(&mask.lift, u, v)
        } else {
            1.0
        };
        let known_here = self.known_at(here, u, v);
        let soft = self.known_soft(here, u, v);

        let cloud = clouds.then(|| self.cloud_cover(here, u, v, jitter, known_here, soft));

        let (mut colour, mut alpha) = ([0f32; 3], 0f32);
        let (sun_u, sun_v) = (-9.0 * self.fog_scale, -3.6 * self.fog_scale);
        let height = self.height;
        for i in 0..LAYERS {
            let h = height * (1.0 - (i as f32 + 0.5 + jitter * 0.6) / LAYERS as f32);
            let gv = v + f64::from(h) * COS_PITCH;
            let th = self.thickness_at(here, u, gv, self.known_soft(here, u, gv));
            let a = smoothstep(h - 1.4, h + 1.4, th);
            if a <= 0.002 {
                continue;
            }
            let (pu, pv) = (u + sun_u, gv + sun_v);
            let probe_known = if here.clear {
                1.0
            } else {
                self.known_at(here, pu, pv)
            };
            let toward = self.thickness_at(here, pu, pv, probe_known);
            let occlusion = clamp01((toward - th) / height * 2.2);
            let shadow = if clouds { SHADOW } else { FOG_SHADOW };
            let light = clamp01(0.2 + 0.7 * (h / height) + 0.3 - occlusion * shadow);
            let layer_alpha = a * LAYER_ALPHA;
            for (c, out) in colour.iter_mut().enumerate() {
                *out += (1.0 - alpha) * layer_alpha * mix(self.shade[c], self.lit[c], light);
            }
            alpha += (1.0 - alpha) * layer_alpha;
            if alpha > 0.985 {
                break;
            }
        }

        // The ground under a thick bank is never seen.
        let floor = smoothstep(0.5, 2.5, self.thickness_at(here, u, v, soft))
            * smoothstep(0.3, 0.9, lift_here);
        for (c, out) in colour.iter_mut().enumerate() {
            *out += (1.0 - alpha) * floor * self.shade[c];
        }
        alpha += (1.0 - alpha) * floor;

        // Ground known right here stays visible: fog only rolls over its edges. Caves use the
        // sharp mask so narrow tunnels stay clear.
        let near = if self.kind == FogKind::Cave {
            known_here
        } else {
            soft
        };
        alpha *= 1.0 - 0.85 * smoothstep(0.45, 0.95, near);

        // Where there is no map at all nothing shows through, in the colour the layers have.
        let empty = (1.0 - smoothstep(0.3, 0.85, known_here)) * smoothstep(0.7, 1.0, lift_here);
        let cover: [f32; 3] = if alpha > 0.02 {
            colour.map(|c| c / alpha)
        } else {
            std::array::from_fn(|c| mix(self.shade[c], self.lit[c], 0.7))
        };
        for (c, out) in colour.iter_mut().enumerate() {
            *out += (1.0 - alpha) * empty * cover[c];
        }
        alpha += (1.0 - alpha) * empty;

        if let Some((glow, cloud_cover)) = cloud {
            // The fog rolls over the cloud terraces: its premultiplied colour over theirs.
            let a = alpha * FOG_MIX;
            for (c, out) in colour.iter_mut().enumerate() {
                *out = *out * FOG_MIX + (1.0 - a) * glow[c];
            }
            alpha = 1.0 - (1.0 - a) * (1.0 - cloud_cover);
        }
        if alpha <= 0.002 {
            return [0.0; 4];
        }
        // The shader's grain, then its colour as the render target clamps it.
        std::array::from_fn(|c| {
            if c == 3 {
                alpha
            } else {
                clamp01((colour[c] + jitter / 255.0) / alpha * self.moonlit[c]) * alpha
            }
        })
    }
}

/// Paints the fog over `grid` from `mask`, on `threads` threads: premultiplied ABGR texels of the
/// fog of `mask.kind` for map layer `layer` (fog.fsh, written into the layer as FogLayer draws it).
pub fn paint_layer(
    mask: &FogMask,
    params: &FogParams,
    grid: &LayerGrid,
    layer: i32,
    threads: usize,
) -> FogLayer {
    FogLayer {
        grid: *grid,
        pixels: paint_pixels(mask, params, grid, threads),
        flat: flat_colour(mask.kind, params.night),
        kind: mask.kind,
        layer,
        version: NEXT_VERSION.fetch_add(1, Ordering::Relaxed),
        coverage_version: 0,
    }
}

fn paint_pixels(mask: &FogMask, params: &FogParams, grid: &LayerGrid, threads: usize) -> Vec<u32> {
    let kind = mask.kind;
    let drift = params.seconds.rem_euclid(DRIFT_LOOP) / DRIFT_LOOP;
    let fog_scale = (f64::from(mask.steps) + f64::from(mask.fraction)).exp2();
    // The noise is read up to the sun probe west and north of the layer, and the height
    // layers' lean south; on the fine noise's texels.
    let (steps, fraction) = (mask.steps, mask.fraction);
    let field = Field::new(
        grid.start_u - 9.0 * fog_scale - grid.texel,
        grid.start_v - 3.6 * fog_scale - grid.texel,
        grid.start_u + (grid.w + 1) as f64 * grid.texel,
        grid.start_v + (grid.h + 1) as f64 * grid.texel + f64::from(kind.height()) * COS_PITCH,
        NOISE_PERIOD * f64::from(1u32 << steps) / NOISE as f64,
        |u, v| noise_at(noise(), steps, fraction, drift, u, v),
    );
    let painter = Painter {
        mask,
        noise: noise(),
        kind,
        lit: kind.lit(),
        shade: kind.shade(),
        height: kind.height(),
        fog_scale,
        drift,
        moonlit: moonlit(params.night),
        field,
    };
    let jitter = Jitter::new(grid.texel);
    let (w, h) = (grid.w, grid.h);
    let mut pixels = vec![0u32; w * h];
    let paint_rows = |j0: usize, rows: &mut [u32]| {
        for (dj, row) in rows.chunks_exact_mut(w).enumerate() {
            let v = grid.start_v + ((j0 + dj) as f64 + 0.5) * grid.texel;
            for (i, px) in row.iter_mut().enumerate() {
                let u = grid.start_u + (i as f64 + 0.5) * grid.texel;
                *px = pack(painter.texel(u, v, jitter.at(u, v)));
            }
        }
    };
    // Rows in bands dealt round the threads, so the costly frontier is shared.
    const BAND: usize = 4;
    let threads = threads.max(1);
    if threads == 1 || h <= BAND || w == 0 {
        paint_rows(0, &mut pixels);
    } else {
        let mut shares: Vec<Vec<(usize, &mut [u32])>> = (0..threads).map(|_| Vec::new()).collect();
        for (b, rows) in pixels.chunks_mut(BAND * w).enumerate() {
            shares[b % threads].push((b * BAND, rows));
        }
        std::thread::scope(|scope| {
            let mut shares = shares.into_iter();
            let mine = shares.next().expect("a share");
            for share in shares {
                let paint_rows = &paint_rows;
                scope.spawn(move || {
                    for (j0, rows) in share {
                        paint_rows(j0, rows);
                    }
                });
            }
            for (j0, rows) in mine {
                paint_rows(j0, rows);
            }
        });
    }
    pixels
}

// ---------------------------------------------------------------------------------------------
// Worker

/// What a view asks of the fog besides the view itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FogRequest {
    /// GUI pixels per block: past [`DETAILED_ZOOM`] the clouds grow with it ([`fog_scale`]).
    pub gui_zoom: f64,
}

/// One fog paint asked of the [`FogWorker`].
#[derive(Clone, Debug)]
pub struct FogJob {
    pub area: MaskArea,
    pub kind: FogKind,
    pub rounded: bool,
    pub steps: u32,
    pub fraction: f32,
    /// The coverage of the tiles held, and the version of what's held, which says when the
    /// mask must be built again.
    pub coverage: Vec<(TileKey, Arc<[u8]>)>,
    pub coverage_version: u64,
    pub grid: LayerGrid,
    pub params: FogParams,
}

impl FogJob {
    /// Whether the mask built for `other` serves this job too.
    fn same_mask(&self, other: &FogJob) -> bool {
        self.area == other.area
            && self.kind == other.kind
            && self.rounded == other.rounded
            && self.steps == other.steps
            && self.fraction == other.fraction
            && self.coverage_version == other.coverage_version
    }
}

struct Shared {
    job: Mutex<Option<FogJob>>,
    wake: Condvar,
    done: Mutex<Option<Arc<FogLayer>>>,
    /// The last paint's cost in microseconds.
    cost: AtomicU64,
    stop: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Paints fog layers on a thread of its own ("diorama-fog"), the latest job asked first: a job
/// asked while one paints replaces any still waiting. Nothing here waits on the paint.
pub struct FogWorker {
    shared: Arc<Shared>,
}

impl Default for FogWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl FogWorker {
    pub fn new() -> FogWorker {
        let shared = Arc::new(Shared {
            job: Mutex::new(None),
            wake: Condvar::new(),
            done: Mutex::new(None),
            cost: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("diorama-fog".into())
            .spawn(move || work(&s))
            .expect("spawn fog worker");
        FogWorker { shared }
    }

    /// Asks for a paint; one asked before that hasn't started is dropped.
    pub fn submit(&self, job: FogJob) {
        *lock(&self.shared.job) = Some(job);
        self.shared.wake.notify_one();
    }

    /// The layer painted last, if any.
    pub fn latest(&self) -> Option<Arc<FogLayer>> {
        lock(&self.shared.done).clone()
    }

    /// What the last paint cost, mask included.
    pub fn last_cost(&self) -> Duration {
        Duration::from_micros(self.shared.cost.load(Ordering::Relaxed))
    }
}

impl Drop for FogWorker {
    fn drop(&mut self) {
        // Not joined: a paint under way ends on its own, and nothing waits for it.
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_one();
    }
}

fn work(s: &Shared) {
    let mut built: Option<(FogJob, FogMask)> = None;
    loop {
        let job = {
            let mut slot = lock(&s.job);
            loop {
                if s.stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(job) = slot.take() {
                    break job;
                }
                slot = s.wake.wait(slot).unwrap_or_else(|e| e.into_inner());
            }
        };
        let start = Instant::now();
        if !built.as_ref().is_some_and(|(j, _)| j.same_mask(&job)) {
            let mask = build_mask(
                &job.coverage,
                &job.area,
                job.kind,
                job.rounded,
                job.steps,
                job.fraction,
            );
            built = Some((job.clone(), mask));
        }
        let (_, mask) = built.as_ref().expect("built");
        let mut layer = paint_layer(mask, &job.params, &job.grid, job.area.layer, PAINT_THREADS);
        layer.coverage_version = job.coverage_version;
        s.cost
            .store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        *lock(&s.done) = Some(Arc::new(layer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SURFACE: i32 = TileKey::SURFACE;

    /// Coverage of level-0 tiles from `known(u, v)` at each coverage cell's middle.
    fn coverage(
        tiles: std::ops::RangeInclusive<i32>,
        known: impl Fn(f64, f64) -> f32,
    ) -> Vec<(TileKey, Arc<[u8]>)> {
        let n = COVERAGE_CELLS as usize;
        let cell = 128.0 / n as f64;
        let mut out = Vec::new();
        for tv in tiles.clone() {
            for tu in tiles.clone() {
                let key = TileKey::new(SURFACE, tu, tv);
                let cov: Vec<u8> = (0..n * n)
                    .map(|k| {
                        let u = key.min_u() + ((k % n) as f64 + 0.5) * cell;
                        let v = key.min_v() + ((k / n) as f64 + 0.5) * cell;
                        (known(u, v).clamp(0.0, 1.0) * 255.0).round() as u8
                    })
                    .collect();
                out.push((key, Arc::from(cov)));
            }
        }
        out
    }

    fn grid(w: usize, h: usize, texel: f64) -> LayerGrid {
        LayerGrid {
            start_u: -(w as f64) * texel / 2.0,
            start_v: -(h as f64) * texel / 2.0,
            texel,
            w,
            h,
        }
    }

    fn paint(cov: &[(TileKey, Arc<[u8]>)], kind: FogKind, g: &LayerGrid) -> (FogMask, FogLayer) {
        let area = MaskArea::around(g, SURFACE, 0, kind, 0);
        let mask = build_mask(cov, &area, kind, false, 0, 0.0);
        let layer = paint_layer(
            &mask,
            &FogParams {
                night: 0.0,
                seconds: 0.0,
            },
            g,
            SURFACE,
            2,
        );
        (mask, layer)
    }

    /// An explored disc round (0, 0), in whole chunks.
    fn disc(u: f64, v: f64) -> f32 {
        let (cu, cv) = (
            (u / 16.0).floor() * 16.0 + 8.0,
            (v / 16.0).floor() * 16.0 + 8.0,
        );
        f32::from(u8::from(cu * cu + cv * cv < 150.0 * 150.0))
    }

    /// A grid straddling the blob's eastern frontier.
    fn frontier(w: usize, h: usize, texel: f64) -> LayerGrid {
        LayerGrid {
            start_u: 150.0 - w as f64 * texel / 2.0,
            ..grid(w, h, texel)
        }
    }

    /// An explored blob: a disc of chunks round (0, 0), ragged by a hash.
    fn blob(u: f64, v: f64) -> f32 {
        let (cu, cv) = ((u / 16.0).floor(), (v / 16.0).floor());
        let r = ((cu * 16.0 + 8.0).powi(2) + (cv * 16.0 + 8.0).powi(2)).sqrt();
        let wobble = hash(cu, cv) as f64 * 48.0;
        if r < 120.0 + wobble { 1.0 } else { 0.0 }
    }

    #[test]
    fn noise_is_the_same_every_time() {
        let n = noise();
        let again: Vec<f32> = fractal(NOISE, 6, 1).iter().map(|c| byte(*c)).collect();
        assert!(n.texels.iter().zip(&again).all(|(t, a)| t[0] == *a));
        // Java's Random from seed 1 starts so.
        let mut r = JavaRandom::new(1);
        assert!((r.next_float() - 0.730_878_3).abs() < 1e-6);
        // In range, and the channels differ.
        assert!(
            n.texels
                .iter()
                .all(|t| t.iter().all(|c| (0.0..=1.0).contains(c)))
        );
        assert!(n.texels.iter().any(|t| t[0] != t[1]));
        // It tiles: the last texel's neighbour is the first.
        assert_eq!(n.at(0, 1.0 + 0.5 / 256.0, 0.5 / 256.0), n.texels[0][0]);
    }

    #[test]
    fn fully_known_ground_has_no_fog() {
        let cov = coverage(-4..=3, |_, _| 1.0);
        let g = grid(64, 48, 2.0);
        let (mask, layer) = paint(&cov, FogKind::Clouds, &g);
        let at = |u: f64, v: f64| {
            let i = ((u - mask.origin_u) / mask.texel) as usize;
            let j = ((v - mask.origin_v) / mask.texel) as usize;
            j * mask.w + i
        };
        let k = at(0.0, 0.0);
        assert_eq!(mask.known[k], 1.0);
        assert_eq!(mask.flags[k], 1);
        // Only the veil fog.fsh's lowest layers leave over known ground, which the soft rule
        // cuts to a trace: under 1.5%.
        let most = layer.pixels.iter().map(|p| p >> 24).max();
        assert!(most.is_some_and(|a| a <= 3), "{most:?}");
    }

    #[test]
    fn far_unknown_ground_is_under_the_clouds() {
        // Explored round (0, 0); the layer far east of it.
        let cov = coverage(-1..=0, |_, _| 1.0);
        let g = LayerGrid {
            start_u: 2000.0,
            ..grid(32, 32, 2.0)
        };
        let (mask, layer) = paint(&cov, FogKind::Clouds, &g);
        let mid = (mask.h / 2) * mask.w + mask.w - 1;
        assert_eq!(mask.lift[mid], 1.0);
        assert_eq!(mask.flags[mid], 2);
        for p in &layer.pixels {
            assert!(p >> 24 > 242, "{p:08x}");
        }
        // And with nothing explored at all.
        let (_, layer) = paint(&[], FogKind::Clouds, &grid(16, 16, 2.0));
        assert!(layer.pixels.iter().all(|p| p >> 24 > 242));
    }

    #[test]
    fn clouds_climb_away_from_explored_land() {
        let cov = coverage(-3..=2, disc);
        let g = grid(64, 64, 4.0);
        let area = MaskArea::around(&g, SURFACE, 0, FogKind::Clouds, 0);
        let mask = build_mask(&cov, &area, FogKind::Clouds, false, 0, 0.0);
        // Along a row east from the middle: never lower further out.
        let j = ((0.0 - mask.origin_v) / mask.texel) as usize;
        let i0 = ((0.0 - mask.origin_u) / mask.texel) as usize;
        let row = &mask.lift[j * mask.w + i0..(j + 1) * mask.w];
        assert_eq!(row[0], 0.0);
        assert!(row.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(*row.last().unwrap(), 1.0);
        assert!(row.iter().any(|l| *l > 0.0 && *l < 1.0));
    }

    #[test]
    fn rounding_never_uncovers_ground() {
        let cov = coverage(-2..=1, blob);
        let g = grid(64, 64, 2.0);
        let area = MaskArea::around(&g, SURFACE, 0, FogKind::Clouds, 0);
        let sharp = build_mask(&cov, &area, FogKind::Clouds, false, 0, 0.0);
        let round = build_mask(&cov, &area, FogKind::Clouds, true, 0, 0.0);
        assert!(sharp.known.iter().zip(&round.known).all(|(s, r)| r <= s));
        assert!(sharp.known.iter().zip(&round.known).any(|(s, r)| r < s));
        // A cave is never rounded.
        let cave = build_mask(&cov, &area, FogKind::Cave, true, 0, 0.0);
        assert_eq!(cave.known, sharp.known);
    }

    #[test]
    fn caves_have_no_clouds() {
        let cov = coverage(-2..=1, blob);
        let g = frontier(48, 48, 4.0);
        let far = 47;
        let (mask, layer) = paint(&cov, FogKind::Cave, &g);
        assert!(mask.cells.iter().all(Option::is_none));
        assert!(mask.lift.iter().all(|l| *l == 1.0));
        // Cave fog is dark and opaque where nothing is known.
        let p = layer.pixels[far];
        assert_eq!(p >> 24, 255);
        assert!(p & 0xFF < 80 && (p >> 16) & 0xFF > p & 0xFF, "{p:08x}");
        // And the clouds are bright.
        let (_, clouds) = paint(&cov, FogKind::Clouds, &g);
        assert!(
            clouds.pixels[far] & 0xFF > 150,
            "{:08x}",
            clouds.pixels[far]
        );
        let (_, smoke) = paint(&cov, FogKind::Smoke, &g);
        let p = smoke.pixels[far];
        assert!(p & 0xFF > (p >> 16) & 0xFF, "smoke is warm: {p:08x}");
    }

    #[test]
    fn layers_hold_the_views_they_were_made_for() {
        let view = View {
            center_u: 1000.3,
            center_v: -77.7,
            zoom: 2.0,
            layer: SURFACE,
            lod: 0,
            width: 1024,
            height: 576,
            round: false,
        };
        let g = LayerGrid::for_view(&view);
        assert!(g.w * g.h <= 540 * 320);
        assert!(g.holds(&view));
        assert_eq!((g.start_u / g.texel).fract(), 0.0);
        let near = View {
            center_u: view.center_u + 5.0,
            ..view
        };
        assert!(g.holds(&near));
        let far = View {
            center_u: view.center_u + 50.0,
            ..view
        };
        assert!(!g.holds(&far));
        let zoomed = View { zoom: 3.0, ..view };
        assert!(!g.holds(&zoomed));
    }

    #[test]
    fn the_fog_drifts() {
        let cov = coverage(-2..=1, blob);
        let g = frontier(48, 48, 2.0);
        let area = MaskArea::around(&g, SURFACE, 0, FogKind::Clouds, 0);
        let mask = build_mask(&cov, &area, FogKind::Clouds, false, 0, 0.0);
        let at = |seconds| {
            paint_layer(
                &mask,
                &FogParams {
                    night: 0.0,
                    seconds,
                },
                &g,
                SURFACE,
                1,
            )
            .pixels
        };
        assert_ne!(at(0.0), at(30.0));
        assert_eq!(at(0.0), at(DRIFT_LOOP));
    }

    #[test]
    fn the_worker_paints_the_latest_job() {
        let worker = FogWorker::new();
        let g = grid(16, 16, 2.0);
        let job = |seconds| FogJob {
            area: MaskArea::around(&g, SURFACE, 0, FogKind::Clouds, 0),
            kind: FogKind::Clouds,
            rounded: false,
            steps: 0,
            fraction: 0.0,
            coverage: Vec::new(),
            coverage_version: 0,
            grid: g,
            params: FogParams {
                night: 0.0,
                seconds,
            },
        };
        worker.submit(job(1.0));
        worker.submit(job(2.0));
        let deadline = Instant::now() + Duration::from_secs(30);
        while worker.latest().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let layer = worker.latest().unwrap();
        assert_eq!(layer.grid, g);
        assert_eq!(layer.pixels.len(), 256);
    }

    /// The synthetic ground under the PNG: green and brown, by chunk.
    fn ground(u: f64, v: f64) -> [f32; 3] {
        let n = noise().at(3, u / 300.0, v / 300.0);
        let h = hash((u / 4.0).floor(), (v / 4.0).floor()) * 0.08;
        if n > 0.62 {
            [0.62 + h, 0.55 + h, 0.38 + h]
        } else {
            [0.30 + h, 0.55 + h * 2.0, 0.22 + h]
        }
    }

    /// Paints a half-explored area to `diorama_fog.png` in the temp dir, for looking at:
    /// `cargo test --release -- --ignored fog_png`.
    #[test]
    #[ignore]
    fn fog_png() {
        let (w, h) = (1024usize, 576usize);
        let shapes: [(&str, FogKind, f64); 4] = [
            ("diorama_fog.png", FogKind::Clouds, 2.0),
            ("diorama_fog_far.png", FogKind::Clouds, 0.4),
            ("diorama_fog_smoke.png", FogKind::Smoke, 2.0),
            ("diorama_fog_cave.png", FogKind::Cave, 2.0),
        ];
        for (name, kind, zoom) in shapes {
            let view = View {
                center_u: 40.0,
                center_v: 10.0,
                zoom,
                layer: SURFACE,
                lod: 0,
                width: w,
                height: h,
                round: false,
            };
            let explored = |u: f64, v: f64| {
                if kind == FogKind::Cave {
                    // Tunnels.
                    let t = ((u / 40.0).sin() * 30.0 - v).abs() < 6.0 || (u - 60.0).abs() < 5.0;
                    f32::from(u8::from(t && u.abs() < 300.0))
                } else {
                    blob(u, v)
                }
            };
            let cov = coverage(-12..=12, explored);
            let g = LayerGrid::for_view(&view);
            let (steps, fraction) = fog_scale(zoom / 2.0);
            let area = MaskArea::around(&g, SURFACE, 0, kind, steps);
            let mask = build_mask(&cov, &area, kind, kind == FogKind::Clouds, steps, fraction);
            let layer = paint_layer(
                &mask,
                &FogParams {
                    night: 0.0,
                    seconds: 0.0,
                },
                &g,
                SURFACE,
                PAINT_THREADS,
            );
            let backdrop = match kind {
                FogKind::Clouds => [0x8C, 0x99, 0xAB],
                FogKind::Smoke => [0x29, 0x21, 0x1F],
                FogKind::Cave => [0x0C, 0x0E, 0x12],
            }
            .map(|c| c as f32 / 255.0);
            let mut rgba = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let (u, v) = view.map_uv(x as f64 + 0.5, y as f64 + 0.5);
                    let k = mask.sample(&mask.known, u, v).min(explored(u, v));
                    let gr = ground(u, v);
                    let fx = (u - g.start_u) / g.texel - 0.5;
                    let fy = (v - g.start_v) / g.texel - 0.5;
                    let (i0, j0) = (fx.floor() as usize, fy.floor() as usize);
                    let (tx, ty) = ((fx - fx.floor()) as f32, (fy - fy.floor()) as f32);
                    let t = |i: usize, j: usize, c: usize| {
                        ((layer.pixels[j * g.w + i] >> (8 * c)) & 0xFF) as f32 / 255.0
                    };
                    let f = |c: usize| {
                        mix(
                            mix(t(i0, j0, c), t(i0 + 1, j0, c), tx),
                            mix(t(i0, j0 + 1, c), t(i0 + 1, j0 + 1, c), tx),
                            ty,
                        )
                    };
                    let fa = f(3);
                    for c in 0..3 {
                        let base = gr[c] * k + backdrop[c] * (1.0 - k);
                        rgba.push(((f(c).min(fa) + base * (1.0 - fa)) * 255.0).round() as u8);
                    }
                    rgba.push(255);
                }
            }
            let path = std::env::temp_dir().join(name);
            let file = std::fs::File::create(&path).expect("create png");
            let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header()
                .expect("header")
                .write_image_data(&rgba)
                .expect("data");
            println!("wrote {}", path.display());
        }
    }

    /// One layer for the largest image, half explored, on [`PAINT_THREADS`] threads:
    /// `cargo test --release -- --ignored fog_speed --nocapture`.
    #[test]
    #[ignore]
    fn fog_speed() {
        let cov = coverage(-12..=12, blob);
        for zoom in [2.0, 1.0, 0.4] {
            let view = View {
                center_u: 40.0,
                center_v: 10.0,
                zoom,
                layer: SURFACE,
                lod: 0,
                width: 1024,
                height: 576,
                round: false,
            };
            let g = LayerGrid::for_view(&view);
            let (steps, fraction) = fog_scale(zoom / 2.0);
            let area = MaskArea::around(&g, SURFACE, 0, FogKind::Clouds, steps);
            let params = FogParams {
                night: 0.0,
                seconds: 3.0,
            };
            let start = Instant::now();
            let mask = build_mask(&cov, &area, FogKind::Clouds, true, steps, fraction);
            let built = start.elapsed();
            let runs = 5;
            let start = Instant::now();
            for _ in 0..runs {
                paint_layer(&mask, &params, &g, SURFACE, PAINT_THREADS);
            }
            let each = start.elapsed() / runs;
            println!(
                "zoom {zoom}: layer {}x{}, mask {}x{} built in {built:.2?}, painted in {each:.2?}",
                g.w, g.h, mask.w, mask.h
            );
            assert!(built + each < Duration::from_millis(120));
        }
    }
}
