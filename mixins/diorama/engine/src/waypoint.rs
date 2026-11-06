//! Marked places: the player's own waypoints and the pins the map adds by itself where they died.
//! Kept per world in one text file, every dimension's together, and saved after each change.
//!
//! The file is one waypoint a line, its fields split by tabs, after a header line naming the
//! format. Text fields escape `\`, tab and newline, so any name round-trips.

use crate::view::View;
use std::path::{Path, PathBuf};

const HEADER: &str = "diorama-waypoints 1";

/// How many death pins to keep; older ones are dropped as new deaths come in.
pub const DEATHS_KEPT: usize = 3;
/// The most letters a pin shows.
pub const MAX_INITIALS: usize = 4;

/// Colours a waypoint can take, 0xAARRGGBB, picked to stay readable on both grass and water.
pub const PALETTE: [u32; 16] = [
    0xFFE5484D, 0xFFF76B15, 0xFFFFC53D, 0xFF46A758, 0xFF12A594, 0xFF0090FF, 0xFF3E63DD, 0xFF8E4EC6,
    0xFFD6409F, 0xFFFFFFFF, 0xFFB0B4BA, 0xFF60646C, 0xFF8D6B4B, 0xFF99D52A, 0xFF7CE2FE, 0xFFF0C0D8,
];
/// The palette's first nine are the bright ones new waypoints take turns with.
const BRIGHT: usize = 9;
/// A death pin's colour: slate.
pub const DEATH_COLOR: u32 = 0xFF60646C;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Manual,
    /// Added where the player died.
    Death,
    /// Dropped from the map for a moment: never saved, gone once the player leaves the world.
    Temporary,
}

