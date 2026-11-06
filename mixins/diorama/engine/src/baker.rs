//! Ray traces one map tile through the block data.
//!
//! Every texture pixel fires four rays along the camera direction. Each ray walks the blocks in
//! its column (the camera faces due north, so a ray never changes x) until it hits something solid.
//! That hit is then lit: in the surface view by the sun, with a second ray toward it for shadows;
//! in a cave slice by the game's own block and sky light. Under a roof (the Nether) it is lit from
//! above as if the rock over the first open space were lifted off, and fades into the biome's fog
//! the lower it lies. Corner ambient occlusion and partial colour from plants, glass and water come
//! on top.

use crate::lighting::{self, luminance};
use crate::looks::{BlockLooks, Kind, TintKind};
use crate::projection::{PIXELS_PER_UNIT, Projection, TILE_PIXELS, TILE_UNITS};
use crate::reader::{UNKNOWN, VoxelReader};
use crate::source::ChunkSource;
use crate::style::MapStyle;
use crate::tile::{TileImage, TileKey};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Rotated-grid sample offsets inside a pixel.
pub const SAMPLES: [[f64; 2]; 4] = [
    [0.375, 0.125],
    [0.875, 0.375],
    [0.125, 0.625],
    [0.625, 0.875],
];
/// One sample in the middle of the pixel: a quarter of the work, with jaggier block edges.
pub const FAST_SAMPLES: [[f64; 2]; 1] = [[0.5, 0.5]];

/// Under a roof: light filling shadows, direct light from above, and the block light glow.
const LID_AMBIENT: f32 = 0.26;
const LID_KEY: f32 = 0.52;
const LID_KEY_TINT: [f32; 3] = [1.1, 1.0, 0.88];
const LID_FILL_TINT: [f32; 3] = [1.0, 0.86, 0.84];
const LID_GLOW: f32 = 0.7;
/// The Nether's fog starts this far below the roof.
const HAZE_START: i32 = 30;
const HAZE_DEPTH: f64 = 55.0;
const HAZE_MAX: f32 = 0.72;
const HAZE_LIGHT: f32 = 1.6;
const HAZE_MAX_LUMINANCE: f32 = 0.025;

/// The sun is in the west, slightly south, so shadows fall to the east; how high is per style.
pub const SUN_AZIMUTH_X: f64 = -0.878;
pub const SUN_AZIMUTH_Z: f64 = 0.479;
/// Small per-sample nudges of the sun direction, which soften shadow edges.
const SUN_JITTER: [[f64; 3]; 4] = [
    [0.03, 0.0, -0.02],
    [-0.02, 0.0, 0.03],
    [0.02, 0.0, 0.02],
    [-0.03, 0.0, -0.03],
];

/// How far sky occlusion looks for something overhead, in blocks.
pub const SKY_REACH: i32 = 12;
const AO_LEVELS: [f32; 4] = [0.6, 0.74, 0.87, 1.0];
/// Faces that aren't up read darker, the way Minecraft shades its own block sides.
const SIDE: f32 = 0.82;
/// Outlines on the floor under water fade with the water in front.
const OUTLINE_WATER_FADE: f32 = 6.0;
/// How much nearer a neighbouring pixel has to be, in blocks, before an outline is drawn.
const OUTLINE_DEPTH: f32 = 1.5;
/// How far below the cut a cave slice looks.
pub const CAVE_DEPTH: i32 = 48;
const SHADOW_STEPS: usize = 96;
/// Longest shadow, in blocks along the sun direction. Bounds how far an edit can reach.
pub const SHADOW_DISTANCE: f64 = 40.0;

/// How far east, north and down from a block its shadow can land, for the lowest and highest sun.
pub fn shadow_reach() -> (i32, i32, i32) {
    let low = MapStyle::MIN_SUN_ELEVATION.to_radians();
    let high = MapStyle::MAX_SUN_ELEVATION.to_radians();
    (
        (SHADOW_DISTANCE * low.cos() * -SUN_AZIMUTH_X).ceil() as i32 + 1,
        (SHADOW_DISTANCE * low.cos() * SUN_AZIMUTH_Z).ceil() as i32 + 1,
        (SHADOW_DISTANCE * high.sin()).ceil() as i32 + 1,
    )
}

struct Tables {
    srgb_to_linear: [f32; 256],
    linear_to_srgb: Vec<u8>,
    sky_rays: Vec<[f64; 3]>,
    sky_weights: Vec<f64>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut srgb_to_linear = [0f32; 256];
        for (i, v) in srgb_to_linear.iter_mut().enumerate() {
            let c = i as f64 / 255.0;
            *v = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            } as f32;
        }
        let linear_to_srgb = (0..4096)
            .map(|i| {
                let v = i as f64 / 4095.0;
                let s = if v <= 0.0031308 {
                    v * 12.92
                } else {
                    1.055 * v.powf(1.0 / 2.4) - 0.055
                };
                (s.clamp(0.0, 1.0) * 255.0).round() as u8
            })
            .collect();
        // Two rings round the sky and one straight up, weighted by how much light each part of
        // the sky sends onto flat ground.
        let ring = 6;
        let mut sky_rays = Vec::new();
        let mut sky_weights = Vec::new();
        for (r, e) in [25f64.to_radians(), 55f64.to_radians()]
            .into_iter()
            .enumerate()
        {
            for i in 0..ring {
                let azimuth =
                    (i as f64 + r as f64 * 0.5) * 2.0 * std::f64::consts::PI / ring as f64;
                sky_rays.push([e.cos() * azimuth.cos(), e.sin(), e.cos() * azimuth.sin()]);
                sky_weights.push(e.sin());
            }
        }
        sky_rays.push([0.0, 1.0, 0.0]);
        sky_weights.push(1.5);
        Tables {
            srgb_to_linear,
            linear_to_srgb,
            sky_rays,
            sky_weights,
        }
    })
}

#[inline]
fn lin(c: u32, shift: u32) -> f32 {
    tables().srgb_to_linear[((c >> shift) & 0xFF) as usize]
}

#[inline]
fn to_srgb(v: f32) -> u32 {
    u32::from(tables().linear_to_srgb[((v * 4095.0) as i32).clamp(0, 4095) as usize])
}

/// Linear light graded by the style, as an sRGB ABGR pixel.
pub fn abgr(style: &MapStyle, r: f32, g: f32, b: f32, alpha: u32) -> u32 {
    let l = luminance(r, g, b);
    let grade = |c: f32| (l + (c - l) * style.saturation) * style.exposure;
    (alpha << 24) | (to_srgb(grade(b)) << 16) | (to_srgb(grade(g)) << 8) | to_srgb(grade(r))
}

