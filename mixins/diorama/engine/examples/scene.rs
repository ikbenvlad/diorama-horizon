//! Bakes a made-up landscape and writes what the map shows to a PNG, for looking at by eye.
//! `cargo run --release --example scene -- out.png [facing] [zoom]`

use diorama_engine::chunk::{Blocks, ChunkSnapshot, ColumnTints, Light, Section};
use diorama_engine::looks::{self, Average, FaceColor, FaceTint, Kind, StateInput, TintKind};
use diorama_engine::projection::Facing;
use diorama_engine::session::{LOAD_DELAY, MapSession, SessionConfig};
use diorama_engine::style::MapStyle;
use diorama_engine::tile::TileKey;
use diorama_engine::view::{Fog, Look, View, lod_for};
use std::sync::Arc;
use std::time::{Duration, Instant};

const AIR: u32 = 0;
const STONE: u32 = 1;
const GRASS: u32 = 2;
const WATER: u32 = 3;
const SLAB: u32 = 4;
const SAND: u32 = 5;
const LOG: u32 = 6;
const LEAVES: u32 = 7;
const DIRT: u32 = 8;
const FLOWER: u32 = 9;
const PLANKS: u32 = 10;

fn face(rgb: u32, tint: Option<TintKind>) -> FaceColor {
    let mut f = FaceColor::default();
    f.add(Average::flat(rgb), tint.map(FaceTint::Biome));
    f
}

fn looks() -> looks::BlockLooks {
    let solid = |top: FaceColor, side: FaceColor| StateInput {
        kind: Kind::Solid,
        top,
        side,
        ..Default::default()
    };
    let states = vec![
        StateInput::default(),
        solid(face(0x7D7D7D, None), face(0x7D7D7D, None)),
        solid(face(0x959595, Some(TintKind::Grass)), face(0x866043, None)),
        StateInput {
            kind: Kind::Water,
            fluid: Some((0xB5B5B5, Some(FaceTint::Biome(TintKind::Water)))),
            ..Default::default()
        },
        StateInput {
            kind: Kind::Solid,
            outline: vec![[0.0, 0.0, 0.0, 1.0, 0.5, 1.0]],
            ..solid(face(0xA8A8A8, None), face(0x9A9A9A, None))
        },
        solid(face(0xDBD3A0, None), face(0xDBD3A0, None)),
        solid(face(0x9C7F4E, None), face(0x6B5131, None)),
        StateInput {
            leaves: true,
            ..solid(
                face(0x8F8F8F, Some(TintKind::Foliage)),
                face(0x8F8F8F, Some(TintKind::Foliage)),
            )
        },
        solid(face(0x866043, None), face(0x866043, None)),
        StateInput {
            kind: Kind::Plant,
            top: face(0xD03030, None),
            ..Default::default()
        },
        solid(face(0xA2834F, None), face(0xA2834F, None)),
    ];
    looks::build(&states, AIR)
}

fn height(x: i32, z: i32) -> i32 {
    let (fx, fz) = (x as f64, z as f64);
    let h = 70.0 + 9.0 * (fx / 23.0).sin() * (fz / 31.0).cos() + 5.0 * ((fx + fz) / 13.0).sin()
        - 14.0 * (-((fx - 40.0).powi(2) + (fz + 10.0).powi(2)) / 900.0).exp();
    h.floor() as i32
}

fn hash(x: i32, z: i32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B9) ^ (z as u32).wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h.wrapping_mul(0x2C1B_3C6D)
}

