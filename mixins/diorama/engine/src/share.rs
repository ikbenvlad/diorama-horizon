//! Waypoints shared in chat, in the line format Xaero's Minimap uses, so players of either mod can
//! add each other's: `xaero-waypoint:<name>:<letters>:<x>:<y>:<z>:<colour>:false:0:Internal-<dimension>-waypoints`.
//! Colons in the name are escaped as Xaero's escapes them. The `[Add]` link on a shared line runs
//! [`add_command`], which [`parse_add_command`] reads back.

use crate::waypoint::{MAX_INITIALS, Waypoint};

/// The longest chat line servers take.
const MAX_CHAT: usize = 256;
/// The longest waypoint name.
pub const MAX_NAME: usize = 32;
const MAX_HORIZONTAL: i64 = 30_000_000;
const MAX_VERTICAL: i64 = 20_000;

/// The sixteen chat colours, by their legacy code: what a shared waypoint's colour is rounded to.
pub const CHAT_COLORS: [u32; 16] = [
    0x000000, 0x0000AA, 0x00AA00, 0x00AAAA, 0xAA0000, 0xAA00AA, 0xFFAA00, 0xAAAAAA, 0x555555,
    0x5555FF, 0x55FF55, 0x55FFFF, 0xFF5555, 0xFF55FF, 0xFFFF55, 0xFFFFFF,
];

/// A waypoint someone shared. `y` is none when they didn't know the height.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shared {
    pub name: String,
    pub initials: String,
    pub x: i32,
    pub y: Option<i32>,
    pub z: i32,
    /// Index into [`CHAT_COLORS`].
    pub color: usize,
    pub dimension: String,
}

impl Shared {
    /// The waypoint to add, at `y` when the share has no height.
    pub fn waypoint(&self, y: i32) -> Waypoint {
        let mut w = Waypoint::new(
            &self.name,
            [self.x, self.y.unwrap_or(y), self.z],
            &self.dimension,
            0xFF00_0000 | CHAT_COLORS[self.color],
        );
        w.initials.clone_from(&self.initials);
        w
    }
}

fn encode(text: &str) -> String {
    text.replace('-', "^min^")
        .replace('_', "-")
        .replace(':', "^col^")
        .replace('*', "^ast^")
}

fn decode(text: &str) -> String {
    text.replace("^ast^", "*")
        .replace("^col^", ":")
        .replace('-', "_")
        .replace("^min^", "-")
}

/// Text chat takes: no control characters and no formatting codes.
pub fn chat_safe(text: &str) -> String {
    text.chars()
        .filter(|c| *c >= ' ' && *c != '§' && *c != '\u{7f}')
        .collect()
}

/// The chat colour nearest a waypoint colour.
pub fn color_index(argb: u32) -> usize {
    let channel = |c: u32, s: u32| i64::from((c >> s) & 0xFF);
    (0..16)
        .min_by_key(|&i| {
            let c = CHAT_COLORS[i];
            [16, 8, 0]
                .iter()
                .map(|&s| (channel(argb, s) - channel(c, s)).pow(2))
                .sum::<i64>()
        })
        .unwrap_or(15)
}

fn xaero_dimension(id: &str) -> String {
    match id {
        "minecraft:overworld" => "overworld".to_owned(),
        "minecraft:the_nether" => "the-nether".to_owned(),
        "minecraft:the_end" => "the-end".to_owned(),
        _ => format!("dim%{}", id.replace(':', "$").replace('/', "%")),
    }
}

