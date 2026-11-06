//! What each block state looks like on the map: how rays treat it, and its average top and front
//! colours taken from the game's own textures. Built once per resource load by the game glue; after
//! that the table never changes, so bake threads read it freely.

use crate::projection::Facing;

/// How rays treat a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Kind {
    #[default]
    Air = 0,
    Solid = 1,
    Water = 2,
    Lava = 3,
    /// See-through blocks such as glass: rays pass and pick up a tint.
    Glass = 4,
    /// Plants without collision: rays pass and pick up partial colour.
    Plant = 5,
}

/// Where a tinted face takes its tint from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TintKind {
    #[default]
    None = 0,
    Grass = 1,
    Foliage = 2,
    DryFoliage = 3,
    Water = 4,
}

/// The look of every block state, indexed by state id.
#[derive(Clone, Debug, Default)]
pub struct BlockLooks {
    pub kind: Vec<Kind>,
    pub tint: Vec<TintKind>,
    pub emission: Vec<u8>,
    /// Untinted part of the top colour, 0xRRGGBB.
    pub top: Vec<u32>,
    /// Part of the top colour multiplied by the tint.
    pub top_tinted: Vec<u32>,
    pub top_tint_weight: Vec<f32>,
    pub side: Vec<u32>,
    pub side_tinted: Vec<u32>,
    pub side_tint_weight: Vec<f32>,
    /// Fixed tint for blocks whose tint doesn't depend on the biome (spruce leaves), else none.
    pub constant_tint: Vec<Option<u32>>,
    /// Index into `shapes` of a solid block's outline, or 0 for a full cube.
    pub shape: Vec<u32>,
    /// Outlines of solid blocks smaller than a cube, by facing: boxes as
    /// `[min x, y, z, max x, y, z]` in block units, turned into that facing's space. Entry 0 is unused.
    pub shapes: [Vec<Vec<[f32; 6]>>; 4],
    /// For a waterlogged block smaller than a cube, the state id of the water around it.
    pub wet_shape: Vec<Option<u32>>,
    /// The air state's id.
    pub air: u32,
}

impl BlockLooks {
    pub fn len(&self) -> usize {
        self.kind.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kind.is_empty()
    }

    /// The kind of a state; ids past the table read as air.
    #[inline]
    pub fn kind(&self, id: u32) -> Kind {
        self.kind.get(id as usize).copied().unwrap_or(Kind::Air)
    }

    /// The shape boxes of shape index `shape` seen from `facing`.
    #[inline]
    pub fn boxes(&self, facing: Facing, shape: u32) -> &[[f32; 6]] {
        &self.shapes[facing.turns()][shape as usize]
    }

    /// True when two states draw the same on the map: leaves changing their distance property, say.
    pub fn same_look(&self, a: u32, b: u32) -> bool {
        let (a, b) = (a as usize, b as usize);
        if a >= self.len() || b >= self.len() {
            return a == b;
        }
        self.kind[a] == self.kind[b]
            && self.top[a] == self.top[b]
            && self.top_tinted[a] == self.top_tinted[b]
            && self.side[a] == self.side[b]
            && self.side_tinted[a] == self.side_tinted[b]
            && self.top_tint_weight[a] == self.top_tint_weight[b]
            && self.side_tint_weight[a] == self.side_tint_weight[b]
            && self.tint[a] == self.tint[b]
            && self.constant_tint[a] == self.constant_tint[b]
            && self.emission[a] == self.emission[b]
            && self.shape[a] == self.shape[b]
            && self.wet_shape[a] == self.wet_shape[b]
    }
}

/// An average texture colour: sRGB channels 0..1 and how much of the texture is visible.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Average {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub coverage: f32,
}