fn block(x: i32, y: i32, z: i32) -> u32 {
    // A house of planks with a slab roof.
    if (-30..-20).contains(&x) && (20..28).contains(&z) {
        let floor = height(-25, 24);
        if y > floor && y <= floor + 4 && (x == -30 || x == -21 || z == 20 || z == 27) {
            return PLANKS;
        }
        if y == floor + 5 {
            return SLAB;
        }
        if y <= floor {
            return STONE;
        }
        return AIR;
    }
    // A stone tower.
    if (0..4).contains(&x) && (0..4).contains(&z) && y < 100 {
        return STONE;
    }
    let h = height(x, z);
    let water = 64;
    if y < h - 3 {
        return STONE;
    }
    if y < h {
        return if h <= water + 1 { SAND } else { DIRT };
    }
    if y == h {
        return if h <= water + 1 { SAND } else { GRASS };
    }
    if y <= water {
        return WATER;
    }
    // Trees on grass.
    for (tx, tz) in [
        (x, z),
        (x - 1, z),
        (x + 1, z),
        (x, z - 1),
        (x, z + 1),
        (x - 1, z - 1),
        (x + 1, z + 1),
        (x - 1, z + 1),
        (x + 1, z - 1),
        (x - 2, z),
        (x + 2, z),
        (x, z - 2),
        (x, z + 2),
    ] {
        let th = height(tx, tz);
        if th > water + 1 && hash(tx, tz).is_multiple_of(97) {
            let top = th + 6;
            if tx == x && tz == z && y > th && y < top {
                return LOG;
            }
            let (dx, dz, dy) = ((x - tx).abs(), (z - tz).abs(), y - top);
            if (-2..=1).contains(&dy)
                && dx + dz <= if dy >= 0 { 1 } else { 2 }
                && !(dx == 0 && dz == 0 && dy < 0)
            {
                return LEAVES;
            }
        }
    }
    if y == h + 1 && h > water + 1 && hash(x, z).is_multiple_of(23) {
        return FLOWER;
    }
    AIR
}

fn chunk(cx: i32, cz: i32, l: &looks::BlockLooks) -> ChunkSnapshot {
    let mut sections = Vec::new();
    for sy in -4..20 {
        let mut ids = vec![AIR; 4096];
        for ly in 0..16 {
            for lz in 0..16 {
                for lx in 0..16 {
                    ids[((ly << 8) | (lz << 4) | lx) as usize] =
                        block(cx * 16 + lx, sy * 16 + ly, cz * 16 + lz);
                }
            }
        }
        sections.push(Section {
            blocks: Blocks::from_ids(&ids),
            light: Light::sky(15),
        });
    }
    let mut tints = ColumnTints::plains();
    for i in 0..256 {
        tints.grass[i] = 0x79C05A;
        tints.foliage[i] = 0x59AE30;
    }
    ChunkSnapshot::new(cx, cz, -4, sections, Arc::new(tints), |id| {
        l.kind(id) == Kind::Air
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_path = args.get(1).cloned().unwrap_or_else(|| "scene.png".into());
    let facing = Facing::of(args.get(2).map_or("north", String::as_str));
    let zoom: f64 = args.get(3).and_then(|z| z.parse().ok()).unwrap_or(2.0);
    let dir = std::env::temp_dir().join(format!("diorama-scene-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let l = Arc::new(looks());
    let mut s = MapSession::new(SessionConfig {
        folder: dir.clone(),
        min_y: -64,
        max_y: 320,
        roof: None,
        style: MapStyle::DEFAULT,
        facing,
        fast: false,
        threads: SessionConfig::default_threads().max(4),
    });
    s.set_looks(l.clone());
    let started = Instant::now();
    for cx in -6..6 {
        for cz in -6..6 {
            s.chunk_loaded(Arc::new(chunk(cx, cz, &l)), started);
        }
    }
    let player = [0.0, 100.0, 10.0];
    let (u, v) = s.uv([0.0, 70.0, 10.0]);
    let (w, h) = (768, 512);
    let view = View {
        center_u: u,
        center_v: v,
        zoom,
        layer: TileKey::SURFACE,
        lod: lod_for(zoom, zoom, false),
        width: w,
        height: h,
        round: false,
    };
    let mut out = vec![0u32; w * h];
    let bake_started = Instant::now();
    loop {
        s.tick(Instant::now() + LOAD_DELAY, player);
        s.compose(
            &view,
            &Look::new(false, false),
            Some(&Fog::default()),
            &mut out,
        );
        let (d, wt, b) = s.busy();
        if d + wt + b == 0 && s.cache_version() > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    s.compose(
        &view,
        &Look::new(false, false),
        Some(&Fog::default()),
        &mut out,
    );
    eprintln!("baked in {:.2?}", bake_started.elapsed());
    let mut rgba = Vec::new();
    diorama_engine::view::to_rgba(&out, &mut rgba);
    let file = std::fs::File::create(&out_path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("header")
        .write_image_data(&rgba)
        .expect("data");
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}
