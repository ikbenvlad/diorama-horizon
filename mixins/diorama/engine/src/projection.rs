//! The oblique camera every map pixel is traced through.
//!
//! The camera looks north and down at the style's pitch from the horizon, so the map shows block
//! tops and their south faces. Map space is measured in blocks:
//!
//! - `u = x` (east is right)
//! - `v = z * sin - y * cos` (south is down, and higher blocks move up the screen)
//!
//! That x and z are in turned space; see [`Facing`].

/// Map units per tile edge.
pub const TILE_UNITS: i32 = 128;
/// Texture pixels per map unit.
pub const PIXELS_PER_UNIT: i32 = 4;
/// Texture pixels per tile edge.
pub const TILE_PIXELS: i32 = TILE_UNITS * PIXELS_PER_UNIT;
/// Fog coverage cells per tile edge.
pub const COVERAGE_CELLS: i32 = 64;

/// Which way the map looks. The baker only knows a camera looking north; another facing turns the
/// world under it, so it sees the world's other side. Turned space is what it works in: the map's
/// u and v are its x and tilted z.
///
/// Whole blocks turn about a corner, so a chunk lands exactly on a chunk: a block at `-x - 1` fills
/// the place a point at `-x` reaches to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Facing {
    #[default]
    North,
    East,
    South,
    West,
}

impl Facing {
    pub const ALL: [Facing; 4] = [Facing::North, Facing::East, Facing::South, Facing::West];

    /// Quarter turns from north.
    pub fn turns(self) -> usize {
        self as usize
    }

    /// The word for this facing in folder and file names.
    pub fn id(self) -> &'static str {
        match self {
            Facing::North => "north",
            Facing::East => "east",
            Facing::South => "south",
            Facing::West => "west",
        }
    }

    pub fn of(id: &str) -> Facing {
        Facing::ALL
            .into_iter()
            .find(|f| f.id() == id)
            .unwrap_or(Facing::North)
    }

    pub fn next(self, steps: i32) -> Facing {
        Facing::ALL[(self as i32 + steps).rem_euclid(4) as usize]
    }

    /// The block in turned space of a world block, or a chunk: they turn alike.
    pub fn block_x(self, x: i32, z: i32) -> i32 {
        match self {
            Facing::North => x,
            Facing::East => z,
            Facing::South => -x - 1,
            Facing::West => -z - 1,
        }
    }

    pub fn block_z(self, x: i32, z: i32) -> i32 {
        match self {
            Facing::North => z,
            Facing::East => -x - 1,
            Facing::South => -z - 1,
            Facing::West => x,
        }
    }

    /// The world's block or chunk at a place in turned space.
    pub fn world_block_x(self, x: i32, z: i32) -> i32 {
        match self {
            Facing::North => x,
            Facing::East => -z - 1,
            Facing::South => -x - 1,
            Facing::West => z,
        }
    }

    pub fn world_block_z(self, x: i32, z: i32) -> i32 {
        match self {
            Facing::North => z,
            Facing::East => x,
            Facing::South => -z - 1,
            Facing::West => -x - 1,
        }
    }

    /// A point of the world in turned space.
    pub fn x(self, x: f64, z: f64) -> f64 {
        match self {
            Facing::North => x,
            Facing::East => z,
            Facing::South => -x,
            Facing::West => -z,
        }
    }

    pub fn z(self, x: f64, z: f64) -> f64 {
        match self {
            Facing::North => z,
            Facing::East => -x,
            Facing::South => -z,
            Facing::West => x,
        }
    }

    /// A box inside a unit cell, `[min x, y, z, max x, y, z]`, in turned space.
    pub fn turn_box(self, b: [f32; 6]) -> [f32; 6] {
        let [x0, y0, z0, x1, y1, z1] = b;
        match self {
            Facing::North => b,
            Facing::East => [z0, y0, 1.0 - x1, z1, y1, 1.0 - x0],
            Facing::South => [1.0 - x1, y0, 1.0 - z1, 1.0 - x0, y1, 1.0 - z0],
            Facing::West => [1.0 - z1, y0, x0, 1.0 - z0, y1, x1],
        }
    }
}

/// The camera: its pitch and which way it faces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Projection {
    pub sin: f64,
    pub cos: f64,
    pub facing: Facing,
}

impl Projection {
    pub fn new(pitch_degrees: f64, facing: Facing) -> Self {
        let r = pitch_degrees.to_radians();
        Self {
            sin: r.sin(),
            cos: r.cos(),
            facing,
        }
    }

    /// The map row of a height and z in turned space.
    pub fn v_turned(&self, y: f64, z: f64) -> f64 {
        z * self.sin - y * self.cos
    }

    /// The map column of a place in the world.
    pub fn u(&self, x: f64, z: f64) -> f64 {
        self.facing.x(x, z)
    }

    /// The map row of a place in the world.
    pub fn v(&self, x: f64, y: f64, z: f64) -> f64 {
        self.v_turned(y, self.facing.z(x, z))
    }

    /// The z in turned space where a ray through map row `v` crosses height `y`.
    pub fn z_at(&self, v: f64, y: f64) -> f64 {
        (v + y * self.cos) / self.sin
    }

    /// The world point on the ground at height `y` under map point `(u, v)`.
    pub fn world_at(&self, u: f64, v: f64, y: f64) -> (f64, f64) {
        let tz = self.z_at(v, y);
        // Undo the turn: turned (u, tz) back to world (x, z).
        match self.facing {
            Facing::North => (u, tz),
            Facing::East => (-tz, u),
            Facing::South => (-u, -tz),
            Facing::West => (tz, -u),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_turn_and_turn_back() {
        for f in Facing::ALL {
            for (x, z) in [(0, 0), (5, -3), (-17, 31), (-1, -1)] {
                let (tx, tz) = (f.block_x(x, z), f.block_z(x, z));
                assert_eq!(
                    (f.world_block_x(tx, tz), f.world_block_z(tx, tz)),
                    (x, z),
                    "{f:?}"
                );
            }
        }
    }

    #[test]
    fn points_agree_with_blocks() {
        // The point at a block's middle turns to the turned block's middle.
        for f in Facing::ALL {
            for (x, z) in [(0, 0), (5, -3), (-17, 31)] {
                let (px, pz) = (x as f64 + 0.5, z as f64 + 0.5);
                assert_eq!(f.x(px, pz).floor() as i32, f.block_x(x, z), "{f:?}");
                assert_eq!(f.z(px, pz).floor() as i32, f.block_z(x, z), "{f:?}");
            }
        }
    }

    #[test]
    fn world_at_inverts_projection() {
        for f in Facing::ALL {
            let p = Projection::new(50.0, f);
            let (x, y, z) = (12.25, 70.0, -40.5);
            let (wx, wz) = p.world_at(p.u(x, z), p.v(x, y, z), y);
            assert!((wx - x).abs() < 1e-9 && (wz - z).abs() < 1e-9, "{f:?}");
        }
    }

    #[test]
    fn turned_boxes_stay_in_the_cell() {
        let slab = [0.0, 0.0, 0.0, 1.0, 0.5, 0.25];
        for f in Facing::ALL {
            let b = f.turn_box(slab);
            assert!(b[0] < b[3] && b[2] < b[5]);
            assert!(b.iter().all(|v| (0.0..=1.0).contains(v)));
            assert_eq!((b[1], b[4]), (0.0, 0.5));
        }
        // A strip along the north side, seen from the south, is on the far (south) side of the cell.
        assert_eq!(
            Facing::South.turn_box(slab),
            [0.0, 0.0, 0.75, 1.0, 0.5, 1.0]
        );
    }
}