/// -1..1, fixed per block position.
pub fn block_noise(x: i32, y: i32, z: i32) -> f32 {
    let mut h =
        x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ z.wrapping_mul(83_492_791);
    h ^= ((h as u32) >> 13) as i32;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= ((h as u32) >> 15) as i32;
    (h & 0xFFFF) as f32 / 32767.5 - 1.0
}

fn corner(side1: bool, side2: bool, diagonal: bool) -> f32 {
    if side1 && side2 {
        return AO_LEVELS[0];
    }
    AO_LEVELS[3 - side1 as usize - side2 as usize - diagonal as usize]
}

/// `a b` on the first row, `c d` on the second.
fn bilinear(a: f32, b: f32, c: f32, d: f32, fx: f64, fy: f64) -> f32 {
    let top = a as f64 + (b - a) as f64 * fx;
    let bottom = c as f64 + (d - c) as f64 * fx;
    (top + (bottom - top) * fy) as f32
}

fn darken(abgr: u32, keep: f32) -> u32 {
    let ch =
        |shift: u32| ((((abgr >> shift) & 0xFF) as f32 * keep).round() as u32).min(255) << shift;
    (abgr & 0xFF00_0000) | ch(16) | ch(8) | ch(0)
}

/// Bake settings that don't change from tile to tile.
#[derive(Clone, Debug)]
pub struct BakeSettings {
    pub style: MapStyle,
    pub projection: Projection,
    /// One sample per pixel instead of four.
    pub fast: bool,
    /// Cave slices only show blocks the player has seen.
    pub fog_of_war: bool,
}

/// What one trace found, in linear light.
#[derive(Default, Clone, Copy)]
struct Hit {
    r: f32,
    g: f32,
    b: f32,
    /// Camera depth relative to the tile's origin; larger is farther.
    depth: f32,
    /// Blocks of water looked through.
    water: f32,
}

pub struct Baker<'a> {
    key: TileKey,
    looks: &'a BlockLooks,
    reader: VoxelReader<'a>,
    source: &'a dyn ChunkSource,
    style: &'a MapStyle,
    sin: f64,
    cos: f64,
    inv_sin: f64,
    inv_cos: f64,
    sun: [f64; 3],
    lit: bool,
    lifted: bool,
    fog_of_war: bool,
    fast: bool,
    pub start_y: i32,
    pub limit_y: i32,
    ray_top: i32,
    depth_origin: f64,
    lid_at: HashMap<(i32, i32), i32>,
    sky_at: HashMap<(i32, i32, i32), f32>,
    // The last albedo, linear.
    albedo: [f32; 3],
    // Where the last enter_shape met the boxes.
    shape_t: f64,
    shape_top: bool,
}

impl<'a> Baker<'a> {
    pub fn new(
        key: TileKey,
        source: &'a dyn ChunkSource,
        looks: &'a BlockLooks,
        settings: &'a BakeSettings,
    ) -> Self {
        let p = settings.projection;
        let mut reader = VoxelReader::new(source, looks, p.facing);
        let cave = key.is_cave();
        let roof = source.roof();
        let roofless = !cave && roof.is_some();
        let lit = cave || roofless;
        let elevation = settings.style.sun_elevation.to_radians();
        let top_y = local_top(&key, &mut reader, &p, 0, source.max_y().min(source.top_y()));
        let ray_top = local_top(
            &key,
            &mut reader,
            &p,
            SKY_REACH,
            source.max_y().min(source.top_y()),
        );
        let start_y = if cave {
            key.layer.min(source.max_y())
        } else if let Some(roof) = roof {
            roof.min(source.max_y())
        } else {
            top_y
        };
        let origin_z = (key.min_v() + f64::from(start_y) * p.cos) / p.sin;
        let depth_origin = -(p.sin * f64::from(start_y) + p.cos * origin_z);
        let limit_y = if cave {
            source.min_y().max(start_y - CAVE_DEPTH)
        } else {
            source.min_y()
        };
        Self {
            key,
            looks,
            reader,
            source,
            style: &settings.style,
            sin: p.sin,
            cos: p.cos,
            inv_sin: 1.0 / p.sin,
            inv_cos: 1.0 / p.cos,
            sun: [
                elevation.cos() * SUN_AZIMUTH_X,
                elevation.sin(),
                elevation.cos() * SUN_AZIMUTH_Z,
            ],
            lit,
            lifted: lit && roof.is_some(),
            fog_of_war: lit && settings.fog_of_war,
            fast: settings.fast,
            start_y,
            limit_y,
            ray_top,
            depth_origin,
            lid_at: HashMap::new(),
            sky_at: HashMap::new(),
            albedo: [0.0; 3],
            shape_t: 0.0,
            shape_top: false,
        }
    }

    /// Bakes a whole tile.
    pub fn bake(
        key: TileKey,
        source: &dyn ChunkSource,
        looks: &BlockLooks,
        settings: &BakeSettings,
    ) -> TileImage {
        let mut baker = Baker::new(key, source, looks, settings);
        let size = TILE_PIXELS as usize;
        let pixels = baker.bake_pixels(0, 0, size, size);
        TileImage { key, pixels }
    }

    /// How many chunks this bake asked for and the source didn't have.
    pub fn missing(&self) -> usize {
        self.reader.missing
    }

    /// The world position (turned) where light rays treat the column's rock as open.
    fn lid(&mut self, x: i32, z: i32) -> i32 {
        if let Some(l) = self.lid_at.get(&(x, z)) {
            return *l;
        }
        let l = self.reader.lid(x, z, self.start_y, self.limit_y);
        self.lid_at.insert((x, z), l);
        l
    }