impl Kind {
    fn id(self) -> &'static str {
        match self {
            Kind::Manual => "manual",
            Kind::Death => "death",
            Kind::Temporary => "temporary",
        }
    }

    fn of(id: &str) -> Option<Kind> {
        match id {
            "manual" => Some(Kind::Manual),
            "death" => Some(Kind::Death),
            "temporary" => Some(Kind::Temporary),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Waypoint {
    pub name: String,
    /// The letters on its pin; empty for the name's own.
    pub initials: String,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// Dimension id, `minecraft:overworld`.
    pub dimension: String,
    /// 0xAARRGGBB.
    pub color: u32,
    pub visible: bool,
    pub kind: Kind,
    /// Milliseconds since the Unix epoch.
    pub created: u64,
}

impl Waypoint {
    pub fn new(name: &str, at: [i32; 3], dimension: &str, color: u32) -> Waypoint {
        Waypoint {
            name: name.to_owned(),
            initials: String::new(),
            x: at[0],
            y: at[1],
            z: at[2],
            dimension: dimension.to_owned(),
            color,
            visible: true,
            kind: Kind::Manual,
            created: now_millis(),
        }
    }

    /// The letters its pin shows.
    pub fn label(&self) -> String {
        if self.initials.is_empty() {
            initials_for(&self.name)
        } else {
            self.initials.chars().take(MAX_INITIALS).collect()
        }
    }

    pub fn distance_to(&self, p: [f64; 3]) -> f64 {
        let dx = f64::from(self.x) + 0.5 - p[0];
        let dy = f64::from(self.y) - p[1];
        let dz = f64::from(self.z) + 0.5 - p[2];
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    /// Where its pin points: the top middle of its block.
    pub fn point(&self) -> [f64; 3] {
        [
            f64::from(self.x) + 0.5,
            f64::from(self.y),
            f64::from(self.z) + 0.5,
        ]
    }

    fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{:08X}\t{}\t{}\t{}\t{}\t{}",
            self.kind.id(),
            self.x,
            self.y,
            self.z,
            self.color,
            u8::from(self.visible),
            self.created,
            escape(&self.dimension),
            escape(&self.initials),
            escape(&self.name),
        )
    }

    fn parse(line: &str) -> Option<Waypoint> {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 10 {
            return None;
        }
        Some(Waypoint {
            kind: Kind::of(f[0])?,
            x: f[1].parse().ok()?,
            y: f[2].parse().ok()?,
            z: f[3].parse().ok()?,
            color: u32::from_str_radix(f[4], 16).ok()?,
            visible: f[5] != "0",
            created: f[6].parse().ok()?,
            dimension: unescape(f[7]),
            initials: unescape(f[8]),
            name: unescape(f[9]),
        })
    }
}

/// First letter of up to two words: "Home" is H, "Iron Farm" is IF.
pub fn initials_for(name: &str) -> String {
    let out: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .take(2)
        .collect();
    if out.is_empty() { "?".to_owned() } else { out }
}

/// The name of a death pin made at `millis` since the epoch, by the clock's UTC offset
/// `offset_minutes`: `Death 14:05`.
pub fn death_name(millis: u64, offset_minutes: i32) -> String {
    let minutes = (millis / 60_000) as i64 + i64::from(offset_minutes);
    let of_day = minutes.rem_euclid(24 * 60);
    format!("Death {:02}:{:02}", of_day / 60, of_day % 60)
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

/// One world's waypoints and the file they're saved in.
#[derive(Debug)]
pub struct Waypoints {
    path: PathBuf,
    list: Vec<Waypoint>,
}

impl Waypoints {
    /// The waypoints saved at `path`; none when there is no file yet. Lines that don't parse are
    /// skipped, so one damaged line loses one waypoint.
    pub fn load(path: impl Into<PathBuf>) -> Waypoints {
        let path = path.into();
        let list = std::fs::read_to_string(&path)
            .ok()
            .filter(|text| text.lines().next() == Some(HEADER))
            .map(|text| text.lines().skip(1).filter_map(Waypoint::parse).collect())
            .unwrap_or_default();
        Waypoints { path, list }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn all(&self) -> &[Waypoint] {
        &self.list
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The visible waypoints in `dimension`, with their indices.
    pub fn shown_in<'a>(
        &'a self,
        dimension: &'a str,
    ) -> impl Iterator<Item = (usize, &'a Waypoint)> + 'a {
        self.list
            .iter()
            .enumerate()
            .filter(move |(_, w)| w.visible && w.dimension == dimension)
    }

    /// A new waypoint at `at`, named and coloured so it differs from the last one. Not added yet.
    pub fn next_at(&self, at: [i32; 3], dimension: &str) -> Waypoint {
        let count = self.list.iter().filter(|w| w.kind == Kind::Manual).count() + 1;
        let last = self
            .list
            .iter()
            .rev()
            .find(|w| w.kind == Kind::Manual)
            .and_then(|w| PALETTE[..BRIGHT].iter().position(|c| *c == w.color));
        let color = PALETTE[last.map_or(0, |i| (i + 1) % BRIGHT)];
        Waypoint::new(&format!("Waypoint {count}"), at, dimension, color)
    }

    /// Adds a waypoint and saves. A death pin pushes out the oldest past [`DEATHS_KEPT`].
    pub fn add(&mut self, waypoint: Waypoint) -> std::io::Result<()> {
        let death = waypoint.kind == Kind::Death;
        self.list.push(waypoint);
        if death {
            let mut deaths: Vec<(u64, usize)> = self
                .list
                .iter()
                .enumerate()
                .filter(|(_, w)| w.kind == Kind::Death)
                .map(|(i, w)| (w.created, i))
                .collect();
            if deaths.len() > DEATHS_KEPT {
                deaths.sort_unstable();
                let mut drop: Vec<usize> = deaths[..deaths.len() - DEATHS_KEPT]
                    .iter()
                    .map(|(_, i)| *i)
                    .collect();
                drop.sort_unstable_by(|a, b| b.cmp(a));
                for i in drop {
                    self.list.remove(i);
                }
            }
        }
        self.save()
    }

    /// Removes waypoint `index` and saves; the removed one, if there was such.
    pub fn remove(&mut self, index: usize) -> std::io::Result<Option<Waypoint>> {
        if index >= self.list.len() {
            return Ok(None);
        }
        let w = self.list.remove(index);
        self.save().map(|()| Some(w))
    }

    /// Changes waypoint `index` with `edit` and saves.
    pub fn edit(&mut self, index: usize, edit: impl FnOnce(&mut Waypoint)) -> std::io::Result<()> {
        if let Some(w) = self.list.get_mut(index) {
            edit(w);
            return self.save();
        }
        Ok(())
    }

    /// Writes every waypoint but the temporary ones, beside the file then over it, so a crash
    /// never leaves half a list.
    pub fn save(&self) -> std::io::Result<()> {
        let mut text = String::from(HEADER);
        text.push('\n');
        for w in self.list.iter().filter(|w| w.kind != Kind::Temporary) {
            text.push_str(&w.line());
            text.push('\n');
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// Where a pin lands in a view's image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PinSpot {
    pub x: f64,
    pub y: f64,
    /// Out of view and held at its edge, pointing the way.
    pub edge: bool,
}

/// Where a pin for map point `(u, v)` goes in `view`'s image: where the point is when it is inside,
/// at least `inset` pixels from the edge; else, when `clamp`, on that inner edge (a circle's for a
/// round view) toward the point, and nowhere otherwise.
pub fn pin_spot(view: &View, u: f64, v: f64, inset: f64, clamp: bool) -> Option<PinSpot> {
    let (x, y) = view.image_xy(u, v);
    let (w, h) = (view.width as f64, view.height as f64);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (dx, dy) = (x - cx, y - cy);
    if view.round {
        let r = (w.min(h) / 2.0 - inset).max(0.0);
        let d = (dx * dx + dy * dy).sqrt();
        if d <= r {
            return Some(PinSpot { x, y, edge: false });
        }
        return clamp.then(|| PinSpot {
            x: cx + dx / d * r,
            y: cy + dy / d * r,
            edge: true,
        });
    }
    let (hx, hy) = ((cx - inset).max(0.0), (cy - inset).max(0.0));
    if dx.abs() <= hx && dy.abs() <= hy {
        return Some(PinSpot { x, y, edge: false });
    }
    if !clamp {
        return None;
    }
    // Along the line from the middle, to where it leaves the inner box.
    let t = (hx / dx.abs()).min(hy / dy.abs());
    Some(PinSpot {
        x: cx + dx * t,
        y: cy + dy * t,
        edge: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::TileKey;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("diorama-waypoints-{name}-{}", std::process::id()))
            .join("waypoints.txt")
    }

    #[test]
    fn initials() {
        assert_eq!(initials_for("Home"), "H");
        assert_eq!(initials_for("iron farm east"), "IF");
        assert_eq!(initials_for("  "), "?");
        let mut w = Waypoint::new("Base", [0, 0, 0], "minecraft:overworld", PALETTE[0]);
        assert_eq!(w.label(), "B");
        w.initials = "ABCDEF".into();
        assert_eq!(w.label(), "ABCD");
    }

    #[test]
    fn saves_and_loads_any_name() {
        let path = temp("rt");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut ws = Waypoints::load(&path);
        assert!(ws.is_empty());
        let mut w = ws.next_at([1, 64, -3], "minecraft:overworld");
        w.name = "Tab\there\\ and\nnewline".into();
        w.initials = "T".into();
        ws.add(w.clone()).unwrap();
        let mut t = ws.next_at([5, 5, 5], "minecraft:overworld");
        t.kind = Kind::Temporary;
        ws.add(t).unwrap();
        let back = Waypoints::load(&path);
        assert_eq!(back.all(), &[w], "temporary ones are not saved");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn new_waypoints_take_turns_with_colours() {
        let path = temp("colours");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut ws = Waypoints::load(&path);
        let a = ws.next_at([0, 0, 0], "d");
        assert_eq!((a.name.as_str(), a.color), ("Waypoint 1", PALETTE[0]));
        ws.add(a).unwrap();
        let b = ws.next_at([0, 0, 0], "d");
        assert_eq!((b.name.as_str(), b.color), ("Waypoint 2", PALETTE[1]));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn keeps_the_newest_deaths() {
        let path = temp("deaths");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut ws = Waypoints::load(&path);
        ws.add(Waypoint::new("Home", [0, 0, 0], "d", PALETTE[0]))
            .unwrap();
        for i in 0..5u64 {
            let mut d = Waypoint::new(&format!("Death {i}"), [0, 0, 0], "d", DEATH_COLOR);
            d.kind = Kind::Death;
            d.created = 1000 + i;
            ws.add(d).unwrap();
        }
        let names: Vec<&str> = ws.all().iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["Home", "Death 2", "Death 3", "Death 4"]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn damaged_lines_are_skipped() {
        let path = temp("damaged");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let good = Waypoint::new("Ok", [1, 2, 3], "d", PALETTE[2]);
        std::fs::write(
            &path,
            format!("{HEADER}\nnonsense\n{}\nmanual\t1\t2\n", good.line()),
        )
        .unwrap();
        assert_eq!(Waypoints::load(&path).all(), &[good]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn death_names_read_the_clock() {
        // 1970-01-01 13:07 UTC, shown two hours ahead.
        assert_eq!(death_name((13 * 60 + 7) * 60_000, 120), "Death 15:07");
        assert_eq!(death_name(0, -30), "Death 23:30");
    }

    fn view(round: bool) -> View {
        View {
            center_u: 0.0,
            center_v: 0.0,
            zoom: 1.0,
            layer: TileKey::SURFACE,
            lod: 0,
            width: 100,
            height: 100,
            round,
        }
    }

    #[test]
    fn pins_inside_stay_and_outside_hold_the_edge() {
        let sq = view(false);
        assert_eq!(
            pin_spot(&sq, 10.0, -20.0, 5.0, true),
            Some(PinSpot {
                x: 60.0,
                y: 30.0,
                edge: false
            })
        );
        let e = pin_spot(&sq, 200.0, 100.0, 5.0, true).unwrap();
        assert!(e.edge);
        assert!((e.x - 95.0).abs() < 1e-9 && (e.y - 72.5).abs() < 1e-9);
        assert_eq!(pin_spot(&sq, 200.0, 0.0, 5.0, false), None);
        let round = view(true);
        let r = pin_spot(&round, 0.0, -300.0, 5.0, true).unwrap();
        assert!(r.edge && (r.x - 50.0).abs() < 1e-9 && (r.y - 5.0).abs() < 1e-9);
        // A corner of the square is outside the circle.
        assert!(pin_spot(&round, 48.0, 48.0, 0.0, true).unwrap().edge);
    }
}