impl Average {
    /// Averages RGBA pixels, ignoring nearly transparent ones and weighting by alpha. Stored sRGB
    /// values are averaged, as Xaero's World Map and JourneyMap do: in linear light bright
    /// pixels outweigh dark ones.
    pub fn of_rgba(rgba: &[u8]) -> Average {
        let (mut r, mut g, mut b, mut weight) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        let mut visible = 0usize;
        let count = rgba.len() / 4;
        for p in rgba.as_chunks::<4>().0 {
            let a = p[3];
            if a < 26 {
                continue;
            }
            let wa = f64::from(a) / 255.0;
            r += f64::from(p[0]) * wa;
            g += f64::from(p[1]) * wa;
            b += f64::from(p[2]) * wa;
            weight += wa;
            visible += 1;
        }
        if weight == 0.0 || count == 0 {
            return Average::default();
        }
        Average {
            r: (r / weight / 255.0) as f32,
            g: (g / weight / 255.0) as f32,
            b: (b / weight / 255.0) as f32,
            coverage: visible as f32 / count as f32,
        }
    }

    pub fn flat(rgb: u32) -> Average {
        Average {
            r: ((rgb >> 16) & 0xFF) as f32 / 255.0,
            g: ((rgb >> 8) & 0xFF) as f32 / 255.0,
            b: (rgb & 0xFF) as f32 / 255.0,
            coverage: 1.0,
        }
    }
}

/// Running average of the faces that make up one face direction.
#[derive(Clone, Copy, Debug, Default)]
pub struct FaceColor {
    ur: f32,
    ug: f32,
    ub: f32,
    uw: f32,
    tr: f32,
    tg: f32,
    tb: f32,
    tw: f32,
    coverage: f32,
    count: u32,
    /// The first tint a face asked for.
    tint: Option<FaceTint>,
}

/// What a tinted face is multiplied by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaceTint {
    Biome(TintKind),
    Fixed(u32),
}