    /// Traces pixels `(x0, y0)` to `(x0 + width, y0 + height)` of the tile.
    pub fn bake_pixels(&mut self, px0: usize, py0: usize, width: usize, height: usize) -> Vec<u32> {
        let mut out = vec![0u32; width * height];
        if self.start_y <= self.limit_y {
            return out;
        }
        // Outlines compare each pixel with its neighbours, so bake a one-pixel border around the area.
        let border = usize::from(self.style.outline > 0.0);
        let w = width + 2 * border;
        let h = height + 2 * border;
        let mut pixels = vec![0u32; w * h];
        let mut depth = vec![f32::NAN; w * h];
        let mut water = vec![0f32; w * h];
        let (min_u, min_v) = (self.key.min_u(), self.key.min_v());
        let unit = 1.0 / f64::from(PIXELS_PER_UNIT);
        let samples: &[[f64; 2]] = if self.fast { &FAST_SAMPLES } else { &SAMPLES };
        for y in 0..h {
            for x in 0..w {
                let (mut r, mut g, mut b, mut d, mut wd) = (0f32, 0f32, 0f32, 0f32, 0f32);
                let mut known = 0u32;
                for (s, off) in samples.iter().enumerate() {
                    let u = min_u + ((px0 + x) as f64 - border as f64 + off[0]) * unit;
                    let v = min_v + ((py0 + y) as f64 - border as f64 + off[1]) * unit;
                    if let Some(hit) = self.trace(u, v, s) {
                        r += hit.r;
                        g += hit.g;
                        b += hit.b;
                        d += hit.depth;
                        wd += hit.water;
                        known += 1;
                    }
                }
                if known > 0 {
                    let k = known as f32;
                    pixels[y * w + x] = abgr(
                        self.style,
                        r / k,
                        g / k,
                        b / k,
                        known * 255 / samples.len() as u32,
                    );
                    depth[y * w + x] = d / k;
                    water[y * w + x] = wd / k;
                }
            }
        }
        if border == 0 {
            return pixels;
        }
        let nearer = |n: f32, here: f32| here - n > OUTLINE_DEPTH;
        for y in 0..height {
            for x in 0..width {
                let i = (y + 1) * w + x + 1;
                let mut pixel = pixels[i];
                let here = depth[i];
                if pixel != 0
                    && (nearer(depth[i - 1], here)
                        || nearer(depth[i + 1], here)
                        || nearer(depth[i - w], here)
                        || nearer(depth[i + w], here))
                {
                    // The seafloor's outlines show through shallow water and fade out deeper down.
                    let outline = (-water[i] / OUTLINE_WATER_FADE).exp();
                    pixel = darken(pixel, 1.0 - self.style.outline * outline);
                }
                out[y * width + x] = pixel;
            }
        }
        out
    }

    /// Traces one ray. None when it ran into a chunk the map has no data for.
    fn trace(&mut self, u: f64, v: f64, sample: usize) -> Option<Hit> {
        let (sin, cos) = (self.sin, self.cos);
        let x = u.floor() as i32;
        let fx = u - f64::from(x);
        let top = self.start_y;
        let y0 = f64::from(top);
        let z0 = (v + y0 * cos) / sin;
        let mut by = top - 1;
        let mut bz = z0.floor() as i32;
        let mut t_max_y = (y0 - f64::from(by)) / sin;
        let mut t_max_z = (z0 - f64::from(bz)) / cos;
        let t_delta_y = 1.0 / sin;
        let t_delta_z = 1.0 / cos;
        let mut t = 0.0;
        let mut entered_top = true;
        let mut skipping_rock = self.lit;
        // Colour picked up from plants and glass on the way down, applied to the surface below.
        let (mut over_r, mut over_g, mut over_b) = (0f32, 0f32, 0f32);
        let mut through = 1f32;
        // Water the ray passed through.
        let mut water: Option<(u32, f64, i32, i32)> = None;

        macro_rules! step {
            () => {
                if t_max_y < t_max_z {
                    by -= 1;
                    t = t_max_y;
                    t_max_y += t_delta_y;
                    entered_top = true;
                } else {
                    bz -= 1;
                    t = t_max_z;
                    t_max_z += t_delta_z;
                    entered_top = false;
                }
            };
        }

        while by >= self.limit_y {
            if skipping_rock {
                let solid = self.reader.solid(x, by, bz);
                if solid == UNKNOWN {
                    return None;
                }
                if solid > 0
                    || self.lifted && by + 1 < self.start_y && self.reader.pocket(x, by, bz)
                {
                    // Still inside the rock above the first open space below the cut.
                    step!();
                    continue;
                }
                skipping_rock = false;
            }
            let id = self.reader.state(x, by, bz)?;
            let mut kind = self.looks.kind(id);
            // Only solid blocks (and the see-through ones among them) have shapes.
            let shape = if matches!(kind, Kind::Solid | Kind::Glass) {
                self.looks.shape[id as usize]
            } else {
                0
            };
            let partial = shape != 0;
            if partial {
                let t_in = t;
                let hit = self.enter_shape(
                    shape,
                    fx,
                    y0,
                    z0,
                    by,
                    bz,
                    t,
                    t_max_y.min(t_max_z),
                    entered_top,
                );
                // Waterlogged: the ray is in water from where it came into the cell, unless it met
                // the block right there, as on a fence top level with the water.
                if let Some(wet) = self.looks.wet_shape[id as usize]
                    && water.is_none()
                    && (!hit || self.shape_t > t_in)
                {
                    water = Some((wet, t_in, by, bz));
                }
                if hit {
                    t = self.shape_t;
                    entered_top = self.shape_top;
                } else {
                    // The ray passes beside the block's boxes, on into the next cell.
                    kind = Kind::Air;
                }
            }
            match kind {
                Kind::Solid | Kind::Lava => {
                    let lid = if self.lifted && kind == Kind::Solid {
                        Some(self.lid(x, bz))
                    } else {
                        None
                    };
                    if let Some(lid) = lid
                        && self.reader.ceiling_underside(x, by, bz, lid, self.limit_y)
                    {
                        // A ray that left the rock near a step in a stepped ceiling would clip the
                        // step below it and draw a piece of ceiling floating over the floor.
                    } else {
                        if self.fog_of_war
                            && !self.seen(x, by, bz)
                            && !(if entered_top {
                                self.seen(x, by + 1, bz)
                            } else {
                                self.seen(x, by, bz + 1)
                            })
                        {
                            return None;
                        }
                        let hit_y = y0 - t * sin;
                        let hit_z = z0 - t * cos;
                        let mut out = self.shade(
                            id,
                            x,
                            by,
                            bz,
                            fx,
                            hit_y,
                            hit_z,
                            entered_top,
                            partial,
                            sample,
                        );
                        out.depth = (-(sin * hit_y + cos * hit_z) - self.depth_origin) as f32;
                        if through < 1.0 {
                            // The overlay tints the albedo; approximate by mixing it in after lighting.
                            let light = luminance(out.r, out.g, out.b)
                                / luminance(self.albedo[0], self.albedo[1], self.albedo[2])
                                    .max(0.05);
                            out.r = out.r * through + over_r * light;
                            out.g = out.g * through + over_g * light;
                            out.b = out.b * through + over_b * light;
                        }
                        if let Some((wid, wt, wby, wbz)) = water {
                            self.mix_water(&mut out, wid, x, wby, wbz, u, y0, z0, wt, t, sample);
                        }
                        return Some(out);
                    }
                }
                Kind::Water => {
                    if water.is_none() {
                        water = Some((id, t, by, bz));
                    }
                }
                Kind::Plant | Kind::Glass => {
                    let w = if kind == Kind::Plant { 0.4 } else { 0.25 };
                    self.set_albedo(id, x, bz, true);
                    over_r += through * w * self.albedo[0];
                    over_g += through * w * self.albedo[1];
                    over_b += through * w * self.albedo[2];
                    through *= 1.0 - w;
                }
                Kind::Air => {}
            }
            step!();
        }

        if self.fog_of_war {
            match water {
                Some((_, _, wby, wbz)) if !skipping_rock && self.seen(x, wby, wbz) => {}
                _ => return None,
            }
        }
        let mut out = Hit {
            depth: (-(sin * (y0 - t * sin) + cos * (z0 - t * cos)) - self.depth_origin) as f32,
            ..Hit::default()
        };
        if skipping_rock {
            // Solid rock all the way down the slice: draw the cut face.
            (out.r, out.g, out.b) = (0.0065, 0.0068, 0.0085);
        } else if let Some((wid, wt, wby, wbz)) = water {
            self.mix_water(&mut out, wid, x, wby, wbz, u, y0, z0, wt, wt + 64.0, sample);
        } else {
            (out.r, out.g, out.b) = (0.01, 0.01, 0.015);
        }
        Some(out)
    }

