//! A private copy of one chunk's blocks, light and biome tints, safe to read from bake threads.
//! The game glue makes them on its own thread; sections it shares (`Arc`) cost nothing to copy.

use std::sync::Arc;

/// Blocks per section.
pub const SECTION_BLOCKS: usize = 4096;

/// The blocks of one 16x16x16 section, indexed `(y << 8) | (z << 4) | x`.
#[derive(Clone, Debug)]
pub enum Blocks {
    /// One state fills the section.
    Single(u32),
    /// Palette indices packed `bits` to an entry, `64 / bits` entries to a word (none spanning
    /// two words, as vanilla packs them), and the palette's state ids.
    Packed {
        bits: u8,
        data: Arc<[u64]>,
        palette: Arc<[u32]>,
    },
}

impl Blocks {
    /// Packs a section's 4096 state ids.
    pub fn from_ids(ids: &[u32]) -> Blocks {
        debug_assert_eq!(ids.len(), SECTION_BLOCKS);
        let first = ids[0];
        if ids.iter().all(|&i| i == first) {
            return Blocks::Single(first);
        }
        let mut palette: Vec<u32> = Vec::new();
        let mut index: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        let indices: Vec<u32> = ids
            .iter()
            .map(|&id| {
                *index.entry(id).or_insert_with(|| {
                    palette.push(id);
                    (palette.len() - 1) as u32
                })
            })
            .collect();
        let bits = (32 - (palette.len() as u32 - 1).leading_zeros()).max(1) as u8;
        let per = 64 / usize::from(bits);
        let mut data = vec![0u64; SECTION_BLOCKS.div_ceil(per)];
        for (i, v) in indices.into_iter().enumerate() {
            data[i / per] |= u64::from(v) << ((i % per) * usize::from(bits));
        }
        Blocks::Packed {
            bits,
            data: data.into(),
            palette: palette.into(),
        }
    }

    #[inline]
    pub fn get(&self, i: usize) -> u32 {
        match self {
            Blocks::Single(s) => *s,
            Blocks::Packed {
                bits,
                data,
                palette,
            } => {
                let bits = usize::from(*bits);
                let per = 64 / bits;
                let v = (data[i / per] >> ((i % per) * bits)) & ((1u64 << bits) - 1);
                palette[v as usize]
            }
        }
    }

    /// Every block's state id, into `out`.
    pub fn unpack(&self, out: &mut [u32; SECTION_BLOCKS]) {
        match self {
            Blocks::Single(s) => out.fill(*s),
            Blocks::Packed {
                bits,
                data,
                palette,
            } => {
                let bits = usize::from(*bits);
                let per = 64 / bits;
                let mask = (1u64 << bits) - 1;
                for (w, word) in data.iter().enumerate() {
                    let base = w * per;
                    for k in 0..per.min(SECTION_BLOCKS.saturating_sub(base)) {
                        out[base + k] = palette[((word >> (k * bits)) & mask) as usize];
                    }
                }
            }
        }
    }
}

/// A section's light, packed as Horizon keeps it: sky light in each byte's high nibble, block
/// light in the low one. Only the kind that varies is kept per block.
#[derive(Clone, Debug)]
pub enum Light {
    Uniform(u8),
    /// Sky light per block as nibbles (block `i` in byte `i / 2`, the even one in the low
    /// nibble), and one block light for all.
    Sky(Arc<[u8; 2048]>, u8),
    /// Block light per block as nibbles, and one sky light for all.
    Block(Arc<[u8; 2048]>, u8),
    Full(Arc<[u8; 4096]>),
}

#[inline]
fn nibble(layer: &[u8; 2048], i: usize) -> u8 {
    (layer[i >> 1] >> ((i & 1) << 2)) & 15
}

impl Light {
    /// Sky light only, the same everywhere, and no block light.
    pub fn sky(level: u8) -> Light {
        Light::Uniform(level << 4)
    }

    /// The packed light of block `i`.
    #[inline]
    pub fn get(&self, i: usize) -> u8 {
        match self {
            Light::Uniform(v) => *v,
            Light::Sky(layer, block) => (nibble(layer, i) << 4) | block,
            Light::Block(layer, sky) => (sky << 4) | nibble(layer, i),
            Light::Full(a) => a[i],
        }
    }

    #[inline]
    pub fn block(&self, i: usize) -> u8 {
        self.get(i) & 15
    }

    #[inline]
    pub fn sky_level(&self, i: usize) -> u8 {
        self.get(i) >> 4
    }
}

