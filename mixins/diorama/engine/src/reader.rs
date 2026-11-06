//! Block and light lookups for one bake job, in turned space.
//!
//! Lookups are the bake's inner loop, so they skip what they can: the chunks a job has seen sit in
//! a small table indexed by their coordinates, and each section's solid blocks are kept as a bit
//! per block, built once, which keeps far more of the world in the CPU's caches than the ids do.
//! Not thread-safe; each job gets its own reader.

use crate::chunk::{ChunkSnapshot, ColumnTints};
use crate::looks::{BlockLooks, Kind, TintKind};
use crate::projection::Facing;
use crate::source::ChunkSource;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// A lookup in a chunk the source has no data for.
pub const UNKNOWN: i8 = -1;

const TABLE_BITS: i32 = 5;
const TABLE_MASK: i32 = (1 << TABLE_BITS) - 1;
const TABLE_SIZE: usize = 1 << (2 * TABLE_BITS);

/// One chunk as a job reads it: the snapshot, with masks made on first use.
struct Decoded {
    snap: Arc<ChunkSnapshot>,
    /// Per section, every block's id, unpacked on first use.
    ids: Vec<OnceCell<Box<[u32; 4096]>>>,
    /// Per section, a bit per solid block, indexed like the blocks.
    solid: Vec<OnceCell<Box<[u64; 64]>>>,
    /// Per section, which solid blocks are smaller than a cube.
    partial: Vec<OnceCell<Box<[u64; 64]>>>,
    solid_top: OnceCell<i32>,
}

impl Decoded {
    fn new(snap: Arc<ChunkSnapshot>) -> Decoded {
        let n = snap.sections.len();
        Decoded {
            snap,
            ids: (0..n).map(|_| OnceCell::new()).collect(),
            solid: (0..n).map(|_| OnceCell::new()).collect(),
            partial: (0..n).map(|_| OnceCell::new()).collect(),
            solid_top: OnceCell::new(),
        }
    }

    /// A section's ids, unpacked once.
    fn ids(&self, index: usize) -> &[u32; 4096] {
        self.ids[index].get_or_init(|| {
            let mut out = Box::new([0u32; 4096]);
            self.snap.sections[index].blocks.unpack(&mut out);
            out
        })
    }

    #[inline]
    fn state(&self, lx: i32, y: i32, lz: i32, air: u32) -> u32 {
        let index = (y >> 4) - self.snap.min_section_y;
        if index < 0 || index as usize >= self.snap.sections.len() {
            return air;
        }
        match &self.snap.sections[index as usize].blocks {
            crate::chunk::Blocks::Single(s) => *s,
            crate::chunk::Blocks::Packed { .. } => {
                self.ids(index as usize)[crate::chunk::index(lx, y, lz)]
            }
        }
    }

    fn masks(&self, index: usize, looks: &BlockLooks) -> (&[u64; 64], &[u64; 64]) {
        let solid = self.solid[index].get_or_init(|| {
            let mut solid = Box::new([0u64; 64]);
            let mut partial = Box::new([0u64; 64]);
            let single = match &self.snap.sections[index].blocks {
                crate::chunk::Blocks::Single(s) => Some(*s),
                crate::chunk::Blocks::Packed { .. } => None,
            };
            if let Some(id) = single {
                if looks.kind(id) == Kind::Solid {
                    solid.fill(u64::MAX);
                    if looks.shape[id as usize] != 0 {
                        partial.fill(u64::MAX);
                    }
                }
                let _ = self.partial[index].set(partial);
                return solid;
            }
            let ids = self.ids(index);
            for (i, &id) in ids.iter().enumerate() {
                if looks.kind(id) == Kind::Solid {
                    solid[i >> 6] |= 1 << (i & 63);
                    if looks.shape[id as usize] != 0 {
                        partial[i >> 6] |= 1 << (i & 63);
                    }
                }
            }
            let _ = self.partial[index].set(partial);
            solid
        });
        (solid, self.partial[index].get().expect("set with solid"))
    }