    fn seen(&self, x: i32, y: i32, z: i32) -> bool {
        let f = self.reader.facing;
        self.source
            .is_seen(f.world_block_x(x, z), y, f.world_block_z(x, z))
    }

    /// Sets `albedo` to the block's linear colour for its top or front face at this column.
    fn set_albedo(&mut self, id: u32, x: i32, z: i32, top: bool) {
        let l = self.looks;
        let i = id as usize;
        let (base, tinted, w) = if top {
            (l.top[i], l.top_tinted[i], l.top_tint_weight[i])
        } else {
            (l.side[i], l.side_tinted[i], l.side_tint_weight[i])
        };
        let tint = match l.tint[i] {
            TintKind::None => l.constant_tint[i].unwrap_or(0xFFFFFF),
            k => self.reader.tint(k, x, z),
        };
        let bw = 1.0 - w;
        self.albedo = [
            lin(base, 16) * bw + lin(tinted, 16) * lin(tint, 16) * w,
            lin(base, 8) * bw + lin(tinted, 8) * lin(tint, 8) * w,
            lin(base, 0) * bw + lin(tinted, 0) * lin(tint, 0) * w,
        ];
    }

    #[allow(clippy::too_many_arguments)]
    fn shade(
        &mut self,
        id: u32,
        x: i32,
        by: i32,
        bz: i32,
        fx: f64,
        hit_y: f64,
        hit_z: f64,
        top: bool,
        partial: bool,
        sample: usize,
    ) -> Hit {
        self.set_albedo(id, x, bz, top);
        let style = self.style;
        if style.block_variation > 0.0 {
            let nudge = 1.0 + style.block_variation * block_noise(x, by, bz);
            self.albedo.iter_mut().for_each(|c| *c *= nudge);
        }
        // The open cell in front of the face the ray hit.
        let front_y = if top { by + 1 } else { by };
        let front_z = if top { bz } else { bz + 1 };
        let ao = if top {
            self.ao_top(x, by + 1, bz, fx, hit_z - f64::from(bz))
        } else {
            self.ao_south(x, by, bz + 1, fx, hit_y - f64::from(by))
        };
        let emission = f32::from(self.looks.emission[id as usize]) / 15.0;
        let lava = self.looks.kind(id) == Kind::Lava;
        if lava {
            // In the Nether's fog a lava sea reads as one glow rather than a field of speckles.
            let crust = 1.0 + (if self.lifted { 0.1 } else { 0.3 }) * block_noise(x, by, bz);
            self.albedo[0] *= crust;
            self.albedo[1] *= crust * crust;
            self.albedo[2] *= crust;
        }
        let ao_sun = (1.0 - style.corner_occlusion * (1.0 - ao)).max(0.0);
        let ao_sky = ao.min(ao_sun);
        let j = SUN_JITTER[sample % 4];
        let (sx, sy, sz) = (self.sun[0] + j[0], self.sun[1] + j[1], self.sun[2] + j[2]);

        let light: [f32; 3] = if self.lifted {
            // Lit like the surface with the lid off: direct light from above casts shadows under
            // ledges and down the faces of pillars. Block light glows on top.
            let lambert = if top { 1.0 } else { sz.max(0.0) / sy };
            let mut direct = 0.0;
            if lambert > 0.0 {
                let ox = f64::from(x) + fx;
                let oy = if top { f64::from(by + 1) + 1e-4 } else { hit_y };
                let oz = if top { hit_z } else { f64::from(bz + 1) + 1e-4 };
                direct =
                    if self.blocked(ox, oy, oz, sx, sy, sz, SHADOW_DISTANCE, SHADOW_STEPS, true) {
                        0.0
                    } else {
                        (f64::from(LID_KEY) * lambert) as f32 * ao_sun
                    };
            }
            let sky = if top {
                self.sky_top(x, by + 1, bz, fx, hit_z - f64::from(bz))
            } else {
                self.sky_south(x, by, bz + 1, fx, hit_y - f64::from(by))
            };
            let ambient =
                LID_AMBIENT * ao_sky * (1.0 - style.sky_occlusion * (1.0 - (0.15 + 0.85 * sky)));
            let warm = (f32::from(self.reader.block_light(x, front_y, front_z)) / 15.0).powf(1.5)
                * LID_GLOW;
            let mut l = [
                ambient * LID_FILL_TINT[0] + direct * LID_KEY_TINT[0] + warm * 0.9 * ao,
                ambient * LID_FILL_TINT[1] + direct * LID_KEY_TINT[1] + warm * 0.75 * ao,
                ambient * LID_FILL_TINT[2] + direct * LID_KEY_TINT[2] + warm * 0.5 * ao,
            ];
            if emission > 0.0 {
                // In the fog, small lights glow softer so they don't speckle; lava keeps its glow.
                let e = if !lava {
                    0.3 + emission * 0.45
                } else {
                    0.35 + emission * 0.85
                };
                l.iter_mut().for_each(|c| *c = c.max(e));
            }
            if !top {
                l.iter_mut().for_each(|c| *c *= SIDE);
            }
            l
        } else if self.lit {
            // A cave slice. Deeper below the cut reads darker.
            let bl = f32::from(self.reader.block_light(x, front_y, front_z));
            let sl = self.reader.sky_light(x, front_y, front_z);
            let fade = 1.0 - ((self.start_y as f64 - hit_y) as f32 / 60.0).min(0.5);
            lighting::cave(bl, sl, ao, fade, emission, if top { 1.0 } else { SIDE })
        } else {
            // Relative to a top face, so tops stay as bright whatever the sun's height.
            let lambert = if top { 1.0 } else { sz.max(0.0) / sy };
            // A partial block's face can sit inside its cell; the shadow ray starts on the face.
            let shadowed = lambert > 0.0 && {
                let ox = f64::from(x) + fx;
                let oy = if top {
                    (if partial { hit_y } else { f64::from(by + 1) }) + 1e-4
                } else {
                    hit_y
                };
                let oz = if top {
                    hit_z
                } else {
                    (if partial { hit_z } else { f64::from(bz + 1) }) + 1e-4
                };
                self.blocked(ox, oy, oz, sx, sy, sz, SHADOW_DISTANCE, SHADOW_STEPS, true)
            };
            let sky = if style.sky_occlusion > 0.0 {
                if top {
                    self.sky_top(x, by + 1, bz, fx, hit_z - f64::from(bz))
                } else {
                    self.sky_south(x, by, bz + 1, fx, hit_y - f64::from(by))
                }
            } else {
                1.0
            };
            lighting::surface(
                style,
                lambert,
                shadowed,
                ao,
                sky,
                emission,
                if top { 1.0 } else { SIDE },
                hit_y,
            )
        };
        let mut out = Hit {
            r: self.albedo[0] * light[0],
            g: self.albedo[1] * light[1],
            b: self.albedo[2] * light[2],
            ..Hit::default()
        };
        if self.lifted
            && let Some(roof) = self.source.roof()
        {
            // The lower the ground, the more of the biome's fog lies in front.
            let below = f64::from(roof - HAZE_START) - hit_y;
            let f = if below <= 0.0 {
                0.0
            } else {
                (1.0 - (-below / HAZE_DEPTH).exp()).min(f64::from(HAZE_MAX)) as f32
            };
            if f > 0.0 {
                let haze = self.haze(f64::from(x) + fx, hit_z);
                out.r += (haze[0] - out.r) * f;
                out.g += (haze[1] - out.g) * f;
                out.b += (haze[2] - out.b) * f;
            }
        }
        out
    }