impl FaceColor {
    pub fn add(&mut self, avg: Average, tint: Option<FaceTint>) {
        let w = avg.coverage;
        if w <= 0.0 {
            return;
        }
        if tint.is_some() {
            if self.tint.is_none() {
                self.tint = tint;
            }
            self.tr += avg.r * w;
            self.tg += avg.g * w;
            self.tb += avg.b * w;
            self.tw += w;
        } else {
            self.ur += avg.r * w;
            self.ug += avg.g * w;
            self.ub += avg.b * w;
            self.uw += w;
        }
        self.coverage = self.coverage.max(w);
        self.count += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn coverage(&self) -> f32 {
        self.coverage
    }

    fn untinted(&self) -> u32 {
        if self.uw == 0.0 {
            0
        } else {
            pack(self.ur / self.uw, self.ug / self.uw, self.ub / self.uw)
        }
    }

    fn tinted(&self) -> u32 {
        if self.tw == 0.0 {
            0
        } else {
            pack(self.tr / self.tw, self.tg / self.tw, self.tb / self.tw)
        }
    }

    fn tint_weight(&self) -> f32 {
        let total = self.uw + self.tw;
        if total == 0.0 { 0.0 } else { self.tw / total }
    }
}

fn pack(r: f32, g: f32, b: f32) -> u32 {
    let c = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
    (c(r) << 16) | (c(g) << 8) | c(b)
}

/// One block state as the game glue describes it.
#[derive(Clone, Debug, Default)]
pub struct StateInput {
    pub kind: Kind,
    pub emission: u8,
    /// Leaves stay solid however see-through their texture is.
    pub leaves: bool,
    /// The outline boxes in block space, for solid blocks smaller than a cube.
    pub outline: Vec<[f32; 6]>,
    pub top: FaceColor,
    pub side: FaceColor,
    /// The block's colour on vanilla maps (0xRRGGBB, 0 for none), for blocks with no texture.
    pub map_color: u32,
    /// For a waterlogged block, the id of the water state around it.
    pub wet: Option<u32>,
    /// Fluids: their still texture's colour and tint, applied to every side.
    pub fluid: Option<(u32, Option<FaceTint>)>,
}

/// A shape with more boxes than this counts as its bounding box.
const MAX_BOXES: usize = 16;

/// Builds the table from every state, in id order.
pub fn build(states: &[StateInput], air: u32) -> BlockLooks {
    let n = states.len();
    let mut looks = BlockLooks {
        kind: vec![Kind::Air; n],
        tint: vec![TintKind::None; n],
        emission: vec![0; n],
        top: vec![0; n],
        top_tinted: vec![0; n],
        top_tint_weight: vec![0.0; n],
        side: vec![0; n],
        side_tinted: vec![0; n],
        side_tint_weight: vec![0.0; n],
        constant_tint: vec![None; n],
        shape: vec![0; n],
        shapes: Default::default(),
        wet_shape: vec![None; n],
        air,
    };
    let mut shapes: Vec<Vec<[f32; 6]>> = vec![Vec::new()];
    let mut by_boxes: std::collections::HashMap<Vec<u32>, u32> = std::collections::HashMap::new();
    for (id, s) in states.iter().enumerate() {
        looks.emission[id] = s.emission;
        let mut kind = s.kind;
        if kind == Kind::Air {
            continue;
        }
        if kind == Kind::Solid {
            looks.shape[id] = shape_index(&s.outline, &mut shapes, &mut by_boxes);
        }
        let mut top = s.top;
        let mut side = s.side;
        if let Some((color, tint)) = s.fluid {
            let mut face = FaceColor::default();
            face.add(Average::flat(color), tint);
            top = face;
            side = face;
        }
        if top.is_empty() && side.is_empty() {
            if s.map_color == 0 {
                looks.kind[id] = Kind::Air;
                looks.shape[id] = 0;
                continue;
            }
            top.add(Average::flat(s.map_color), None);
            side = top;
        }
        if top.is_empty() {
            top = side;
        }
        if side.is_empty() {
            side = top;
        }
        looks.top[id] = top.untinted();
        looks.top_tinted[id] = top.tinted();
        looks.top_tint_weight[id] = top.tint_weight();
        looks.side[id] = side.untinted();
        looks.side_tinted[id] = side.tinted();
        looks.side_tint_weight[id] = side.tint_weight();
        if kind == Kind::Solid && !s.leaves && top.coverage() < 0.55 {
            kind = Kind::Glass;
        }
        looks.kind[id] = kind;
        match top.tint.or(side.tint) {
            Some(FaceTint::Biome(t)) => looks.tint[id] = t,
            Some(FaceTint::Fixed(c)) => looks.constant_tint[id] = Some(c),
            None => {}
        }
    }
    for (id, s) in states.iter().enumerate() {
        if looks.shape[id] != 0
            && let Some(w) = s.wet
            && looks.kind(w) == Kind::Water
        {
            looks.wet_shape[id] = Some(w);
        }
    }
    for f in Facing::ALL {
        looks.shapes[f.turns()] = shapes
            .iter()
            .map(|boxes| boxes.iter().map(|b| f.turn_box(*b)).collect())
            .collect();
    }
    looks
}

/// Finds or adds an outline among `shapes`; 0 for a full cube or no outline.
fn shape_index(
    outline: &[[f32; 6]],
    shapes: &mut Vec<Vec<[f32; 6]>>,
    by_boxes: &mut std::collections::HashMap<Vec<u32>, u32>,
) -> u32 {
    if outline.is_empty() {
        return 0;
    }
    let bounds;
    let boxes: &[[f32; 6]] = if outline.len() > MAX_BOXES {
        let mut b = [1.0f32, 1.0, 1.0, 0.0, 0.0, 0.0];
        for o in outline {
            for a in 0..3 {
                b[a] = b[a].min(o[a]);
                b[a + 3] = b[a + 3].max(o[a + 3]);
            }
        }
        bounds = [b];
        &bounds
    } else {
        outline
    };
    let mut kept = Vec::with_capacity(boxes.len());
    let mut full = false;
    for b in boxes {
        // Some mods' outlines reach into the next block; only the part in this one counts.
        let b = b.map(|v| v.clamp(0.0, 1.0));
        if b[0] >= b[3] || b[1] >= b[4] || b[2] >= b[5] {
            continue;
        }
        full |= b == [0.0, 0.0, 0.0, 1.0, 1.0, 1.0];
        kept.push(b);
    }
    if full || kept.is_empty() {
        return 0;
    }
    let key: Vec<u32> = kept.iter().flatten().map(|v| v.to_bits()).collect();
    *by_boxes.entry(key).or_insert_with(|| {
        shapes.push(kept);
        (shapes.len() - 1) as u32
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small table for tests: 0 air, 1 stone, 2 grass block, 3 water, 4 bottom slab,
    /// 5 glass, 6 short grass, 7 lava, 8 glowstone, 9 waterlogged slab.
    pub fn test_looks() -> BlockLooks {
        let solid = |rgb: u32| {
            let mut f = FaceColor::default();
            f.add(Average::flat(rgb), None);
            f
        };
        let tinted = |rgb: u32, t: TintKind| {
            let mut f = FaceColor::default();
            f.add(Average::flat(rgb), Some(FaceTint::Biome(t)));
            f
        };
        let slab = vec![[0.0, 0.0, 0.0, 1.0, 0.5, 1.0]];
        let states = vec![
            StateInput::default(),
            StateInput {
                kind: Kind::Solid,
                top: solid(0x7F7F7F),
                ..Default::default()
            },
            StateInput {
                kind: Kind::Solid,
                top: tinted(0x9A9A9A, TintKind::Grass),
                side: solid(0x86603F),
                ..Default::default()
            },
            StateInput {
                kind: Kind::Water,
                fluid: Some((0xB0B0B0, Some(FaceTint::Biome(TintKind::Water)))),
                ..Default::default()
            },
            StateInput {
                kind: Kind::Solid,
                outline: slab.clone(),
                top: solid(0xA0A0A0),
                ..Default::default()
            },
            StateInput {
                kind: Kind::Solid,
                top: {
                    let mut f = FaceColor::default();
                    f.add(
                        Average {
                            coverage: 0.2,
                            ..Average::flat(0xDDEEFF)
                        },
                        None,
                    );
                    f
                },
                ..Default::default()
            },
            StateInput {
                kind: Kind::Plant,
                top: tinted(0x909090, TintKind::Grass),
                ..Default::default()
            },
            StateInput {
                kind: Kind::Lava,
                emission: 15,
                fluid: Some((0xD45A12, None)),
                ..Default::default()
            },
            StateInput {
                kind: Kind::Solid,
                emission: 15,
                top: solid(0xFFD07A),
                ..Default::default()
            },
            StateInput {
                kind: Kind::Solid,
                outline: slab,
                top: solid(0xA0A0A0),
                wet: Some(3),
                ..Default::default()
            },
        ];
        build(&states, 0)
    }

    #[test]
    fn builds_kinds_shapes_and_tints() {
        let l = test_looks();
        assert_eq!(l.kind(0), Kind::Air);
        assert_eq!(l.kind(1), Kind::Solid);
        assert_eq!(l.kind(5), Kind::Glass, "mostly clear texture");
        assert_eq!(l.tint[2], TintKind::Grass);
        assert_eq!(l.top_tint_weight[2], 1.0);
        assert_eq!(l.side_tint_weight[2], 0.0);
        assert_eq!(l.tint[3], TintKind::Water);
        assert_eq!(l.shape[1], 0);
        assert_eq!(l.shape[4], 1);
        assert_eq!(l.shape[9], 1, "same outline, same shape");
        assert_eq!(l.wet_shape[9], Some(3));
        assert_eq!(l.wet_shape[4], None);
        assert_eq!(l.boxes(Facing::North, 1), &[[0.0, 0.0, 0.0, 1.0, 0.5, 1.0]]);
        assert!(l.same_look(4, 4));
        assert!(!l.same_look(1, 4));
    }

    #[test]
    fn averages_ignore_clear_pixels() {
        let px = [255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 9, 9, 9, 10];
        let a = Average::of_rgba(&px);
        assert!((a.r - 0.5).abs() < 1e-6 && (a.b - 0.5).abs() < 1e-6 && a.g == 0.0);
        assert_eq!(a.coverage, 0.5);
    }
}