    /// One above the highest solid block, or `i32::MIN`.
    fn solid_top(&self, looks: &BlockLooks) -> i32 {
        *self.solid_top.get_or_init(|| {
            for index in (0..self.snap.sections.len()).rev() {
                let (solid, _) = self.masks(index, looks);
                // Four words to a layer of y.
                for ly in (0..16).rev() {
                    if solid[ly * 4..ly * 4 + 4].iter().any(|w| *w != 0) {
                        return (self.snap.min_section_y + index as i32) * 16 + ly as i32 + 1;
                    }
                }
            }
            i32::MIN
        })
    }
}

pub struct VoxelReader<'a> {
    source: &'a dyn ChunkSource,
    pub looks: &'a BlockLooks,
    pub facing: Facing,
    pub min_y: i32,
    pub max_y: i32,
    /// Every chunk this job has asked the source for, none where it had none: a chunk is asked
    /// for once, so the whole job sees the same blocks while chunks load and unload.
    cache: HashMap<(i32, i32), Option<Rc<Decoded>>>,
    table_key: Vec<(i32, i32)>,
    table: Vec<Option<Rc<Decoded>>>,
    /// Chunks (world coordinates) asked for and missing.
    pub missing: usize,
}

impl<'a> VoxelReader<'a> {
    pub fn new(source: &'a dyn ChunkSource, looks: &'a BlockLooks, facing: Facing) -> Self {
        Self {
            source,
            looks,
            facing,
            min_y: source.min_y(),
            max_y: source.max_y(),
            cache: HashMap::new(),
            table_key: vec![(i32::MIN, i32::MIN); TABLE_SIZE],
            table: vec![None; TABLE_SIZE],
            missing: 0,
        }
    }

    /// The world block of a turned block.
    #[inline]
    fn world(&self, x: i32, z: i32) -> (i32, i32) {
        (
            self.facing.world_block_x(x, z),
            self.facing.world_block_z(x, z),
        )
    }

    /// A chunk by world chunk coordinates.
    #[inline]
    fn chunk(&mut self, cx: i32, cz: i32) -> Option<&Rc<Decoded>> {
        let slot = (((cx & TABLE_MASK) << TABLE_BITS) | (cz & TABLE_MASK)) as usize;
        if self.table_key[slot] != (cx, cz) {
            let decoded = match self.cache.get(&(cx, cz)) {
                Some(d) => d.clone(),
                None => {
                    let d = self.source.chunk(cx, cz).map(|s| Rc::new(Decoded::new(s)));
                    if d.is_none() {
                        self.missing += 1;
                    }
                    self.cache.insert((cx, cz), d.clone());
                    d
                }
            };
            self.table_key[slot] = (cx, cz);
            self.table[slot] = decoded;
        }
        self.table[slot].as_ref()
    }

    /// Block state id at a turned position, air outside the world, or none where unknown.
    #[inline]
    pub fn state(&mut self, x: i32, y: i32, z: i32) -> Option<u32> {
        let air = self.looks.air;
        if y < self.min_y || y >= self.max_y {
            return Some(air);
        }
        let (wx, wz) = self.world(x, z);
        let c = self.chunk(wx >> 4, wz >> 4)?;
        Some(c.state(wx & 15, y, wz & 15, air))
    }

    /// 1 if the block is solid (it stops shadow and sky rays and casts ambient occlusion), 2 if
    /// solid but smaller than a cube, 0 if not, or [`UNKNOWN`].
    #[inline]
    pub fn solid(&mut self, x: i32, y: i32, z: i32) -> i8 {
        if y < self.min_y || y >= self.max_y {
            return 0;
        }
        let (wx, wz) = self.world(x, z);
        let looks = self.looks;
        let Some(c) = self.chunk(wx >> 4, wz >> 4) else {
            return UNKNOWN;
        };
        let index = (y >> 4) - c.snap.min_section_y;
        if index < 0 || index as usize >= c.snap.sections.len() {
            return 0;
        }
        let (solid, partial) = c.masks(index as usize, looks);
        let i = crate::chunk::index(wx, y, wz);
        if (solid[i >> 6] >> (i & 63)) & 1 == 0 {
            return 0;
        }
        1 + ((partial[i >> 6] >> (i & 63)) & 1) as i8
    }

    pub fn block_light(&mut self, x: i32, y: i32, z: i32) -> u8 {
        if y < self.min_y || y >= self.max_y {
            return 0;
        }
        let (wx, wz) = self.world(x, z);
        let Some(c) = self.chunk(wx >> 4, wz >> 4) else {
            return 0;
        };
        c.snap
            .section(y)
            .map_or(0, |s| s.light.block(crate::chunk::index(wx, y, wz)))
    }