#[derive(Clone, Debug)]
pub struct Section {
    pub blocks: Blocks,
    pub light: Light,
}

impl Section {
    pub fn air(air: u32, sky: u8) -> Section {
        Section {
            blocks: Blocks::Single(air),
            light: Light::sky(sky),
        }
    }

    #[inline]
    pub fn state(&self, i: usize) -> u32 {
        self.blocks.get(i)
    }
}

/// Biome tints per column of a chunk, 0xRRGGBB, indexed `(z << 4) | x`.
#[derive(Clone, Debug)]
pub struct ColumnTints {
    pub grass: [u32; 256],
    pub foliage: [u32; 256],
    pub dry_foliage: [u32; 256],
    pub water: [u32; 256],
    /// The biome's fog colour, for the haze under a roof.
    pub fog: [u32; 256],
}

impl ColumnTints {
    /// Plains' colours everywhere.
    pub fn plains() -> ColumnTints {
        ColumnTints {
            grass: [0x91BD59; 256],
            foliage: [0x77AB2F; 256],
            dry_foliage: [0xA0A69C; 256],
            water: [0x3F76E4; 256],
            fog: [0xC0D8FF; 256],
        }
    }
}

/// One chunk, as of when the glue copied it.
#[derive(Clone, Debug)]
pub struct ChunkSnapshot {
    pub x: i32,
    pub z: i32,
    /// The section y (block y / 16) of `sections[0]`.
    pub min_section_y: i32,
    pub sections: Vec<Section>,
    pub tints: Arc<ColumnTints>,
    /// Per section, one above its highest non-air local y (0 when it is all air).
    section_tops: Vec<u8>,
    /// One above the highest non-air block, or `i32::MIN` when the chunk is empty.
    pub top_y: i32,
    /// One above the lowest column's highest non-air block: nothing of the chunk under it shows
    /// from the sky but through water or under overhangs. The chunk's bottom when a column is
    /// empty.
    pub floor_y: i32,
}

impl ChunkSnapshot {
    /// A snapshot of `sections` from `min_section_y` up. `is_air` tells which states are air.
    pub fn new(
        x: i32,
        z: i32,
        min_section_y: i32,
        sections: Vec<Section>,
        tints: Arc<ColumnTints>,
        is_air: impl Fn(u32) -> bool,
    ) -> ChunkSnapshot {
        let mut ids = [0u32; SECTION_BLOCKS];
        // Each column's surface, from the top down, until every column has one.
        let mut columns = [i32::MIN; 256];
        let mut open = 256;
        for (i, s) in sections.iter().enumerate().rev() {
            if open == 0 {
                break;
            }
            let base = (min_section_y + i as i32) * 16;
            match &s.blocks {
                Blocks::Single(id) if is_air(*id) => continue,
                Blocks::Single(_) => {
                    for c in columns.iter_mut().filter(|c| **c == i32::MIN) {
                        *c = base + 16;
                    }
                    open = 0;
                }
                Blocks::Packed { .. } => {
                    s.blocks.unpack(&mut ids);
                    for (col, c) in columns.iter_mut().enumerate() {
                        if *c != i32::MIN {
                            continue;
                        }
                        if let Some(ly) =
                            (0..16usize).rev().find(|ly| !is_air(ids[(ly << 8) | col]))
                        {
                            *c = base + ly as i32 + 1;
                            open -= 1;
                        }
                    }
                }
            }
        }
        let floor_y = if open > 0 {
            min_section_y * 16
        } else {
            columns.iter().copied().min().unwrap_or(min_section_y * 16)
        };
        let section_tops: Vec<u8> = sections
            .iter()
            .map(|s| match &s.blocks {
                Blocks::Single(id) => {
                    if is_air(*id) {
                        0
                    } else {
                        16
                    }
                }
                Blocks::Packed { palette, .. } => {
                    if palette.iter().all(|id| is_air(*id)) {
                        return 0;
                    }
                    s.blocks.unpack(&mut ids);
                    let mut top = 0u8;
                    for ly in (0..16usize).rev() {
                        if ids[ly << 8..(ly + 1) << 8].iter().any(|&id| !is_air(id)) {
                            top = ly as u8 + 1;
                            break;
                        }
                    }
                    top
                }
            })
            .collect();
        let top_y = section_tops
            .iter()
            .enumerate()
            .rev()
            .find(|(_, t)| **t > 0)
            .map_or(i32::MIN, |(i, t)| {
                (min_section_y + i as i32) * 16 + i32::from(*t)
            });
        ChunkSnapshot {
            x,
            z,
            min_section_y,
            sections,
            tints,
            section_tops,
            top_y,
            floor_y,
        }
    }