fn our_dimension(xaero: &str) -> Option<String> {
    let id = match xaero.to_ascii_lowercase().as_str() {
        "overworld" => "minecraft:overworld".to_owned(),
        "the-nether" => "minecraft:the_nether".to_owned(),
        "the-end" => "minecraft:the_end".to_owned(),
        other => other
            .strip_prefix("dim%")?
            .replace('$', ":")
            .replace('%', "/"),
    };
    let (ns, path) = id.split_once(':')?;
    let ok = |s: &str, extra: &str| {
        !s.is_empty()
            && s.chars().all(|c| {
                c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || "_.-".contains(c)
                    || extra.contains(c)
            })
    };
    (ok(ns, "") && ok(path, "/")).then_some(id)
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The chat line sharing `w`, or none when it can't fit in a chat line.
pub fn format(w: &Waypoint) -> Option<String> {
    let initials = encode(&chat_safe(&w.label()));
    let tail = format!(
        ":{initials}:{}:{}:{}:{}:false:0:Internal-{}-waypoints",
        w.x,
        w.y,
        w.z,
        color_index(w.color),
        xaero_dimension(&w.dimension)
    );
    let mut name = chat_safe(&w.name).trim().to_owned();
    if name.is_empty() {
        name = "Waypoint".to_owned();
    }
    let mut encoded = encode(&name);
    let head = "xaero-waypoint:".len();
    while head + encoded.len() + tail.len() > MAX_CHAT && name.chars().count() > 1 {
        name = cut(&name, name.chars().count() - 1);
        encoded = encode(&name);
    }
    let line = format!("xaero-waypoint:{encoded}{tail}");
    (line.len() <= MAX_CHAT).then_some(line)
}

/// The first well-formed shared waypoint in a chat line.
pub fn parse(text: &str) -> Option<Shared> {
    const PREFIX: &str = "xaero-waypoint:";
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let tail = &rest[at + PREFIX.len()..];
        if let Some(found) = parse_fields(tail) {
            return Some(found);
        }
        rest = tail;
    }
    None
}

fn parse_fields(text: &str) -> Option<Shared> {
    // name:letters:x:y:z:colour:bool:number:Internal-<dim>-waypoints, the last up to whitespace.
    let mut f = text.splitn(9, ':');
    let name = f.next()?;
    let letters = f.next()?;
    let (x, y, z) = (f.next()?, f.next()?, f.next()?);
    let color = f.next()?;
    let flag = f.next()?;
    let number = f.next()?;
    let last = f.next()?;
    let last = last.split_whitespace().next()?;
    let int = |s: &str, digits: usize| -> Option<i64> {
        let d = s.strip_prefix('-').unwrap_or(s);
        (!d.is_empty() && d.len() <= digits && d.chars().all(|c| c.is_ascii_digit()))
            .then(|| s.parse().ok())?
    };
    if name.is_empty() || name.len() > 160 || letters.len() > 20 {
        return None;
    }
    if flag != "true" && flag != "false" {
        return None;
    }
    int(number, 5)?;
    let (x, z) = (int(x, 9)?, int(z, 9)?);
    let y = if y == "~" { None } else { Some(int(y, 5)?) };
    let color = int(color, 2)?;
    let xaero = last.strip_prefix("Internal-")?.strip_suffix("-waypoints")?;
    let dimension = our_dimension(xaero)?;
    let name = cut(chat_safe(&decode(name)).trim(), MAX_NAME)
        .trim()
        .to_owned();
    let initials: String = decode(letters)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(MAX_INITIALS)
        .collect();
    if name.is_empty()
        || x.abs() > MAX_HORIZONTAL
        || z.abs() > MAX_HORIZONTAL
        || y.is_some_and(|y| y.abs() > MAX_VERTICAL)
        || !(0..16).contains(&color)
    {
        return None;
    }
    Some(Shared {
        name,
        initials,
        x: x as i32,
        y: y.map(|y| y as i32),
        z: z as i32,
        color: color as usize,
        dimension,
    })
}

/// The command the `[Add]` link on a shared line runs, without its slash, with the height to use.
pub fn add_command(s: &Shared, y: i32) -> String {
    format!(
        "diorama waypoint add {} {} {} {} \"{}\" \"{}\" {}",
        s.x, y, s.z, s.color, s.dimension, s.initials, s.name
    )
}

