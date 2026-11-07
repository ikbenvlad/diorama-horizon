//! Other players, mobs and dropped items on the map, as dots coloured by how they treat the
//! player: hostile red, neutral yellow, passive green, players white and items aqua. The map is a
//! view from above at an angle and can't show height by itself, so one well above or below the
//! player gets a mark saying which.

use crate::projection::Projection;
use crate::view::View;

/// Height difference, in blocks, past which a dot says it is above or below.
pub const CARET_HEIGHT: f64 = 3.0;

/// What a dot is, in drawing order: later kinds are drawn over earlier ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Item,
    Passive,
    Neutral,
    Hostile,
    Player,
}

impl Kind {
    /// 0xAARRGGBB.
    pub fn color(self) -> u32 {
        match self {
            Kind::Item => 0xFF55_FFFF,
            Kind::Passive => 0xFF62_D96B,
            Kind::Neutral => 0xFFF2_C14E,
            Kind::Hostile => 0xFFEF_4B4B,
            Kind::Player => 0xFFFF_FFFF,
        }
    }
}

/// Mobs that only attack when provoked (vanilla's `NeutralMob`s).
const NEUTRAL: [&str; 6] = [
    "minecraft:bee",
    "minecraft:enderman",
    "minecraft:iron_golem",
    "minecraft:polar_bear",
    "minecraft:wolf",
    "minecraft:zombified_piglin",
];
/// Mobs in the `misc` spawn category that are still mobs, and peaceful.
const MISC_PASSIVE: [&str; 3] = [
    "minecraft:villager",
    "minecraft:wandering_trader",
    "minecraft:snow_golem",
];

/// The kind of an entity type `id` in spawn category `category`; none for what the radar doesn't
/// show (boats, arrows, armour stands...).
pub fn kind(id: &str, category: &str) -> Option<Kind> {
    if id == "minecraft:player" {
        return Some(Kind::Player);
    }
    if id == "minecraft:item" {
        return Some(Kind::Item);
    }
    if NEUTRAL.contains(&id) {
        return Some(Kind::Neutral);
    }
    match category {
        "monster" => Some(Kind::Hostile),
        "creature"
        | "ambient"
        | "axolotls"
        | "underground_water_creature"
        | "water_creature"
        | "water_ambient" => Some(Kind::Passive),
        _ => MISC_PASSIVE.contains(&id).then_some(Kind::Passive),
    }
}

/// An entity to show.
#[derive(Clone, Debug, PartialEq)]
pub struct Entity {
    pub kind: Kind,
    /// World position, at its feet.
    pub pos: [f64; 3],
    /// A player's name.
    pub name: Option<String>,
}

/// An entity placed in a view's image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blip {
    /// Its index in the entities given.
    pub index: usize,
    pub kind: Kind,
    pub x: f64,
    pub y: f64,
    /// Its height above the player's (below when negative).
    pub dy: f64,
}

/// The entities that show in `view`, within `range` blocks above or below `player_y`, in drawing
/// order. `margin` image pixels past the edges still count, so dots don't pop at the border; a
/// round view keeps only those inside its circle.
pub fn place(
    view: &View,
    projection: &Projection,
    entities: &[Entity],
    player_y: f64,
    range: f64,
    margin: f64,
) -> Vec<Blip> {
    let (w, h) = (view.width as f64, view.height as f64);
    let r = w.min(h) / 2.0;
    let mut out: Vec<Blip> = entities
        .iter()
        .enumerate()
        .filter_map(|(index, e)| {
            let dy = e.pos[1] - player_y;
            if dy.abs() > range {
                return None;
            }
            let [x, y, z] = e.pos;
            let (ix, iy) = view.image_xy(projection.u(x, z), projection.v(x, y, z));
            let inside = if view.round {
                (ix - w / 2.0).hypot(iy - h / 2.0) <= r
            } else {
                (-margin..=w + margin).contains(&ix) && (-margin..=h + margin).contains(&iy)
            };
            inside.then_some(Blip {
                index,
                kind: e.kind,
                x: ix,
                y: iy,
                dy,
            })
        })
        .collect();
    out.sort_by_key(|b| b.kind);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::Facing;
    use crate::tile::TileKey;

    #[test]
    fn kinds() {
        assert_eq!(kind("minecraft:zombie", "monster"), Some(Kind::Hostile));
        assert_eq!(kind("minecraft:enderman", "monster"), Some(Kind::Neutral));
        assert_eq!(kind("minecraft:cow", "creature"), Some(Kind::Passive));
        assert_eq!(kind("minecraft:villager", "misc"), Some(Kind::Passive));
        assert_eq!(kind("minecraft:boat", "misc"), None);
        assert_eq!(kind("minecraft:player", "misc"), Some(Kind::Player));
        assert_eq!(kind("minecraft:item", "misc"), Some(Kind::Item));
    }

    #[test]
    fn places_in_view_and_range_players_last() {
        let p = Projection::new(50.0, Facing::North);
        let (u, v) = (p.u(0.5, 0.5), p.v(0.5, 64.0, 0.5));
        let view = View {
            center_u: u,
            center_v: v,
            zoom: 2.0,
            layer: TileKey::SURFACE,
            lod: 0,
            width: 100,
            height: 100,
            round: false,
        };
        let e = |kind, pos| Entity {
            kind,
            pos,
            name: None,
        };
        let entities = [
            e(Kind::Player, [5.5, 64.0, 0.5]),
            e(Kind::Hostile, [0.5, 64.0, 5.5]),
            e(Kind::Hostile, [0.5, 120.0, 0.5]),
            e(Kind::Passive, [500.0, 64.0, 0.5]),
        ];
        let blips = place(&view, &p, &entities, 64.0, 32.0, 4.0);
        assert_eq!(
            blips.iter().map(|b| b.index).collect::<Vec<_>>(),
            vec![1, 0]
        );
        // East is right, at two pixels a block.
        assert!((blips[1].x - 60.0).abs() < 1e-9 && (blips[1].y - 50.0).abs() < 1e-9);
        let round = View {
            round: true,
            ..view
        };
        let corner = [e(Kind::Item, [24.0, 64.0, 0.5 + 24.0 / p.sin])];
        assert!(place(&view, &p, &corner, 64.0, 32.0, 4.0).len() == 1);
        assert!(place(&round, &p, &corner, 64.0, 32.0, 4.0).is_empty());
    }
}