    /// The biome fog colour at a point, linear, blended between biome cells so borders don't step,
    /// and kept no brighter than [`HAZE_MAX_LUMINANCE`] before [`HAZE_LIGHT`].
    fn haze(&mut self, x: f64, z: f64) -> [f32; 3] {
        let qx = x / 4.0 - 0.5;
        let qz = z / 4.0 - 0.5;
        let (x0, z0) = (qx.floor() as i32, qz.floor() as i32);
        let (fx, fz) = (qx - f64::from(x0), qz - f64::from(z0));
        let mut c = [0f32; 3];
        for i in 0..4 {
            let cx = x0 + (i & 1);
            let cz = z0 + (i >> 1);
            let w = (if i & 1 == 0 { 1.0 - fx } else { fx })
                * (if i >> 1 == 0 { 1.0 - fz } else { fz });
            let fog = self.fog_cell(cx * 4 + 2, cz * 4 + 2);
            c[0] += (w * f64::from(lin(fog, 16))) as f32;
            c[1] += (w * f64::from(lin(fog, 8))) as f32;
            c[2] += (w * f64::from(lin(fog, 0))) as f32;
        }
        let scale =
            HAZE_LIGHT * (HAZE_MAX_LUMINANCE / luminance(c[0], c[1], c[2]).max(1e-6)).min(1.0);
        c.map(|v| v * scale)
    }