    pub fn sky_light(&mut self, x: i32, y: i32, z: i32) -> u8 {
        if y < self.min_y {
            return 0;
        }
        if y >= self.max_y {
            return 15;
        }
        let (wx, wz) = self.world(x, z);
        let Some(c) = self.chunk(wx >> 4, wz >> 4) else {
            return 15;
        };
        c.snap
            .section(y)
            .map_or(15, |s| s.light.sky_level(crate::chunk::index(wx, y, wz)))
    }

    /// The biome tint of a kind at a turned column; white where unknown.
    pub fn tint(&mut self, kind: TintKind, x: i32, z: i32) -> u32 {
        let (wx, wz) = self.world(x, z);
        let Some(c) = self.chunk(wx >> 4, wz >> 4) else {
            return 0xFFFFFF;
        };
        let t: &ColumnTints = &c.snap.tints;
        let i = (((wz & 15) << 4) | (wx & 15)) as usize;
        match kind {
            TintKind::None => 0xFFFFFF,
            TintKind::Grass => t.grass[i],
            TintKind::Foliage => t.foliage[i],
            TintKind::DryFoliage => t.dry_foliage[i],
            TintKind::Water => t.water[i],
        }
    }

    /// Whether the source has the chunk holding this turned column.
    pub fn has_column(&mut self, x: i32, z: i32) -> bool {
        let (wx, wz) = self.world(x, z);
        self.chunk(wx >> 4, wz >> 4).is_some()
    }

    /// One above the highest non-air block below `ceiling` in a turned chunk, or `i32::MIN`.
    pub fn top_below(&mut self, cx: i32, cz: i32, ceiling: i32) -> i32 {
        let (wx, wz) = (
            self.facing.world_block_x(cx, cz),
            self.facing.world_block_z(cx, cz),
        );
        let looks = self.looks;
        match self.chunk(wx, wz) {
            Some(c) => c.snap.top_below(ceiling, |id| looks.kind(id) == Kind::Air),
            None => i32::MIN,
        }
    }

    /// One above the highest solid block in these turned chunks, or `i32::MIN` if they have none.
    /// Chunks missing from the source are left out. If this job hasn't asked for one of the chunks
    /// yet, returns `i32::MAX` instead of asking, so the blocks a job sees don't depend on this call.
    pub fn solid_top(&mut self, cx0: i32, cz0: i32, cx1: i32, cz1: i32) -> i32 {
        let mut top = i32::MIN;
        for cx in cx0..=cx1 {
            for cz in cz0..=cz1 {
                let (wx, wz) = (
                    self.facing.world_block_x(cx, cz),
                    self.facing.world_block_z(cx, cz),
                );
                match self.cache.get(&(wx, wz)) {
                    None => return i32::MAX,
                    Some(None) => {}
                    Some(Some(c)) => top = top.max(c.solid_top(self.looks)),
                }
            }
        }
        top
    }

    /// Whether an open cell is a pocket one block tall, with solid blocks right above and below.
    pub fn pocket(&mut self, x: i32, y: i32, z: i32) -> bool {
        self.solid(x, y + 1, z) > 0 && self.solid(x, y - 1, z) > 0
    }

    /// Under a roof, the lowest y of the rock lifted off this column: the solid blocks from the
    /// cut at `start_y` down to the first open space that isn't a pocket.
    pub fn lid(&mut self, x: i32, z: i32, start_y: i32, limit_y: i32) -> i32 {
        let mut y = start_y - 1;
        while y >= limit_y && (self.solid(x, y, z) > 0 || y + 1 < start_y && self.pocket(x, y, z)) {
            y -= 1;
        }
        y + 1
    }

    /// Whether this block is in the bottom of the rock lifted off its column, with open space below.
    pub fn ceiling_underside(&mut self, x: i32, y: i32, z: i32, lid: i32, limit_y: i32) -> bool {
        y >= lid && y < lid + CEILING_UNDERSIDE && lid > limit_y && self.solid(x, lid - 1, z) == 0
    }
}

/// How much of a ceiling's underside a ray that has left the rock passes through.
pub const CEILING_UNDERSIDE: i32 = 2;