    pub fn min_y(&self) -> i32 {
        self.min_section_y * 16
    }

    pub fn max_y(&self) -> i32 {
        (self.min_section_y + self.sections.len() as i32) * 16
    }

    /// The section holding block y, if inside the chunk.
    #[inline]
    pub fn section(&self, y: i32) -> Option<&Section> {
        let i = (y >> 4) - self.min_section_y;
        if i < 0 {
            return None;
        }
        self.sections.get(i as usize)
    }

    /// Block state at chunk-local x, z and world y; `air` outside the chunk's height.
    pub fn state(&self, lx: i32, y: i32, lz: i32, air: u32) -> u32 {
        self.section(y).map_or(air, |s| s.state(index(lx, y, lz)))
    }

    /// One above the highest non-air block below `ceiling`, or `i32::MIN` if there is none.
    pub fn top_below(&self, ceiling: i32, is_air: impl Fn(u32) -> bool) -> i32 {
        if ceiling >= self.top_y {
            return self.top_y;
        }
        for (i, s) in self.sections.iter().enumerate().rev() {
            let top = self.section_tops[i];
            if top == 0 {
                continue;
            }
            let base = (self.min_section_y + i as i32) * 16;
            if base >= ceiling {
                continue;
            }
            if base + i32::from(top) <= ceiling {
                return base + i32::from(top);
            }
            // The ceiling cuts through this section's blocks.
            for ly in (0..(ceiling - base)).rev() {
                let row = (ly as usize) << 8;
                if (0..256).any(|j| !is_air(s.state(row | j))) {
                    return base + ly + 1;
                }
            }
        }
        i32::MIN
    }
}

/// A block's index in a section.
#[inline]
pub fn index(x: i32, y: i32, z: i32) -> usize {
    (((y & 15) << 8) | ((z & 15) << 4) | (x & 15)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tops() {
        let mut ids = vec![0u32; SECTION_BLOCKS];
        ids[index(3, 5, 7)] = 1;
        ids[index(0, 9, 0)] = 1;
        let sections = vec![
            Section {
                blocks: Blocks::Single(1),
                light: Light::Uniform(0),
            },
            Section {
                blocks: Blocks::from_ids(&ids),
                light: Light::sky(15),
            },
            Section::air(0, 15),
        ];
        let c = ChunkSnapshot::new(0, 0, -4, sections, Arc::new(ColumnTints::plains()), |id| {
            id == 0
        });
        assert_eq!(c.min_y(), -64);
        assert_eq!(c.max_y(), -16);
        assert_eq!(c.top_y, -48 + 10);
        // Most columns of the second section are air: the floor is the full first section's top.
        assert_eq!(c.floor_y, -48);
        assert_eq!(c.top_below(1000, |id| id == 0), -38);
        assert_eq!(c.top_below(-39, |id| id == 0), -48 + 6);
        assert_eq!(c.top_below(-48, |id| id == 0), -48);
        assert_eq!(c.top_below(-64, |id| id == 0), i32::MIN);
        assert_eq!(c.state(3, -43, 7, 0), 1);
        assert_eq!(c.state(3, 200, 7, 0), 0);
    }

    #[test]
    fn packing_round_trips() {
        for kinds in [2u32, 3, 5, 17, 300, 4096] {
            let ids: Vec<u32> = (0..SECTION_BLOCKS as u32)
                .map(|i| (i * 7919) % kinds + 100)
                .collect();
            let b = Blocks::from_ids(&ids);
            let mut out = [0u32; SECTION_BLOCKS];
            b.unpack(&mut out);
            assert_eq!(&out[..], &ids[..], "{kinds} kinds");
            assert_eq!(b.get(4095), ids[4095]);
        }
        assert!(matches!(
            Blocks::from_ids(&[9; SECTION_BLOCKS]),
            Blocks::Single(9)
        ));
    }

    #[test]
    fn light_nibbles() {
        let mut layer = [0u8; 2048];
        layer[1] = 0xA3; // block 2 is 3, block 3 is 10
        let l = Light::Sky(Arc::new(layer), 7);
        assert_eq!((l.sky_level(2), l.block(2)), (3, 7));
        assert_eq!(l.sky_level(3), 10);
        let b = Light::Block(Arc::new(layer), 15);
        assert_eq!((b.sky_level(3), b.block(3)), (15, 10));
    }
}