    fn fog_cell(&mut self, x: i32, z: i32) -> u32 {
        const NETHER_WASTES_FOG: u32 = 0x330808;
        let f = self.reader.facing;
        let (wx, wz) = (f.world_block_x(x, z), f.world_block_z(x, z));
        self.source
            .chunk(wx >> 4, wz >> 4)
            .map_or(NETHER_WASTES_FOG, |c| {
                c.tints.fog[(((wz & 15) << 4) | (wx & 15)) as usize]
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn mix_water(
        &mut self,
        out: &mut Hit,
        water_id: u32,
        x: i32,
        by: i32,
        bz: i32,
        u: f64,
        y0: f64,
        z0: f64,
        t_entry: f64,
        t_hit: f64,
        sample: usize,
    ) {
        self.set_albedo(water_id, x, bz, true);
        let [wr, wg, wb] = self.albedo;
        let style = self.style;
        let light = if self.lit {
            lighting::cave_water(
                f32::from(self.reader.block_light(x, by + 1, bz)),
                self.reader.sky_light(x, by + 1, bz),
            )
        } else {
            let sy = self.sun[1] + SUN_JITTER[sample % 4][1];
            let ox = u;
            let oy = y0 - t_entry * self.sin + 1e-4;
            let oz = z0 - t_entry * self.cos;
            let shadow = self.blocked(
                ox,
                oy,
                oz,
                self.sun[0],
                sy,
                self.sun[2],
                SHADOW_DISTANCE,
                SHADOW_STEPS,
                true,
            );
            let mut ambient = style.ambient;
            if style.sky_occlusion > 0.0 {
                let sky = self.sky_top(x, by + 1, bz, u - f64::from(x), oz - f64::from(bz));
                ambient *= 1.0 - style.sky_occlusion * (1.0 - (0.15 + 0.85 * sky));
            }
            let mut light = ambient + if shadow { 0.0 } else { style.sun_strength };
            if style.height_shade != 0.0 {
                light *= 1.0
                    + style.height_shade
                        * ((f64::from(by + 1 - 64)) / 64.0).clamp(-1.0, 1.0) as f32;
            }
            light
        };
        let depth = (t_hit - t_entry).max(0.0);
        out.water = depth as f32;
        if style.clear_water {
            // Light through water loses the colours the water doesn't have: red goes first, so
            // shallows over sand turn turquoise. The floor is gone a few blocks down.
            let peak = wr.max(wg).max(wb).max(1e-4);
            let tr = (-depth * (0.45 * f64::from(1.0 - wr / peak) + 0.16)).exp() as f32;
            let tg = (-depth * (0.45 * f64::from(1.0 - wg / peak) + 0.16)).exp() as f32;
            let tb = (-depth * (0.45 * f64::from(1.0 - wb / peak) + 0.16)).exp() as f32;
            let deep = light * (0.3 + 0.7 * (-depth / 18.0).exp()) as f32;
            // A little richer than the water's own colour, to sit beside grass.
            let l = luminance(wr, wg, wb);
            let rich = |c: f32| (l + (c - l) * 1.2).max(0.0) * deep;
            out.r = out.r * tr + rich(wr) * (1.0 - tr);
            out.g = out.g * tg + rich(wg) * (1.0 - tg);
            out.b = out.b * tb + rich(wb) * (1.0 - tb);
            return;
        }
        let a = (1.0 - (-0.2 * depth).exp()).clamp(0.5, 0.94) as f32;
        out.r = out.r * (1.0 - a) + wr * light * a;
        out.g = out.g * (1.0 - a) + wg * light * a;
        out.b = out.b * (1.0 - a) + wb * light * a;
    }

    /// Sky visibility on a top face at `y`, blended from its four corners.
    fn sky_top(&mut self, x: i32, y: i32, z: i32, fx: f64, fz: f64) -> f32 {
        let (a, b, c, d) = (
            self.sky(x, y, z),
            self.sky(x + 1, y, z),
            self.sky(x, y, z + 1),
            self.sky(x + 1, y, z + 1),
        );
        bilinear(a, b, c, d, fx, fz)
    }

    /// Sky visibility on a south face at `z`, blended from its four corners; `fy` runs bottom to top.
    fn sky_south(&mut self, x: i32, y: i32, z: i32, fx: f64, fy: f64) -> f32 {
        let (a, b, c, d) = (
            self.sky(x, y + 1, z),
            self.sky(x + 1, y + 1, z),
            self.sky(x, y, z),
            self.sky(x + 1, y, z),
        );
        bilinear(a, b, c, d, fx, 1.0 - fy)
    }

    /// Share of the sky seen from a block corner, 0..1, from a few short rays upward.
    fn sky(&mut self, x: i32, y: i32, z: i32) -> f32 {
        if let Some(v) = self.sky_at.get(&(x, y, z)) {
            return *v;
        }
        let t = tables();
        let (mut open, mut total) = (0.0, 0.0);
        for (d, w) in t.sky_rays.iter().zip(&t.sky_weights) {
            // Start a hair along the ray, so it begins in the block it heads into.
            let blocked = self.blocked(
                f64::from(x) + d[0] * 1e-3,
                f64::from(y) + d[1] * 1e-3,
                f64::from(z) + d[2] * 1e-3,
                d[0],
                d[1],
                d[2],
                f64::from(SKY_REACH),
                (SKY_REACH * 3) as usize,
                false,
            );
            if !blocked {
                open += w;
            }
            total += w;
        }
        let v = (open / total) as f32;
        self.sky_at.insert((x, y, z), v);
        v
    }

    /// Walks from a point along a direction for up to `distance` blocks; true if a solid block is in
    /// the way. Without `shapes`, a block smaller than a cube blocks the way like a cube.
    #[allow(clippy::too_many_arguments)]
    fn blocked(
        &mut self,
        ox: f64,
        oy: f64,
        oz: f64,
        dx: f64,
        dy: f64,
        dz: f64,
        distance: f64,
        steps: usize,
        shapes: bool,
    ) -> bool {
        let (ix, iy, iz) = (1.0 / dx, 1.0 / dy, 1.0 / dz);
        let (mut cx, mut cy, mut cz) = (ox.floor() as i32, oy.floor() as i32, oz.floor() as i32);
        let start = if cy < self.ray_top && !(self.lifted && cy >= self.lid(cx, cz)) {
            self.reader.solid(cx, cy, cz)
        } else {
            0
        };
        if start == 1
            || start == 2
                && (!shapes || self.meets_shape(cx, cy, cz, ox, oy, oz, [dx, dy, dz], [ix, iy, iz]))
        {
            return true;
        }
        // Under a lifted lid nothing above the cut is left to block light.
        let mut top = if self.lifted {
            self.ray_top.min(self.start_y)
        } else {
            self.ray_top
        };
        if dy > 0.0 {
            // A ray that only climbs can't be blocked once it is above every solid block in the
            // chunks it can reach.
            let ex = ox + dx * distance;
            let ez = oz + dz * distance;
            top = top.min(self.reader.solid_top(
                (ox.min(ex).floor() as i32 - 1) >> 4,
                (oz.min(ez).floor() as i32 - 1) >> 4,
                (ox.max(ex).floor() as i32 + 1) >> 4,
                (oz.max(ez).floor() as i32 + 1) >> 4,
            ));
        }
        let step_x = if dx > 0.0 { 1 } else { -1 };
        let step_y = if dy > 0.0 { 1 } else { -1 };
        let step_z = if dz > 0.0 { 1 } else { -1 };
        let t_delta_x = if dx == 0.0 { f64::MAX } else { ix.abs() };
        let t_delta_y = if dy == 0.0 { f64::MAX } else { iy.abs() };
        let t_delta_z = if dz == 0.0 { f64::MAX } else { iz.abs() };
        let mut t_max_x = if dx == 0.0 {
            f64::MAX
        } else {
            (if dx > 0.0 {
                f64::from(cx + 1) - ox
            } else {
                ox - f64::from(cx)
            }) * t_delta_x
        };
        let mut t_max_y = if dy == 0.0 {
            f64::MAX
        } else {
            (if dy > 0.0 {
                f64::from(cy + 1) - oy
            } else {
                oy - f64::from(cy)
            }) * t_delta_y
        };
        let mut t_max_z = if dz == 0.0 {
            f64::MAX
        } else {
            (if dz > 0.0 {
                f64::from(cz + 1) - oz
            } else {
                oz - f64::from(cz)
            }) * t_delta_z
        };
        for _ in 0..steps {
            if t_max_x.min(t_max_y).min(t_max_z) > distance {
                return false;
            }
            if t_max_x < t_max_y && t_max_x < t_max_z {
                cx += step_x;
                t_max_x += t_delta_x;
            } else if t_max_y < t_max_z {
                cy += step_y;
                t_max_y += t_delta_y;
            } else {
                cz += step_z;
                t_max_z += t_delta_z;
            }
            if cy >= top {
                return false;
            }
            let solid = self.reader.solid(cx, cy, cz);
            if solid == UNKNOWN {
                return false;
            }
            if self.lifted && solid > 0 && cy >= self.lid(cx, cz) {
                continue;
            }
            if solid == 1
                || solid == 2
                    && (!shapes
                        || self.meets_shape(cx, cy, cz, ox, oy, oz, [dx, dy, dz], [ix, iy, iz]))
            {
                return true;
            }
        }
        false
    }

    /// Whether a ray meets the solid block in this cell: always for a full cube, and for a smaller
    /// block only if it crosses one of its boxes.
    #[allow(clippy::too_many_arguments)]
    fn meets_shape(
        &mut self,
        cx: i32,
        cy: i32,
        cz: i32,
        ox: f64,
        oy: f64,
        oz: f64,
        d: [f64; 3],
        inv: [f64; 3],
    ) -> bool {
        let Some(id) = self.reader.state(cx, cy, cz) else {
            return true;
        };
        let shape = self.looks.shape.get(id as usize).copied().unwrap_or(0);
        if shape == 0 {
            return true;
        }
        let p = [ox - f64::from(cx), oy - f64::from(cy), oz - f64::from(cz)];
        for b in self.looks.boxes(self.reader.facing, shape) {
            let (mut near, mut far) = (0.0f64, f64::MAX);
            for axis in 0..3 {
                if near > far {
                    break;
                }
                let (min, max) = (f64::from(b[axis]), f64::from(b[axis + 3]));
                if d[axis] == 0.0 {
                    if p[axis] < min || p[axis] > max {
                        far = -1.0;
                    }
                } else {
                    let t0 = (min - p[axis]) * inv[axis];
                    let t1 = (max - p[axis]) * inv[axis];
                    near = near.max(t0.min(t1));
                    far = far.min(t0.max(t1));
                }
            }
            if near <= far {
                return true;
            }
        }
        false
    }

    /// Whether a camera ray crossing cell (`by`, `bz`) between `t_in` and `t_out` meets one of a
    /// block's boxes, and where first. The ray runs `fx` across the cell, heading down and north.
    #[allow(clippy::too_many_arguments)]
    fn enter_shape(
        &mut self,
        shape: u32,
        fx: f64,
        y0: f64,
        z0: f64,
        by: i32,
        bz: i32,
        t_in: f64,
        t_out: f64,
        in_top: bool,
    ) -> bool {
        let mut best = f64::MAX;
        for b in self.looks.boxes(self.reader.facing, shape) {
            if fx < f64::from(b[0]) || fx >= f64::from(b[3]) {
                continue;
            }
            let t_top = (y0 - f64::from(by) - f64::from(b[4])) * self.inv_sin;
            let t_south = (z0 - f64::from(bz) - f64::from(b[5])) * self.inv_cos;
            let enter = t_in.max(t_top.max(t_south));
            let exit = t_out.min(
                ((y0 - f64::from(by) - f64::from(b[1])) * self.inv_sin)
                    .min((z0 - f64::from(bz) - f64::from(b[2])) * self.inv_cos),
            );
            if enter < exit && enter < best {
                best = enter;
                // A box flush with the cell's side is met where the ray came into the cell.
                self.shape_top = if enter == t_in {
                    in_top
                } else {
                    t_top >= t_south
                };
            }
        }
        self.shape_t = best;
        best != f64::MAX
    }

    fn occludes(&mut self, x: i32, y: i32, z: i32) -> bool {
        self.reader.solid(x, y, z) > 0
    }

    /// Smooth corner occlusion on a top face, from the eight blocks around the cell above it.
    fn ao_top(&mut self, x: i32, y: i32, z: i32, fx: f64, fz: f64) -> f32 {
        let n = self.occludes(x, y, z - 1);
        let s = self.occludes(x, y, z + 1);
        let w = self.occludes(x - 1, y, z);
        let e = self.occludes(x + 1, y, z);
        let nw = corner(w, n, self.occludes(x - 1, y, z - 1));
        let ne = corner(e, n, self.occludes(x + 1, y, z - 1));
        let sw = corner(w, s, self.occludes(x - 1, y, z + 1));
        let se = corner(e, s, self.occludes(x + 1, y, z + 1));
        bilinear(nw, ne, sw, se, fx, fz)
    }

    /// Same for a south face; `z` is the open layer in front of it, `fy` runs bottom to top.
    fn ao_south(&mut self, x: i32, y: i32, z: i32, fx: f64, fy: f64) -> f32 {
        let up = self.occludes(x, y + 1, z);
        let down = self.occludes(x, y - 1, z);
        let w = self.occludes(x - 1, y, z);
        let e = self.occludes(x + 1, y, z);
        let tw = corner(w, up, self.occludes(x - 1, y + 1, z));
        let te = corner(e, up, self.occludes(x + 1, y + 1, z));
        let bw = corner(w, down, self.occludes(x - 1, y - 1, z));
        let be = corner(e, down, self.occludes(x + 1, y - 1, z));
        bilinear(tw, te, bw, be, fx, 1.0 - fy)
    }
}

/// One above the tallest block any ray or shadow ray of this tile can touch, below `ceiling`.
/// Starting rays here instead of at the tallest block in the dimension skips a lot of empty sky.
fn local_top(
    key: &TileKey,
    reader: &mut VoxelReader<'_>,
    p: &Projection,
    more_north: i32,
    ceiling: i32,
) -> i32 {
    let (reach_x, reach_z, _) = shadow_reach();
    let (min_u, min_v) = (key.min_u(), key.min_v());
    let units = key.units();
    let x0 = (min_u.floor() as i32 - 1).div_euclid(16);
    let x1 = ((min_u + units).ceil() as i32 + reach_x + 1).div_euclid(16);
    let z0 =
        (p.z_at(min_v, f64::from(reader.min_y)).floor() as i32 - 1 - more_north).div_euclid(16);
    let z1 = (p.z_at(min_v + units, f64::from(ceiling)).ceil() as i32 + reach_z + 1).div_euclid(16);
    let mut top = i32::MIN;
    for cx in x0 - 2..=x1 {
        for cz in z0..=z1 + 1 {
            top = top.max(reader.top_below(cx, cz, ceiling));
        }
    }
    if top == i32::MIN {
        ceiling
    } else {
        top.min(ceiling)
    }
}

/// The tiles (level 0, of `layer`) whose pixels may change when blocks in world box
/// `min..=max` change: where those blocks show, and where their shadows,
/// shade and sky occlusion fall.
pub fn tiles_touched(p: &Projection, layer: i32, min: [i32; 3], max: [i32; 3]) -> Vec<TileKey> {
    let ([x0, y0, z0], [x1, y1, z1]) = (min, max);
    let (rx, rz, ry) = shadow_reach();
    let pad = SKY_REACH + 1;
    // The box's corners in turned space.
    let f = p.facing;
    let xs = [
        f.block_x(x0, z0),
        f.block_x(x1, z1),
        f.block_x(x0, z1),
        f.block_x(x1, z0),
    ];
    let zs = [
        f.block_z(x0, z0),
        f.block_z(x1, z1),
        f.block_z(x0, z1),
        f.block_z(x1, z0),
    ];
    let tx0 = xs.iter().min().copied().unwrap_or(0) - pad - rx;
    let tx1 = xs.iter().max().copied().unwrap_or(0) + 1 + pad + rx;
    let tz0 = zs.iter().min().copied().unwrap_or(0) - pad - rz;
    let tz1 = zs.iter().max().copied().unwrap_or(0) + 1 + pad + rz;
    let (ylo, yhi) = (y0 - pad - ry, y1 + 1 + pad);
    let v0 = p.v_turned(f64::from(yhi), f64::from(tz0));
    let v1 = p.v_turned(f64::from(ylo), f64::from(tz1));
    let units = f64::from(TILE_UNITS);
    let (u_lo, u_hi) = (
        (f64::from(tx0) / units).floor() as i32,
        (f64::from(tx1) / units).floor() as i32,
    );
    let (v_lo, v_hi) = ((v0 / units).floor() as i32, (v1 / units).floor() as i32);
    let mut out = Vec::new();
    for v in v_lo..=v_hi {
        for u in u_lo..=u_hi {
            out.push(TileKey::new(layer, u, v));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::chunk::{Blocks, ChunkSnapshot, ColumnTints, Light, Section};
    use crate::looks::tests::test_looks;
    use crate::projection::Facing;
    use crate::source::LiveChunks;
    use std::sync::Arc;

    /// A flat world: stone up to y 63 with a grass top at 63, from chunk -r to r, and a stone
    /// tower at (8..12, 64..80, 8..12) when `tower`.
    pub(crate) fn flat_world(r: i32, tower: bool) -> LiveChunks {
        let looks = test_looks();
        let live = LiveChunks::new(-64, 320, None);
        for cx in -r..=r {
            for cz in -r..=r {
                let mut sections = Vec::new();
                for sy in -4..20 {
                    let base = sy * 16;
                    let mut ids = vec![0u32; 4096];
                    for ly in 0..16 {
                        let y = base + ly;
                        for lz in 0..16 {
                            for lx in 0..16 {
                                let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                                let id = if y < 63 {
                                    1
                                } else if y == 63 {
                                    2
                                } else if tower
                                    && (8..12).contains(&x)
                                    && (8..12).contains(&z)
                                    && y < 80
                                {
                                    1
                                } else {
                                    0
                                };
                                ids[crate::chunk::index(lx, ly, lz)] = id;
                            }
                        }
                    }
                    sections.push(Section {
                        blocks: Blocks::from_ids(&ids),
                        light: Light::sky(if base >= 64 { 15 } else { 0 }),
                    });
                }
                live.put(Arc::new(ChunkSnapshot::new(
                    cx,
                    cz,
                    -4,
                    sections,
                    Arc::new(ColumnTints::plains()),
                    |id| looks.kind(id) == Kind::Air,
                )));
            }
        }
        live
    }

    fn settings(facing: Facing) -> BakeSettings {
        BakeSettings {
            style: MapStyle::DEFAULT,
            projection: Projection::new(MapStyle::DEFAULT.pitch_degrees, facing),
            fast: true,
            fog_of_war: false,
        }
    }

    /// The tile pixel showing world block top (x, y, z), centre.
    fn pixel_of(p: &Projection, x: f64, y: f64, z: f64) -> (TileKey, usize) {
        let (u, v) = (p.u(x, z), p.v(x, y, z));
        let units = f64::from(TILE_UNITS);
        let key = TileKey::new(
            TileKey::SURFACE,
            (u / units).floor() as i32,
            (v / units).floor() as i32,
        );
        let px = ((u - key.min_u()) * f64::from(PIXELS_PER_UNIT)) as usize;
        let py = ((v - key.min_v()) * f64::from(PIXELS_PER_UNIT)) as usize;
        (key, py * TILE_PIXELS as usize + px)
    }

    #[test]
    fn flat_ground_is_known_and_green() {
        let world = flat_world(3, false);
        let looks = test_looks();
        let s = settings(Facing::North);
        let (key, i) = pixel_of(&s.projection, 0.5, 64.0, 0.5);
        let img = Baker::bake(key, &world, &looks, &s);
        let p = img.pixels[i];
        assert_eq!(p >> 24, 255, "known");
        let (r, g, b) = (p & 0xFF, (p >> 8) & 0xFF, (p >> 16) & 0xFF);
        assert!(g > r && g > b, "grass reads green: {r} {g} {b}");
    }

    #[test]
    fn unloaded_ground_is_unknown() {
        let world = flat_world(0, false);
        let looks = test_looks();
        let s = settings(Facing::North);
        let (key, i) = pixel_of(&s.projection, 100.5, 64.0, 100.5);
        let img = Baker::bake(key, &world, &looks, &s);
        assert_eq!(img.pixels[i] >> 24, 0);
    }

    #[test]
    fn a_tower_casts_a_shadow_to_the_east() {
        let looks = test_looks();
        let s = settings(Facing::North);
        let world = flat_world(3, false);
        let tower = flat_world(3, true);
        // Shadows fall away from the sun, which is west and a little south: east and a little north.
        let (key, i) = pixel_of(&s.projection, 16.5, 64.0, 9.5);
        let open = Baker::bake(key, &world, &looks, &s).pixels[i];
        let shade = Baker::bake(key, &tower, &looks, &s).pixels[i];
        let lum = |p: u32| (p & 0xFF) + ((p >> 8) & 0xFF) + ((p >> 16) & 0xFF);
        assert!(
            lum(shade) < lum(open),
            "shadow {shade:08x} vs open {open:08x}"
        );
    }

    #[test]
    fn turned_views_see_the_same_ground() {
        let world = flat_world(3, true);
        let looks = test_looks();
        for f in Facing::ALL {
            let s = settings(f);
            let (key, i) = pixel_of(&s.projection, -20.5, 64.0, -20.5);
            let img = Baker::bake(key, &world, &looks, &s);
            assert_eq!(img.pixels[i] >> 24, 255, "{f:?}");
        }
    }

    #[test]
    fn edits_touch_the_tiles_they_show_in() {
        let p = Projection::new(50.0, Facing::North);
        let (key, _) = pixel_of(&p, 10.5, 70.0, 10.5);
        let touched = tiles_touched(&p, TileKey::SURFACE, [10, 70, 10], [10, 70, 10]);
        assert!(touched.contains(&key));
        assert!(touched.len() <= 9, "{}", touched.len());
    }

    #[test]
    fn noise_is_stable() {
        assert_eq!(block_noise(1, 2, 3), block_noise(1, 2, 3));
        assert!((-1.0..=1.0).contains(&block_noise(-7, 64, 99)));
    }
}