/// A `"quoted"` word at the start of `s`, and what follows it.
fn quoted(s: &str) -> Option<(String, &str)> {
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some((s[..end].to_owned(), s[end + 1..].trim_start()))
}

/// Reads back an [`add_command`].
pub fn parse_add_command(command: &str) -> Option<Shared> {
    let rest = command.trim().strip_prefix("diorama waypoint add ")?;
    let mut words = rest.splitn(5, ' ');
    let x: i32 = words.next()?.parse().ok()?;
    let y: i32 = words.next()?.parse().ok()?;
    let z: i32 = words.next()?.parse().ok()?;
    let color: usize = words.next()?.parse().ok().filter(|c| *c < 16)?;
    let rest = words.next()?;
    let (dimension, rest) = quoted(rest)?;
    let (initials, name) = quoted(rest)?;
    let name = cut(chat_safe(name).trim(), MAX_NAME);
    (!name.is_empty() && our_dimension(&xaero_dimension(&dimension)).as_deref() == Some(&dimension))
        .then_some(Shared {
            name,
            initials: initials
                .chars()
                .filter(|c| c.is_alphanumeric())
                .take(MAX_INITIALS)
                .collect(),
            x,
            y: Some(y),
            z,
            color,
            dimension,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_round_trip() {
        let mut w = Waypoint::new(
            "Iron: farm_2-b",
            [-120, 64, 3000],
            "minecraft:the_nether",
            0xFFFF5555,
        );
        w.initials = "IF".into();
        let line = format(&w).unwrap();
        assert_eq!(
            line,
            "xaero-waypoint:Iron^col^ farm-2^min^b:IF:-120:64:3000:12:false:0:Internal-the-nether-waypoints"
        );
        let s = parse(&format!("<Alex> look {line} here")).unwrap();
        assert_eq!(
            s,
            Shared {
                name: "Iron: farm_2-b".into(),
                initials: "IF".into(),
                x: -120,
                y: Some(64),
                z: 3000,
                color: 12,
                dimension: "minecraft:the_nether".into(),
            }
        );
        let back = s.waypoint(0);
        assert_eq!(
            (back.x, back.y, back.z, back.color),
            (-120, 64, 3000, 0xFFFF5555)
        );
    }

    #[test]
    fn modded_dimensions_and_unknown_heights() {
        let s = parse(
            "xaero-waypoint:Base:B:5:~:6:3:false:0:Internal-dim%twilight$forest%deep-waypoints",
        )
        .unwrap();
        assert_eq!(s.dimension, "twilight:forest/deep");
        assert_eq!(s.y, None);
        assert_eq!(s.waypoint(70).y, 70);
        assert!(
            parse("xaero-waypoint:Base:B:5:6:7:99:false:0:Internal-overworld-waypoints").is_none()
        );
        assert!(
            parse("xaero-waypoint:Base:B:5:6:7:1:false:0:Internal-dim%NOPE-waypoints").is_none()
        );
        assert!(parse("nothing here").is_none());
    }

    #[test]
    fn add_commands_round_trip() {
        let s = parse("xaero-waypoint:My base:MB:1:2:3:10:false:0:Internal-overworld-waypoints")
            .unwrap();
        let cmd = add_command(&s, 2);
        assert_eq!(
            cmd,
            "diorama waypoint add 1 2 3 10 \"minecraft:overworld\" \"MB\" My base"
        );
        assert_eq!(parse_add_command(&cmd), Some(s));
        assert!(parse_add_command("diorama waypoint add 1 2 3 10 \"bad dim\" \"\" x").is_none());
    }

    #[test]
    fn long_names_are_cut_to_fit() {
        let w = Waypoint::new(
            &"n".repeat(400),
            [0, 0, 0],
            "minecraft:overworld",
            0xFFFFFFFF,
        );
        let line = format(&w).unwrap();
        assert!(line.len() <= MAX_CHAT);
        assert_eq!(color_index(0xFFFFFFFF), 15);
    }
}
