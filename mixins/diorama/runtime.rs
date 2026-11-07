// Diorama for Horizon: the game glue, added to the game as `game::diorama`.
//
// The engine (`diorama_engine`, this bundle's own crate) bakes and draws the map; this module
// feeds it the client's chunks and block looks, keeps the map of the dimension the player is in,
// and draws it: the minimap in a corner of the HUD and the fullscreen map in a screen of its own.
// Horizon gives native mods one image in the HUD atlas (`native_mod_image`), so the view on show
// is composed on the CPU and uploaded when it changes.
//
// Ported from Diorama (MIT, SunkenInTime): https://github.com/SunkenInTime/diorama

use super::Game;
use diorama_engine::chunk::{Blocks, ChunkSnapshot, ColumnTints, Light, Section};
use diorama_engine::looks::{
    self, Average, BlockLooks, FaceColor, FaceTint, Kind, StateInput, TintKind,
};
use diorama_engine::projection::{Facing, Projection};
use diorama_engine::radar;
use diorama_engine::session::{self, MapSession, SessionConfig};
use diorama_engine::share;
use diorama_engine::source::ChunkSource;
use diorama_engine::style::MapStyle;
use diorama_engine::tile::TileKey;
use diorama_engine::view::{self, Fog, View};
use diorama_engine::waypoint::{self, Waypoint, Waypoints};
use diorama_engine::waypoint_ui::{self as wui, Button, EditorHit, Form, ListHit, Rect};
use horizon_client::extras::Corner;
use horizon_client::extras_hud::Corners;
use horizon_client::hud::HudBatch;
use horizon_client::mod_api::{NativeCamera, NativeMod, NativeScreenView};
use horizon_client::text::{ClickEvent, Style, Text};
use horizon_world::registry::BlockStateId;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

pub(super) const ID: &str = "diorama";
pub(super) const VERSION: &str = "0.1.0";
const IMAGE_SPRITE: &str = "horizon/modimage";
/// The largest image the HUD atlas slot holds, each way.
const IMAGE_MAX: u32 = 1024;
/// Minimap sides in GUI pixels.
const SIZES: [f32; 4] = [80.0, 112.0, 144.0, 176.0];
/// Minimap zoom steps, in GUI pixels per block (Diorama's).
const ZOOMS: [f64; 10] = [0.1875, 0.25, 0.375, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0];
const MAP_MIN_ZOOM: f64 = 0.04;
const MAP_MAX_ZOOM: f64 = 16.0;
/// Chunks sent to the converter per tick, nearest the player first.
const CONVERT_PER_TICK: usize = 64;
/// How often a view with fog in it is drawn again, for the drift.
const FOG_FRAME: Duration = Duration::from_millis(100);
/// How often the world is checked for chunks that came, changed or went.
const TICK: Duration = Duration::from_millis(50);
/// A waypoint pin's height in GUI pixels.
const PIN_H: f32 = 10.0;
/// How long a note on the fullscreen map stays.
const TOAST: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------------------------
// Settings

/// Minimap opacity steps in percent. Diorama's slider goes from 20 to 100 in fives
/// (Settings.java:51-54); a button cycles these.
const OPACITIES: [u8; 5] = [100, 80, 60, 40, 20];
/// The player arrow's colours as 0xRRGGBB, with their names (Settings.java:258-259).
const ARROW_COLOURS: [(u32, &str); 8] = [
    (0x3FA9F5, "blue"),
    (0xEF4B4B, "red"),
    (0x62D96B, "green"),
    (0xFFC53D, "yellow"),
    (0xF76B15, "orange"),
    (0x8E4EC6, "purple"),
    (0xD6409F, "pink"),
    (0xF2F2F2, "white"),
];
/// A key left unbound, as the game names it.
const UNBOUND: &str = "key.keyboard.unknown";

/// The minimap's border (Settings.java:208-212). Diorama's Minecraft frame needs its textures,
/// which a native mod has no atlas room for, so it is left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Frame {
    Classic,
    None,
}

impl Frame {
    const ALL: [Frame; 2] = [Frame::Classic, Frame::None];

    fn id(self) -> &'static str {
        match self {
            Frame::Classic => "classic",
            Frame::None => "none",
        }
    }
}

/// How dark the map is drawn: as the world's time has it, or always by day or at night
/// (Settings.java:201-205).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DayNight {
    Follow,
    Day,
    Night,
}

impl DayNight {
    const ALL: [DayNight; 3] = [DayNight::Follow, DayNight::Day, DayNight::Night];

    fn id(self) -> &'static str {
        match self {
            DayNight::Follow => "follow",
            DayNight::Day => "day",
            DayNight::Night => "night",
        }
    }
}

/// How the time of day under the minimap reads, as Xaero's Minimap offers it
/// (Settings.java:192-198, without OFF, which is the clock setting).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClockFormat {
    Day24h,
    Day12h,
    Only24h,
    Only12h,
}

impl ClockFormat {
    const ALL: [ClockFormat; 4] = [
        ClockFormat::Day24h,
        ClockFormat::Day12h,
        ClockFormat::Only24h,
        ClockFormat::Only12h,
    ];

    fn id(self) -> &'static str {
        match self {
            ClockFormat::Day24h => "day_24h",
            ClockFormat::Day12h => "day_12h",
            ClockFormat::Only24h => "only_24h",
            ClockFormat::Only12h => "only_12h",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ClockFormat::Day24h => "Day, 24h",
            ClockFormat::Day12h => "Day, 12h",
            ClockFormat::Only24h => "24h",
            ClockFormat::Only12h => "12h",
        }
    }
}

/// The item after `now` in `all`, round to the first.
fn cycle<T: Copy + PartialEq>(all: &[T], now: T) -> T {
    let i = all.iter().position(|x| *x == now).map_or(0, |i| i + 1);
    all[i % all.len()]
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Settings {
    pub minimap: bool,
    pub corner: usize,
    pub size: usize,
    pub round: bool,
    pub zoom: usize,
    pub fast: bool,
    pub facing: Facing,
    pub coordinates: bool,
    /// Waypoints on the minimap.
    pub pins: bool,
    /// A pin where the player dies.
    pub deaths: bool,
    /// Waypoints floating in the world.
    pub world_pins: bool,
    /// Players and mobs on the maps.
    pub radar: bool,
    /// Dropped items on the maps too.
    pub radar_items: bool,
    /// The biome under the minimap.
    pub biome: bool,
    /// The time of day under the minimap.
    pub clock: bool,
    /// The chunk grid on the fullscreen map.
    pub grid: bool,
    /// How opaque the minimap's map is, in percent: one of [`OPACITIES`].
    pub opacity: u8,
    pub frame: Frame,
    /// The compass letters round the minimap.
    pub compass: bool,
    pub day_night: DayNight,
    /// Index into [`ARROW_COLOURS`].
    pub arrow_colour: usize,
    pub clock_format: ClockFormat,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            minimap: true,
            corner: 1,
            size: 1,
            round: false,
            zoom: 5,
            fast: false,
            facing: Facing::North,
            coordinates: true,
            pins: true,
            deaths: true,
            world_pins: true,
            radar: true,
            radar_items: true,
            biome: true,
            clock: false,
            grid: false,
            opacity: 100,
            frame: Frame::Classic,
            compass: true,
            day_night: DayNight::Follow,
            arrow_colour: 0,
            clock_format: ClockFormat::Day24h,
        }
    }
}

impl Settings {
    fn read(get: impl Fn(&str) -> Option<String>) -> Settings {
        let d = Settings::default();
        let get = |k: &str| get(k).map(|v| v.trim().to_owned());
        let flag = |k: &str, default: bool| get(k).map_or(default, |v| v == "true");
        let index = |k: &str, n: usize, default: usize| {
            get(k)
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|i| *i < n)
                .unwrap_or(default)
        };
        Settings {
            minimap: flag("minimap", d.minimap),
            corner: get("corner")
                .and_then(|c| Corner::ALL.iter().position(|k| k.name() == c))
                .unwrap_or(d.corner),
            size: index("size", SIZES.len(), d.size),
            round: get("shape").as_deref() == Some("round"),
            zoom: get("zoom")
                .and_then(|z| z.parse::<f64>().ok())
                .and_then(|z| ZOOMS.iter().position(|s| (s - z).abs() < 1e-9))
                .unwrap_or(d.zoom),
            fast: get("quality").as_deref() == Some("fast"),
            facing: get("facing").map_or(d.facing, |f| Facing::of(&f)),
            coordinates: flag("coordinates", d.coordinates),
            pins: flag("pins", d.pins),
            deaths: flag("deaths", d.deaths),
            world_pins: flag("world_pins", d.world_pins),
            radar: flag("radar", d.radar),
            radar_items: flag("radar_items", d.radar_items),
            biome: flag("biome", d.biome),
            clock: flag("clock", d.clock),
            grid: flag("grid", d.grid),
            // The nearest step to a saved percentage, as Settings.snapOpacity does.
            opacity: get("opacity")
                .and_then(|o| o.parse::<f64>().ok())
                .and_then(|o| {
                    OPACITIES.iter().copied().min_by(|a, b| {
                        (f64::from(*a) - o)
                            .abs()
                            .total_cmp(&(f64::from(*b) - o).abs())
                    })
                })
                .unwrap_or(d.opacity),
            frame: get("frame")
                .and_then(|f| Frame::ALL.into_iter().find(|k| k.id() == f))
                .unwrap_or(d.frame),
            compass: flag("compass", d.compass),
            day_night: get("day_night")
                .and_then(|f| DayNight::ALL.into_iter().find(|k| k.id() == f))
                .unwrap_or(d.day_night),
            arrow_colour: get("arrow_colour")
                .and_then(|c| ARROW_COLOURS.iter().position(|(_, n)| *n == c))
                .unwrap_or(d.arrow_colour),
            clock_format: get("clock_format")
                .and_then(|f| ClockFormat::ALL.into_iter().find(|k| k.id() == f))
                .unwrap_or(d.clock_format),
        }
    }

    fn stored(&self) -> Vec<(&'static str, String)> {
        vec![
            ("minimap", self.minimap.to_string()),
            ("corner", Corner::ALL[self.corner].name().to_owned()),
            ("size", self.size.to_string()),
            (
                "shape",
                if self.round { "round" } else { "square" }.to_owned(),
            ),
            ("zoom", ZOOMS[self.zoom].to_string()),
            (
                "quality",
                if self.fast { "fast" } else { "fine" }.to_owned(),
            ),
            ("facing", self.facing.id().to_owned()),
            ("coordinates", self.coordinates.to_string()),
            ("pins", self.pins.to_string()),
            ("deaths", self.deaths.to_string()),
            ("world_pins", self.world_pins.to_string()),
            ("radar", self.radar.to_string()),
            ("radar_items", self.radar_items.to_string()),
            ("biome", self.biome.to_string()),
            ("clock", self.clock.to_string()),
            ("grid", self.grid.to_string()),
            ("opacity", self.opacity.to_string()),
            ("frame", self.frame.id().to_owned()),
            ("compass", self.compass.to_string()),
            ("day_night", self.day_night.id().to_owned()),
            (
                "arrow_colour",
                ARROW_COLOURS[self.arrow_colour].1.to_owned(),
            ),
            ("clock_format", self.clock_format.id().to_owned()),
        ]
    }

    fn labels(&self) -> Vec<String> {
        let on = |b: bool| if b { "ON" } else { "OFF" };
        let facing = match self.facing {
            Facing::North => "North",
            Facing::East => "East",
            Facing::South => "South",
            Facing::West => "West",
        };
        vec![
            format!("Minimap: {}", on(self.minimap)),
            format!("Corner: {}", Corner::ALL[self.corner].label()),
            format!("Size: {}", SIZES[self.size]),
            format!("Shape: {}", if self.round { "Round" } else { "Square" }),
            format!("Minimap zoom: {} px per block", ZOOMS[self.zoom]),
            format!("Quality: {}", if self.fast { "Fast" } else { "Fine" }),
            format!("Map looks: {facing}"),
            format!("Coordinates: {}", on(self.coordinates)),
            format!("Waypoints on minimap: {}", on(self.pins)),
            format!("Death pins: {}", on(self.deaths)),
            format!("Waypoints in the world: {}", on(self.world_pins)),
            format!("Radar: {}", on(self.radar)),
            format!("Radar items: {}", on(self.radar_items)),
            format!("Biome: {}", on(self.biome)),
            format!("Time of day: {}", on(self.clock)),
            format!("Chunk grid on the map: {}", on(self.grid)),
            format!("Minimap opacity: {}%", self.opacity),
            format!(
                "Frame: {}",
                match self.frame {
                    Frame::Classic => "Classic",
                    Frame::None => "None",
                }
            ),
            format!("Compass: {}", on(self.compass)),
            format!(
                "Day and night: {}",
                match self.day_night {
                    DayNight::Follow => "World time",
                    DayNight::Day => "Always day",
                    DayNight::Night => "Always night",
                }
            ),
            format!("Arrow colour: {}", {
                let name = ARROW_COLOURS[self.arrow_colour].1;
                name[..1].to_uppercase() + &name[1..]
            }),
            format!("Time format: {}", self.clock_format.label()),
        ]
    }

    /// How opaque the minimap draws, 0 to 1.
    fn opacity(&self) -> f32 {
        f32::from(self.opacity) / 100.0
    }

    fn click(&mut self, i: usize) {
        match i {
            0 => self.minimap = !self.minimap,
            1 => self.corner = (self.corner + 1) % Corner::ALL.len(),
            2 => self.size = (self.size + 1) % SIZES.len(),
            3 => self.round = !self.round,
            4 => self.zoom = (self.zoom + 1) % ZOOMS.len(),
            5 => self.fast = !self.fast,
            6 => self.facing = self.facing.next(1),
            7 => self.coordinates = !self.coordinates,
            8 => self.pins = !self.pins,
            9 => self.deaths = !self.deaths,
            10 => self.world_pins = !self.world_pins,
            11 => self.radar = !self.radar,
            12 => self.radar_items = !self.radar_items,
            13 => self.biome = !self.biome,
            14 => self.clock = !self.clock,
            15 => self.grid = !self.grid,
            16 => self.opacity = cycle(&OPACITIES, self.opacity),
            17 => self.frame = cycle(&Frame::ALL, self.frame),
            18 => self.compass = !self.compass,
            19 => self.day_night = cycle(&DayNight::ALL, self.day_night),
            20 => self.arrow_colour = (self.arrow_colour + 1) % ARROW_COLOURS.len(),
            21 => self.clock_format = cycle(&ClockFormat::ALL, self.clock_format),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// State

/// What the fullscreen map shows.
#[derive(Clone, Copy, Debug)]
struct MapScreen {
    center: (f64, f64),
    /// GUI pixels per block.
    zoom: f64,
    /// Centred on the player, moving with them.
    follow: bool,
    /// Showing a cave slice instead of the surface.
    caves: bool,
    /// The slice, when the player picked one; else the one around them.
    layer: Option<i32>,
    dragging: bool,
}

impl Default for MapScreen {
    fn default() -> Self {
        MapScreen {
            center: (0.0, 0.0),
            zoom: 2.0,
            follow: true,
            caves: false,
            layer: None,
            dragging: false,
        }
    }
}

/// The image last uploaded, and what it shows.
#[derive(Clone, Copy, Debug)]
struct Shown {
    fullscreen: bool,
    view: View,
    cache_version: u64,
    at: Instant,
    /// The GUI size it was made for.
    gui: [f32; 2],
}

/// The world box of blocks that changed: its lowest and highest corners.
type ChangedBox = ([i32; 3], [i32; 3]);

struct ConvertJob {
    chunk: Arc<horizon_client::chunk::Chunk>,
    /// The world box of what changed; none for a chunk that just arrived.
    changed: Option<([i32; 3], [i32; 3])>,
}

struct Converted {
    snapshot: Arc<ChunkSnapshot>,
    changed: Option<([i32; 3], [i32; 3])>,
}

/// The chunk converter thread, for one dimension's tables.
struct Converter {
    jobs: Sender<ConvertJob>,
    done: Receiver<Converted>,
}

impl Converter {
    fn start(
        biomes: Arc<horizon_client::biome::Biomes>,
        air: Arc<HashSet<u32>>,
        default_fog: u32,
    ) -> Converter {
        let (jobs, rx) = mpsc::channel::<ConvertJob>();
        let (tx, done) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("diorama-convert".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let snapshot = Arc::new(convert(&job.chunk, &biomes, &air, default_fog));
                    if tx
                        .send(Converted {
                            snapshot,
                            changed: job.changed,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            });
        if let Err(e) = spawned {
            log::warn!("Diorama: no chunk converter thread: {e}");
        }
        Converter { jobs, done }
    }
}

/// The looks being built on a thread of their own, for a resource generation.
struct LooksJob {
    generation: u64,
    done: Receiver<BlockLooks>,
}

struct State {
    settings: Settings,
    /// The world and dimension the session is for.
    session_key: Option<String>,
    session: Option<MapSession>,
    converter: Option<Converter>,
    /// Horizon's chunks as last seen, to tell which changed.
    tracked: HashMap<(i32, i32), Arc<horizon_client::chunk::Chunk>>,
    looks: Option<Arc<BlockLooks>>,
    looks_generation: u64,
    looks_job: Option<LooksJob>,
    air: Arc<HashSet<u32>>,
    underground: bool,
    underground_checked: Option<Instant>,
    map: MapScreen,
    shown: Option<Shown>,
    pixels: Vec<u32>,
    rgba: Vec<u8>,
    started: Instant,
    /// The map screen should open (asked from a key press).
    open_map: bool,
    last_tick: Option<Instant>,
    /// This world's waypoints, and the world they're for.
    waypoints: Option<Waypoints>,
    waypoints_world: Option<String>,
    /// The dimension the player is in.
    dimension: String,
    /// The death screen was up at the last tick.
    dead: bool,
    /// A note on the fullscreen map, and when it was made.
    toast: Option<(String, Instant)>,
    /// The list or editor over the fullscreen map.
    panel: Panel,
    /// Where the left button went down on the map, and whether it has moved far since.
    press: Option<([f32; 2], bool)>,
    /// What the next frame sends or does for the screen hooks.
    outbox: Vec<Outgoing>,
    /// Where this world's maps are kept.
    map_root: Option<PathBuf>,
    /// Another dimension's saved map, shown on the fullscreen map instead of this one's.
    viewing: Option<Viewer>,
}

/// Another dimension's map, read from what was saved of it: nothing bakes.
struct Viewer {
    dimension: String,
    session: MapSession,
}

impl State {
    fn new(settings: Settings) -> State {
        State {
            settings,
            session_key: None,
            session: None,
            converter: None,
            tracked: HashMap::new(),
            looks: None,
            looks_generation: 0,
            looks_job: None,
            air: Arc::new(HashSet::new()),
            underground: false,
            underground_checked: None,
            map: MapScreen::default(),
            shown: None,
            pixels: Vec::new(),
            rgba: Vec::new(),
            started: Instant::now(),
            open_map: false,
            last_tick: None,
            waypoints: None,
            waypoints_world: None,
            dimension: String::new(),
            dead: false,
            toast: None,
            panel: Panel::None,
            press: None,
            outbox: Vec::new(),
            map_root: None,
            viewing: None,
        }
    }

    /// The map the fullscreen map shows: another dimension's, or the player's.
    fn map_session(&self) -> Option<&MapSession> {
        match &self.viewing {
            Some(v) => Some(&v.session),
            None => self.session.as_ref(),
        }
    }

    /// The dimension the fullscreen map shows.
    fn map_dimension(&self) -> &str {
        self.viewing
            .as_ref()
            .map_or(&self.dimension, |v| &v.dimension)
    }

    /// The waypoints shown in the dimension the fullscreen map shows.
    fn map_pins(&self) -> Vec<(usize, Waypoint)> {
        let dimension = self.map_dimension();
        self.waypoints.as_ref().map_or_else(Vec::new, |ws| {
            ws.shown_in(dimension)
                .map(|(i, w)| (i, w.clone()))
                .collect()
        })
    }

    /// The waypoints shown in the player's dimension, with their indices.
    fn pins(&self) -> Vec<(usize, Waypoint)> {
        self.waypoints.as_ref().map_or_else(Vec::new, |ws| {
            ws.shown_in(&self.dimension)
                .map(|(i, w)| (i, w.clone()))
                .collect()
        })
    }

    /// Adds a waypoint; the line to tell the player.
    fn add_waypoint(&mut self, w: Waypoint) -> String {
        let Some(ws) = self.waypoints.as_mut() else {
            return "Diorama: no world to add a waypoint to".to_owned();
        };
        let line = format!("Added {} at {} {} {}", w.name, w.x, w.y, w.z);
        match ws.add(w) {
            Ok(()) => line,
            Err(e) => {
                log::warn!("Diorama: saving waypoints: {e}");
                format!("{line}, but it couldn't be saved: {e}")
            }
        }
    }

    /// Ends the session, saving what it baked.
    fn end_session(&mut self) {
        // Its bake threads finish their tiles and its store writes what's left: not on the
        // game's thread.
        if let Some(session) = self.session.take() {
            let spawned = std::thread::Builder::new()
                .name("diorama-close".into())
                .spawn(move || drop(session));
            if let Err(e) = spawned {
                log::warn!("Diorama: closing the map on the game's thread: {e}");
            }
        }
        self.session_key = None;
        self.viewing = None;
        self.converter = None;
        self.tracked.clear();
        self.shown = None;
        self.underground = false;
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Runs `f` on the state, made from the settings files the first time.
fn with_state<R>(game: &Game<'_>, f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let state =
            s.get_or_insert_with(|| State::new(Settings::read(|k| game.native_setting(ID, k))));
        f(state)
    })
}

/// The same from a hook that has no game at hand; nothing before the state exists.
fn with_state_if<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

// ---------------------------------------------------------------------------------------------
// Options > Mods

pub(super) fn list(mods: &mut Vec<NativeMod>) {
    let key = |name: &str, label: &str, default: &str| {
        [name.to_owned(), label.to_owned(), default.to_owned()]
    };
    mods.push(NativeMod {
        id: ID.to_owned(),
        title: format!("Diorama {VERSION}"),
        description: "A minimap and world map that draw your world at an angle, like a little model of it on a table.".to_owned(),
        keys: vec![
            key("map", "Open the map", "key.keyboard.m"),
            key("zoom_in", "Zoom the minimap in", "key.keyboard.equal"),
            key("zoom_out", "Zoom the minimap out", "key.keyboard.minus"),
            key("waypoint", "Add a waypoint here", "key.keyboard.b"),
            key("waypoints", "List your waypoints", "key.keyboard.u"),
            // Unbound until the player picks keys, as in Diorama (DioramaClient.java:42-46).
            key("toggle_minimap", "Show or hide the minimap", UNBOUND),
            key("enlarge_minimap", "Enlarge the minimap", UNBOUND),
            key("map_dimension", "Toggle the map's dimension", UNBOUND),
        ],
    });
}

pub(super) fn settings(game: &mut Game<'_>, mod_id: &str, buttons: &mut Vec<String>) {
    if mod_id != ID {
        return;
    }
    buttons.extend(with_state(game, |s| s.settings.labels()));
}

pub(super) fn settings_click(game: &mut Game<'_>, mod_id: &str, button: usize) {
    if mod_id != ID {
        return;
    }
    let (stored, restart) = with_state(game, |s| {
        let before = s.settings;
        s.settings.click(button);
        if let Some(session) = s.session.as_mut() {
            session.set_fast(s.settings.fast);
        }
        // Another facing is another map: its own tiles, baked again.
        let restart = before.facing != s.settings.facing;
        if restart {
            s.end_session();
        }
        s.shown = None;
        (s.settings.stored(), restart)
    });
    for (k, v) in stored {
        game.native_set_setting(ID, k, &v);
    }
    if restart {
        log::info!("Diorama: map turned; baking it again from this side");
    }
}

// ---------------------------------------------------------------------------------------------
// Keys

pub(super) fn key(game: &mut Game<'_>, key: &str, pressed: bool) {
    if !pressed || !game.native_on(ID) || !game.native_playing() {
        return;
    }
    let map = game.native_mod_key(ID, "map", "key.keyboard.m");
    let zoom_in = game.native_mod_key(ID, "zoom_in", "key.keyboard.equal");
    let zoom_out = game.native_mod_key(ID, "zoom_out", "key.keyboard.minus");
    let add = game.native_mod_key(ID, "waypoint", "key.keyboard.b");
    let list = game.native_mod_key(ID, "waypoints", "key.keyboard.u");
    let toggle = game.native_mod_key(ID, "toggle_minimap", UNBOUND);
    let player = game.native_player();
    if key == UNBOUND {
        return;
    }
    if key == add {
        let Some(p) = player else { return };
        let at = [p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32];
        let line = with_state(game, |s| {
            let w = s.waypoints.as_ref()?.next_at(at, &s.dimension);
            Some(s.add_waypoint(w))
        });
        if let Some(line) = line {
            game.native_chat(&format!("Diorama: {line}"));
        }
        return;
    }
    let stored = with_state(game, |s| {
        if key == map || key == list {
            if let (Some(p), Some(session)) = (player, s.session.as_ref()) {
                s.map.center = session.uv([p.x, p.y, p.z]);
            }
            s.map.follow = true;
            s.map.dragging = false;
            s.press = None;
            s.viewing = None;
            s.shown = None;
            s.panel = if key == list {
                Panel::List { scroll: 0 }
            } else {
                Panel::None
            };
            s.open_map = true;
            None
        } else if key == zoom_in || key == zoom_out {
            let step = s.settings.zoom as i32 + if key == zoom_in { 1 } else { -1 };
            s.settings.zoom = step.clamp(0, ZOOMS.len() as i32 - 1) as usize;
            Some(s.settings.stored())
        } else if key == toggle {
            s.settings.minimap = !s.settings.minimap;
            s.shown = None;
            Some(s.settings.stored())
        } else {
            None
        }
    });
    if let Some(stored) = stored {
        for (k, v) in stored {
            game.native_set_setting(ID, k, &v);
        }
    }
    open_map_if_asked(game);
}

/// Opens the map screen if a key or command asked for it.
fn open_map_if_asked(game: &mut Game<'_>) {
    let open = with_state(game, |s| std::mem::take(&mut s.open_map));
    if open && !game.native_mod_screen_open(ID) {
        log::debug!("Diorama: the map can't open over this screen");
    }
}

/// Sends what the screen hooks asked for: chat lines, teleports, closing the map.
fn send_outbox(game: &mut Game<'_>) {
    let out = with_state(game, |s| std::mem::take(&mut s.outbox));
    for o in out {
        let sent = match o {
            Outgoing::Chat(line) => game.native_send_chat(&line),
            Outgoing::Teleport {
                at: [x, y, z],
                dimension,
            } => game
                .native_send_chat(&format!("/execute in {dimension} run tp @s {x} {y} {z}"))
                .or_else(|e| {
                    // In single player there is no server connection; the game's own server
                    // runs it, for its player.
                    let line =
                        format!("execute as @a[limit=1] in {dimension} run tp @s {x} {y} {z}");
                    if game.native_run_command(&line) {
                        Ok(())
                    } else {
                        Err(e)
                    }
                }),
            Outgoing::Close => {
                game.native_mod_screen_close(ID);
                Ok(())
            }
        };
        if let Err(e) = sent {
            game.native_chat(&format!("Diorama: couldn't send that: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The world: sessions, chunks and looks

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Which world this is, for the folder its maps live in.
fn world_key(game: &Game<'_>) -> String {
    if let Some(remote) = &game.remote {
        return format!("mp_{}", sanitize(&remote.session.address));
    }
    match game.world_root.file_name() {
        Some(name) => format!("sp_{}", sanitize(&name.to_string_lossy())),
        None => "unknown".to_owned(),
    }
}

/// The biome fog colour a dimension's chunks fall back on.
fn default_fog(dimension: &str) -> u32 {
    match dimension {
        "minecraft:the_nether" => 0x330808,
        "minecraft:the_end" => 0x0A080C,
        _ => 0xC0D8FF,
    }
}

/// Where a dimension's map is kept under a world's map folder, for a facing and the style.
fn map_folder(root: &Path, dimension: &str, facing: Facing) -> PathBuf {
    root.join(sanitize(dimension)).join(format!(
        "{}-{:08x}",
        facing.id(),
        MapStyle::DEFAULT.fingerprint()
    ))
}

/// The file in a dimension's folder that names it, as its folder name can't.
const DIMENSION_FILE: &str = "dimension.txt";

/// The dimensions of a world that have a map from `facing`: the Overworld, the Nether and the
/// End first, then the others by name.
fn mapped_dimensions(root: &Path, facing: Facing) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(root)
        .map(|dir| {
            dir.flatten()
                .filter_map(|e| std::fs::read_to_string(e.path().join(DIMENSION_FILE)).ok())
                .map(|id| id.trim().to_owned())
                .filter(|id| map_folder(root, id, facing).is_dir())
                .collect()
        })
        .unwrap_or_default();
    let rank = |d: &str| match d {
        "minecraft:overworld" => 0,
        "minecraft:the_nether" => 1,
        "minecraft:the_end" => 2,
        _ => 3,
    };
    out.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.cmp(b)));
    out.dedup();
    out
}

/// Blocks of the Overworld per block of a dimension: 8 for the Nether.
fn coordinate_scale(dimension: &str) -> f64 {
    if dimension == "minecraft:the_nether" {
        8.0
    } else {
        1.0
    }
}

fn tick(game: &mut Game<'_>) {
    if !game.native_on(ID) || !game.native_window() {
        return;
    }
    let Some(player) = game.native_player() else {
        with_state(game, State::end_session);
        return;
    };
    let dimension = game.dim.id().to_owned();
    let world = world_key(game);
    let key = format!("{world}/{}", sanitize(&dimension));
    let generation = game.native_block_sprites_version();
    let data = game.native_data_dir(ID);
    let dead = game.screen == super::Screen::Dead;
    let root = data.join("maps").join(&world);
    let death = with_state(game, |s| {
        s.map_root = Some(root.clone());
        if s.waypoints_world.as_deref() != Some(world.as_str()) {
            s.waypoints = Some(Waypoints::load(
                data.join("maps").join(&world).join("waypoints.txt"),
            ));
            s.waypoints_world = Some(world.clone());
            s.dead = dead;
        }
        s.dimension.clone_from(&dimension);
        // A pin where the player fell, as the death screen comes up.
        let died = dead && !s.dead;
        s.dead = dead;
        if !died || !s.settings.deaths {
            return None;
        }
        let at = [
            player.x.floor() as i32,
            player.y.floor() as i32,
            player.z.floor() as i32,
        ];
        let now = waypoint::now_millis();
        let offset = chrono::Local::now().offset().local_minus_utc() / 60;
        let mut pin = Waypoint::new(
            &waypoint::death_name(now, offset),
            at,
            &dimension,
            waypoint::DEATH_COLOR,
        );
        pin.kind = waypoint::Kind::Death;
        pin.initials = "X".to_owned();
        pin.created = now;
        Some(s.add_waypoint(pin))
    });
    if let Some(line) = death {
        game.native_chat(&format!("Diorama: {line}"));
    }
    let needs_looks = with_state(game, |s| {
        if s.session_key.as_deref() != Some(key.as_str()) {
            s.end_session();
            s.session_key = Some(key.clone());
        }
        s.looks_generation != generation
            && s.looks_job
                .as_ref()
                .is_none_or(|j| j.generation != generation)
    });
    if needs_looks {
        let job = start_looks(game, generation);
        with_state(game, |s| {
            s.air = job.1.clone();
            s.looks_job = Some(job.0);
        });
    }
    let shape = game.world.shape;
    let biomes = game.pack.context.biomes.clone();
    // Start a session for this dimension once there are looks to bake with.
    with_state(game, |s| {
        if let Some(job) = &s.looks_job
            && let Ok(looks) = job.done.try_recv()
        {
            let looks = Arc::new(looks);
            s.looks_generation = job.generation;
            s.looks_job = None;
            s.looks = Some(looks.clone());
            if let Some(session) = s.session.as_mut() {
                session.set_looks(looks);
            }
        }
        if s.session.is_none()
            && let Some(looks) = s.looks.clone()
        {
            let style = MapStyle::DEFAULT;
            let folder = map_folder(&root, &dimension, s.settings.facing);
            // Named, so the map of another dimension can show this one.
            let named = root.join(sanitize(&dimension));
            if let Err(e) = std::fs::create_dir_all(&named)
                .and_then(|()| std::fs::write(named.join(DIMENSION_FILE), &dimension))
            {
                log::warn!("Diorama: naming the map folder: {e}");
            }
            let roof = shape
                .has_ceiling
                .then_some(shape.min_y + shape.logical_height);
            let mut session = MapSession::new(SessionConfig {
                folder,
                min_y: shape.min_y,
                max_y: shape.min_y + shape.height,
                roof,
                style,
                facing: s.settings.facing,
                fast: s.settings.fast,
                threads: SessionConfig::default_threads(),
            });
            session.set_looks(looks);
            s.session = Some(session);
            s.converter = Some(Converter::start(
                biomes.clone(),
                s.air.clone(),
                default_fog(&dimension),
            ));
            s.tracked.clear();
        }
    });
    track_chunks(game, [player.x, player.y, player.z]);
    let now = Instant::now();
    with_state(game, |s| {
        let Some(session) = s.session.as_mut() else {
            return;
        };
        if let Some(conv) = &s.converter {
            while let Ok(done) = conv.done.try_recv() {
                match done.changed {
                    None => session.chunk_loaded(done.snapshot, now),
                    Some((lo, hi)) => session.blocks_changed(done.snapshot, lo, hi, now),
                }
            }
        }
        // Underground, the minimap shows the cave around the player.
        if s.underground_checked
            .is_none_or(|t| now.duration_since(t) > Duration::from_millis(500))
        {
            s.underground_checked = Some(now);
            s.underground = underground(
                session,
                &s.air,
                [player.x, player.y, player.z],
                s.underground,
                shape.has_ceiling,
            );
        }
        let shows_caves = s.underground || s.map.caves;
        let layer = s
            .map
            .layer
            .filter(|_| s.map.caves)
            .unwrap_or_else(|| session::cave_layer_for(player.y));
        session.set_cave_layer(shows_caves.then_some(layer));
    });
}

/// Whether the player is under rock: no sky over their head and little sky light, with separate
/// thresholds so walking along a cave mouth doesn't flip the map back and forth.
fn underground(
    session: &MapSession,
    air: &HashSet<u32>,
    p: [f64; 3],
    was: bool,
    ceiling: bool,
) -> bool {
    if ceiling {
        return true;
    }
    let (x, y, z) = (
        p[0].floor() as i32,
        (p[1] + 1.62).floor() as i32,
        p[2].floor() as i32,
    );
    let Some(chunk) = session.chunks().chunk(x >> 4, z >> 4) else {
        return was;
    };
    let sky = chunk.section(y).map_or(15, |s| {
        s.light.sky_level(diorama_engine::chunk::index(x, y, z))
    });
    let covered = (y + 1..chunk.top_y.max(y + 1)).any(|yy| {
        let id = chunk.state(x & 15, yy, z & 15, 0);
        !air.contains(&id)
    });
    if was {
        covered && sky < 10
    } else {
        covered && sky < 5
    }
}

/// Sends chunks that arrived or changed to the converter, nearest first, and tells the session
/// of those that went.
fn track_chunks(game: &mut Game<'_>, player: [f64; 3]) {
    let (pcx, pcz) = (
        (player[0].floor() as i32) >> 4,
        (player[2].floor() as i32) >> 4,
    );
    let min_y = game.world.shape.min_y;
    let mut arrived: Vec<(
        (i32, i32),
        Arc<horizon_client::chunk::Chunk>,
        Option<ChangedBox>,
    )> = Vec::new();
    let present: HashSet<(i32, i32)>;
    {
        let chunks = &game.world.chunks;
        present = chunks.keys().copied().collect();
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let Some(s) = s.as_mut() else { return };
            if s.session.is_none() {
                return;
            }
            for (key, chunk) in chunks.iter() {
                match s.tracked.get(key) {
                    None => arrived.push((*key, chunk.clone(), None)),
                    Some(old) if !Arc::ptr_eq(old, chunk) => {
                        // Which sections changed: edits copy the sections they touch.
                        let mut lo = i32::MAX;
                        let mut hi = i32::MIN;
                        let n = chunk.sections.len().min(old.sections.len());
                        for i in 0..n {
                            let (a, b) = (&old.sections[i], &chunk.sections[i]);
                            if !Arc::ptr_eq(a, b)
                                && (a.blocks != b.blocks || s.underground || s.map.caves)
                            {
                                let base = min_y + i as i32 * 16;
                                lo = lo.min(base);
                                hi = hi.max(base + 15);
                            }
                        }
                        if lo <= hi {
                            let (bx, bz) = (key.0 * 16, key.1 * 16);
                            arrived.push((
                                *key,
                                chunk.clone(),
                                Some(([bx, lo, bz], [bx + 15, hi, bz + 15])),
                            ));
                        } else {
                            // Only light changed: keep the new chunk without baking again.
                            s.tracked.insert(*key, chunk.clone());
                        }
                    }
                    Some(_) => {}
                }
            }
        });
    }
    arrived.sort_by_key(|((cx, cz), _, _)| (cx - pcx).abs().max((cz - pcz).abs()));
    with_state(game, |s| {
        let Some(conv) = &s.converter else { return };
        for (key, chunk, changed) in arrived.into_iter().take(CONVERT_PER_TICK) {
            s.tracked.insert(key, chunk.clone());
            let _ = conv.jobs.send(ConvertJob { chunk, changed });
        }
        let gone: Vec<(i32, i32)> = s
            .tracked
            .keys()
            .filter(|k| !present.contains(k))
            .copied()
            .collect();
        for k in gone {
            s.tracked.remove(&k);
            if let Some(session) = s.session.as_mut() {
                session.chunk_unloaded(k.0, k.1);
            }
        }
    });
}

/// One of Horizon's chunks as the engine reads it: blocks packed again, light shared, and the
/// biome tints of each column at its surface.
fn convert(
    chunk: &horizon_client::chunk::Chunk,
    biomes: &horizon_client::biome::Biomes,
    air: &HashSet<u32>,
    default_fog: u32,
) -> ChunkSnapshot {
    let min_y = chunk.min_y();
    let mut ids = [0u32; 4096];
    let mut tops = [i32::MIN; 256];
    let mut sections = Vec::with_capacity(chunk.sections.len());
    for (i, s) in chunk.sections.iter().enumerate().rev() {
        s.unpack(&mut ids);
        let base = min_y + i as i32 * 16;
        for (col, top) in tops.iter_mut().enumerate() {
            if *top != i32::MIN {
                continue;
            }
            for ly in (0..16usize).rev() {
                if !air.contains(&ids[(ly << 8) | col]) {
                    *top = base + ly as i32;
                    break;
                }
            }
        }
        let light = match &s.light {
            horizon_client::chunk::Light::Uniform(v) => Light::Uniform(*v),
            horizon_client::chunk::Light::Sky(layer, block) => {
                Light::Sky(Arc::new(**layer), *block)
            }
            horizon_client::chunk::Light::Block(layer, sky) => {
                Light::Block(Arc::new(**layer), *sky)
            }
            horizon_client::chunk::Light::Full(all) => Light::Full(Arc::new(**all)),
        };
        sections.push(Section {
            blocks: Blocks::from_ids(&ids),
            light,
        });
    }
    sections.reverse();
    let mut tints = ColumnTints::plains();
    let (cx, cz) = (chunk.x, chunk.z);
    for (col, top) in tops.iter().enumerate() {
        let (lx, lz) = ((col & 15) as i32, (col >> 4) as i32);
        let y = if *top == i32::MIN { min_y } else { *top };
        let id = chunk.biome(lx, y, lz);
        let info = biomes.get(id);
        tints.grass[col] =
            biomes.grass(id, f64::from(cx * 16 + lx), f64::from(cz * 16 + lz)) & 0xFF_FFFF;
        tints.foliage[col] = info.foliage & 0xFF_FFFF;
        tints.dry_foliage[col] = info.dry_foliage & 0xFF_FFFF;
        tints.water[col] = info.water & 0xFF_FFFF;
        tints.fog[col] = info.fog.unwrap_or(default_fog) & 0xFF_FFFF;
    }
    ChunkSnapshot::new(cx, cz, min_y >> 4, sections, Arc::new(tints), |id| {
        air.contains(&id)
    })
}

/// Starts building the block looks for resource generation `generation`: what the block table
/// says of each state here, the colours of their textures on a thread of their own.
fn start_looks(game: &Game<'_>, generation: u64) -> (LooksJob, Arc<HashSet<u32>>) {
    let facts = game.res.facts();
    let registry = game.pack.context.assets.registry;
    let air: HashSet<u32> = ["minecraft:air", "minecraft:cave_air", "minecraft:void_air"]
        .iter()
        .filter_map(|n| registry.default_state(n))
        .flat_map(|default| {
            // Every state of an air block (they have none but the default, as it happens).
            let name = registry
                .state(default)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            (0..registry.len() as u32).filter(move |i| {
                registry
                    .state(BlockStateId(*i))
                    .is_some_and(|s| s.name == name)
            })
        })
        .collect();
    let water = registry.default_state("minecraft:water").map(|s| s.0);
    let lava = registry.default_state("minecraft:lava").map(|s| s.0);
    let mut pre: Vec<StateInput> = Vec::with_capacity(facts.states.len());
    for (id, f) in facts.states.iter().enumerate() {
        let id = id as u32;
        let mut input = StateInput {
            emission: f.emission,
            map_color: horizon_client::blocks::MAP_COLORS
                .get(usize::from(f.map_color))
                .copied()
                .unwrap_or(0),
            leaves: f.classes.iter().any(|c| &**c == "LeavesBlock"),
            ..StateInput::default()
        };
        let no_fluid = f.fluid == horizon_client::blocks::Fluid::None;
        input.kind = if air.contains(&id)
            || f.render_shape == horizon_client::blocks::RenderShape::Invisible && no_fluid
        {
            Kind::Air
        } else {
            let no_collision = !game.collision.may_collide(BlockStateId(id));
            if !no_fluid && no_collision {
                if f.fluid == horizon_client::blocks::Fluid::Lava {
                    Kind::Lava
                } else {
                    Kind::Water
                }
            } else if no_collision {
                if f.classes.iter().any(|c| &**c == "SnowLayerBlock") {
                    Kind::Solid
                } else {
                    Kind::Plant
                }
            } else {
                Kind::Solid
            }
        };
        if input.kind == Kind::Solid {
            input.outline = f.outline.iter().map(|b| b.map(|v| v as f32)).collect();
        }
        if f.waterlogged {
            input.wet = water;
        }
        pre.push(input);
    }
    let context = game.pack.context.clone();
    let air_id = registry.default_state("minecraft:air").map_or(0, |s| s.0);
    let (tx, done) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("diorama-looks".into())
        .spawn(move || {
            let started = Instant::now();
            let looks = colour_looks(pre, &context, water, lava, air_id);
            log::info!(
                "Diorama: block colours for {} states in {:?}",
                looks.len(),
                started.elapsed()
            );
            let _ = tx.send(looks);
        });
    if let Err(e) = spawned {
        log::warn!("Diorama: no thread for block colours: {e}");
    }
    (LooksJob { generation, done }, Arc::new(air))
}

/// The colours of every state's top and front from its model's textures, then the table.
fn colour_looks(
    mut states: Vec<StateInput>,
    context: &horizon_client::streaming::WorkerContext,
    water: Option<u32>,
    lava: Option<u32>,
    air: u32,
) -> BlockLooks {
    use horizon_client::block_sprites::Lookup;
    use horizon_client::mesher::TintSource;
    let lookup = Lookup::new(context, u32::MAX);
    let pixels = context.render.pixels.get();
    let size = context.render.sprite_size as usize;
    let layer_bytes = size * size * 4;
    let mut averages: HashMap<u32, Average> = HashMap::new();
    let mut average = |layer: u32| -> Average {
        *averages.entry(layer).or_insert_with(|| {
            let at = layer as usize * layer_bytes;
            pixels
                .get(at..at + layer_bytes)
                .map_or(Average::default(), Average::of_rgba)
        })
    };
    let tint = |t: Option<TintSource>| -> Option<FaceTint> {
        t.map(|t| match t {
            TintSource::Grass => FaceTint::Biome(TintKind::Grass),
            TintSource::Foliage => FaceTint::Biome(TintKind::Foliage),
            TintSource::DryFoliage => FaceTint::Biome(TintKind::DryFoliage),
            TintSource::Water => FaceTint::Biome(TintKind::Water),
            TintSource::Fixed(c) => FaceTint::Fixed(c & 0xFF_FFFF),
        })
    };
    // Up, then south (north where a model has nothing south), as Diorama reads them.
    const UP: usize = 1;
    const NORTH: usize = 2;
    const SOUTH: usize = 3;
    let mut fluid_look = |id: Option<u32>| -> Option<(u32, Option<FaceTint>)> {
        let sprites = lookup.state(BlockStateId(id?));
        let face = sprites.sides[UP].first()?;
        let a = average(face.sprite.layer);
        (a.coverage > 0.0).then(|| {
            let c = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
            ((c(a.r) << 16) | (c(a.g) << 8) | c(a.b), tint(face.tint))
        })
    };
    let water_look = fluid_look(water);
    let lava_look = fluid_look(lava);
    for (id, s) in states.iter_mut().enumerate() {
        match s.kind {
            Kind::Air => continue,
            Kind::Water => {
                s.fluid = water_look;
                if s.fluid.is_some() {
                    continue;
                }
            }
            Kind::Lava => {
                s.fluid = lava_look;
                if s.fluid.is_some() {
                    continue;
                }
            }
            _ => {}
        }
        let sprites = lookup.state(BlockStateId(id as u32));
        let mut top = FaceColor::default();
        let mut side = FaceColor::default();
        for f in &sprites.sides[UP] {
            top.add(average(f.sprite.layer), tint(f.tint));
        }
        for f in &sprites.sides[SOUTH] {
            side.add(average(f.sprite.layer), tint(f.tint));
        }
        if side.is_empty() {
            for f in &sprites.sides[NORTH] {
                side.add(average(f.sprite.layer), tint(f.tint));
            }
        }
        if top.is_empty()
            && side.is_empty()
            && let Some(particle) = sprites.particle
        {
            // Crossed plants and blocks drawn only as block entities: their particle texture,
            // tinted as the model's first tint asks.
            let t = tint(context.tables.tint_source(BlockStateId(id as u32), 0));
            let a = average(particle.layer);
            top.add(a, t);
            side.add(a, t);
        }
        s.top = top;
        s.side = side;
    }
    looks::build(&states, air)
}

// ---------------------------------------------------------------------------------------------
// Drawing

/// The GUI scale and size, from the window.
fn gui(game: &Game<'_>) -> Option<(f32, [f32; 2], [u32; 2])> {
    let [w, h] = game.native_physical_window_size()?;
    let scale = horizon_client::hud::GUI_SCALE
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1) as f32;
    Some((scale, [w as f32 / scale, h as f32 / scale], [w, h]))
}

/// Once a frame: the world's changes at 20 a second, then bakes handed on, the view on show
/// composed and uploaded when it changed.
pub(super) fn frame(game: &mut Game<'_>, _elapsed: Duration) {
    if !game.native_on(ID) {
        return;
    }
    send_outbox(game);
    let due = with_state(game, |s| {
        let now = Instant::now();
        let due = s.last_tick.is_none_or(|t| now.duration_since(t) >= TICK);
        if due {
            s.last_tick = Some(now);
        }
        due
    });
    if due {
        tick(game);
    }
    let Some(player) = game.native_player() else {
        return;
    };
    let Some((scale, gui_size, physical)) = gui(game) else {
        return;
    };
    let fullscreen = game.native_mod_screen() == Some(ID);
    let now = Instant::now();
    let upload = with_state(game, |s| {
        let settings = s.settings;
        let underground = s.underground;
        let time = now.duration_since(s.started).as_secs_f64();
        let own = s.session.as_mut()?;
        own.tick(now, [player.x, player.y, player.z]);
        let here = own.uv([player.x, player.y, player.z]);
        let cave = own
            .cave_layer()
            .filter(|_| s.viewing.is_none() || !fullscreen);
        let view = if fullscreen {
            if s.map.follow && s.viewing.is_none() {
                s.map.center = here;
            }
            let fit = (f64::from(IMAGE_MAX) / f64::from(physical[0]))
                .min(f64::from(IMAGE_MAX) / f64::from(physical[1]))
                .min(1.0);
            let (w, h) = (
                (f64::from(physical[0]) * fit) as usize,
                (f64::from(physical[1]) * fit) as usize,
            );
            let zoom = s.map.zoom * f64::from(scale) * fit;
            let layer = if s.map.caves {
                cave.unwrap_or(TileKey::SURFACE)
            } else {
                TileKey::SURFACE
            };
            View {
                center_u: s.map.center.0,
                center_v: s.map.center.1,
                zoom,
                layer,
                lod: view::lod_for(s.map.zoom, zoom, layer != TileKey::SURFACE),
                width: w.max(1),
                height: h.max(1),
                round: false,
            }
        } else {
            if !settings.minimap {
                return None;
            }
            let side = ((SIZES[settings.size] * scale) as usize).min(IMAGE_MAX as usize);
            let gui_zoom = ZOOMS[settings.zoom];
            let zoom = side as f64 / f64::from(SIZES[settings.size]) * gui_zoom;
            let layer = if underground {
                cave.unwrap_or(TileKey::SURFACE)
            } else {
                TileKey::SURFACE
            };
            View {
                center_u: here.0,
                center_v: here.1,
                zoom,
                layer,
                lod: view::lod_for(gui_zoom, zoom, layer != TileKey::SURFACE),
                width: side,
                height: side,
                round: settings.round,
            }
        };
        let session = match s.viewing.as_mut() {
            Some(v) if fullscreen => {
                v.session.tick(now, [player.x, player.y, player.z]);
                &mut v.session
            }
            _ => s.session.as_mut()?,
        };
        let version = session.cache_version();
        let fresh = s.shown.as_ref().is_none_or(|sh| {
            sh.fullscreen != fullscreen
                || sh.view != view
                || sh.cache_version != version
                || now.duration_since(sh.at) >= FOG_FRAME
                || sh.gui != gui_size
        });
        if !fresh {
            return None;
        }
        let fog = if view.layer == TileKey::SURFACE {
            Fog {
                time,
                ..Fog::default()
            }
        } else {
            Fog { time, ..Fog::CAVE }
        };
        // The minimap is as see-through as the settings say; the fullscreen map stays opaque
        // (MapViewport.java:133-140).
        let mut look = session.look(view.layer);
        if !fullscreen {
            look.opacity = settings.opacity();
        }
        s.pixels.resize(view.width * view.height, 0);
        session.compose(&view, &look, Some(&fog), &mut s.pixels);
        view::to_rgba(&s.pixels, &mut s.rgba);
        s.shown = Some(Shown {
            fullscreen,
            view,
            cache_version: version,
            at: now,
            gui: gui_size,
        });
        Some((
            view.width as u32,
            view.height as u32,
            std::mem::take(&mut s.rgba),
        ))
    });
    if let Some((w, h, rgba)) = upload {
        if !game.native_mod_image(w, h, &rgba) {
            log::debug!("Diorama: the HUD has no room for a {w}x{h} map image");
        }
        with_state(game, |s| s.rgba = rgba);
    }
}

/// The player arrow: the map item's player marker, turned to where they look on this map.
fn arrow(game: &Game<'_>, batch: &mut HudBatch, at: [f32; 2], yaw: f32, facing: Facing, size: f32) {
    let Some(r) = game
        .gui
        .atlas
        .sprites
        .get("map/decorations/player")
        .copied()
    else {
        game.native_fill(batch, [at[0] - 2.0, at[1] - 2.0, 4.0, 4.0], 0xFFFF_FFFF);
        return;
    };
    // Its arrow points down the texture, so up on screen is half a turn; the map's facing turns it back.
    let degrees = yaw + 180.0 - 90.0 * facing.turns() as f32 + 180.0;
    let (s, c) = degrees.to_radians().sin_cos();
    let p = |dx: f32, dy: f32| [at[0] + dx * c - dy * s, at[1] + dx * s + dy * c];
    let h = size / 2.0;
    batch.quad(
        [p(-h, h), p(h, h), p(h, -h), p(-h, -h)],
        [[r.u0, r.v0], [r.u1, r.v0], [r.u1, r.v1], [r.u0, r.v1]],
        u32::MAX,
    );
}

/// A waypoint pin's box in GUI pixels: centred on `at`, or standing on it with a tail beneath.
fn pin_box(game: &Game<'_>, label: &str, at: [f32; 2], centred: bool) -> [f32; 4] {
    let w = (game.native_text_width(label) + 4.0).max(PIN_H);
    let top = if centred {
        at[1] - PIN_H / 2.0
    } else {
        at[1] - PIN_H - 2.0
    };
    [at[0] - w / 2.0, top, w, PIN_H]
}

/// Draws a waypoint's pin: a box in its colour with its letters, faded when it is held at the
/// edge of the view.
fn draw_pin(
    game: &Game<'_>,
    batch: &mut HudBatch,
    w: &Waypoint,
    at: [f32; 2],
    centred: bool,
    faded: bool,
) -> [f32; 4] {
    let label = w.label();
    let b @ [x, y, bw, bh] = pin_box(game, &label, at, centred);
    let alpha: u32 = if faded { 0xA0 } else { 0xFF };
    let edge = (alpha << 24) | 0x10_1418;
    game.native_fill(batch, [x - 1.0, y - 1.0, bw + 2.0, bh + 2.0], edge);
    game.native_fill(batch, b, (alpha << 24) | (w.color & 0xFF_FFFF));
    if !centred {
        game.native_fill(batch, [at[0] - 1.0, y + bh + 1.0, 2.0, 1.0], edge);
    }
    let (r, g, bl) = (
        (w.color >> 16) & 0xFF,
        (w.color >> 8) & 0xFF,
        w.color & 0xFF,
    );
    let light = 299 * r + 587 * g + 114 * bl > 150_000;
    let tw = game.native_text_width(&label);
    game.native_text(
        batch,
        &label,
        [x + (bw - tw) / 2.0, y + 1.0],
        if light { 0x10_1418 } else { 0xFF_FFFF },
        !light,
    );
    b
}

/// How far above or below the player the radar looks, in blocks.
const RADAR_RANGE: f64 = 32.0;

/// The players, mobs and dropped items the radar shows: all but the player and what can't be
/// seen.
fn radar_entities(game: &Game<'_>, items: bool, me: [f64; 3]) -> Vec<radar::Entity> {
    let mut out: Vec<radar::Entity> = game
        .mobs
        .iter()
        .filter(|m| {
            !m.removed && m.alive() && !m.invisible && !m.has_effect("minecraft:invisibility")
        })
        .filter_map(|m| {
            let kind = radar::kind(&m.kind.id, &m.kind.category)?;
            let pos = [m.position.x, m.position.y, m.position.z];
            let player = kind == radar::Kind::Player;
            if player
                && (pos[0] - me[0]).hypot(pos[2] - me[2]) < 0.3
                && (pos[1] - me[1]).abs() < 0.5
            {
                return None;
            }
            let name = player
                .then(|| game.remote_player_name(m.id).map(str::to_owned))
                .flatten();
            Some(radar::Entity { kind, pos, name })
        })
        .collect();
    if items {
        out.extend(game.dropped.iter().map(|i| radar::Entity {
            kind: radar::Kind::Item,
            pos: [i.position.x, i.position.y, i.position.z],
            name: None,
        }));
    }
    out
}

/// Draws radar dots placed in an image shown at `origin` with `scale` GUI pixels per image pixel.
/// `size` is a dot's side by kind; players' names show beside them when `names`.
#[allow(clippy::too_many_arguments)]
fn draw_blips(
    game: &Game<'_>,
    batch: &mut HudBatch,
    blips: &[radar::Blip],
    entities: &[radar::Entity],
    origin: [f32; 2],
    scale: f32,
    size: impl Fn(radar::Kind) -> f32,
    names: bool,
) {
    for b in blips {
        let s = size(b.kind);
        let (x, y) = (
            origin[0] + b.x as f32 * scale,
            origin[1] + b.y as f32 * scale,
        );
        let (x0, y0) = ((x - s / 2.0).round(), (y - s / 2.0).round());
        // Well above or below: faded, with a mark on the side it is.
        let far = b.dy.abs() > radar::CARET_HEIGHT;
        let alpha: u32 = if far { 0xB0 } else { 0xFF };
        game.native_fill(
            batch,
            [x0 - 1.0, y0 - 1.0, s + 2.0, s + 2.0],
            (alpha << 24) | 0x10_1418,
        );
        game.native_fill(
            batch,
            [x0, y0, s, s],
            (alpha << 24) | (b.kind.color() & 0xFF_FFFF),
        );
        if far && b.kind != radar::Kind::Item {
            let cx = (x - 0.5).round();
            let (tip, base) = if b.dy > 0.0 {
                (y0 - 4.0, y0 - 3.0)
            } else {
                (y0 + s + 3.0, y0 + s + 2.0)
            };
            game.native_fill(batch, [cx, tip, 1.0, 1.0], 0xFFFF_FFFF);
            game.native_fill(batch, [cx - 1.0, base, 3.0, 1.0], 0xFFFF_FFFF);
        }
        if names && let Some(name) = &entities[b.index].name {
            game.native_text(
                batch,
                name,
                [x0 + s + 3.0, y0 + s / 2.0 - 4.0],
                0xFFFFFF,
                true,
            );
        }
    }
}

/// A biome or dimension id for people: `minecraft:dark_forest` is Dark Forest.
fn pretty_id(id: &str) -> String {
    id.rsplit(':')
        .next()
        .unwrap_or(id)
        .split(['_', '/'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The biome at a world position, from the client's chunks.
fn biome_at(game: &Game<'_>, p: [f64; 3]) -> Option<String> {
    let (x, y, z) = (
        p[0].floor() as i32,
        p[1].floor() as i32,
        p[2].floor() as i32,
    );
    let chunk = game.world.chunks.get(&(x >> 4, z >> 4))?;
    let id = chunk.biome(x & 15, y, z & 15);
    Some(pretty_id(&game.pack.context.biomes.get(id).name))
}

/// The time of day as Xaero's Minimap writes it (MinimapHud.gameTime, MinimapHud.java:310-323):
/// day time 0 is 06:00 on day 1, and a new day starts at midnight.
fn clock_text(format: ClockFormat, day_time: i64) -> String {
    let since_midnight = day_time + 6000;
    let day = since_midnight.div_euclid(24_000) + 1;
    let t = since_midnight.rem_euclid(24_000);
    let (hour, minute) = (t / 1000, t % 1000 * 60 / 1000);
    let twelve = matches!(format, ClockFormat::Day12h | ClockFormat::Only12h);
    let time = if twelve {
        let h = if hour % 12 == 0 { 12 } else { hour % 12 };
        format!("{h:02}:{minute:02} {}", if hour < 12 { "AM" } else { "PM" })
    } else {
        format!("{hour:02}:{minute:02}")
    };
    if matches!(format, ClockFormat::Day24h | ClockFormat::Day12h) {
        format!("Day {day}, {time}")
    } else {
        time
    }
}

/// Where a world point shows on screen through `camera`, in GUI pixels of a screen `size`
/// across; none behind the camera.
fn project(camera: &NativeCamera, p: [f64; 3], size: [f32; 2]) -> Option<[f32; 2]> {
    let rel = [
        (p[0] - camera.position[0]) as f32,
        (p[1] - camera.position[1]) as f32,
        (p[2] - camera.position[2]) as f32,
        1.0,
    ];
    // Column-major 4x4 times a column vector.
    let mul = |m: &[f32; 16], v: [f32; 4]| -> [f32; 4] {
        std::array::from_fn(|row| (0..4).map(|col| m[col * 4 + row] * v[col]).sum())
    };
    let clip = mul(&camera.projection, mul(&camera.view, rel));
    if clip[3] <= 1e-3 {
        return None;
    }
    let (nx, ny) = (clip[0] / clip[3], clip[1] / clip[3]);
    Some([(nx * 0.5 + 0.5) * size[0], (0.5 - ny * 0.5) * size[1]])
}

/// Waypoints floating in the world, a pin on each with its distance under it, and its name too
/// for the one nearest the crosshair. Far ones first, so near ones are drawn over them.
fn world_pins(
    game: &Game<'_>,
    batch: &mut HudBatch,
    pins: &[(usize, Waypoint)],
    dimension: &str,
    size: [f32; 2],
) {
    let Some(camera) = game.native_camera() else {
        return;
    };
    if camera.dimension != dimension {
        return;
    }
    let mut placed: Vec<(f64, [f32; 2], &Waypoint)> = pins
        .iter()
        .filter_map(|(_, w)| {
            let [x, y, z] = w.point();
            let at = project(&camera, [x, y + 1.0, z], size)?;
            let on_screen = (0.0..size[0]).contains(&at[0]) && (0.0..size[1]).contains(&at[1]);
            on_screen.then(|| (w.distance_to(camera.position), at, w))
        })
        .collect();
    placed.sort_by(|a, b| b.0.total_cmp(&a.0));
    let centre = [size[0] / 2.0, size[1] / 2.0];
    let aimed = placed
        .iter()
        .map(|(_, at, _)| (at[0] - centre[0]).hypot(at[1] - centre[1]))
        .enumerate()
        .filter(|(_, d)| *d < 20.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i);
    for (i, (d, at, w)) in placed.iter().enumerate() {
        draw_pin(game, batch, w, *at, false, false);
        let text = if Some(i) == aimed {
            format!("{} ({d:.0} m)", w.name)
        } else {
            format!("{d:.0} m")
        };
        let tw = game.native_text_width(&text);
        let ty = at[1] + 3.0;
        game.native_fill(
            batch,
            [at[0] - tw / 2.0 - 2.0, ty - 1.0, tw + 4.0, 10.0],
            0x8000_0000,
        );
        game.native_text(batch, &text, [at[0] - tw / 2.0, ty], 0xFFFFFF, false);
    }
}

/// The minimap in its corner of the HUD.
pub(super) fn hud(game: &mut Game<'_>, batch: &mut HudBatch, corners: &mut Corners) {
    if !game.native_on(ID) || game.native_mod_screen() == Some(ID) {
        return;
    }
    let Some(player) = game.native_player() else {
        return;
    };
    let world = with_state(game, |s| {
        s.settings
            .world_pins
            .then(|| (s.pins(), s.dimension.clone()))
    });
    if let Some((pins, dimension)) = world {
        let size = [batch.width, batch.height];
        world_pins(game, batch, &pins, &dimension, size);
    }
    let shown = with_state(game, |s| {
        let sh = s.shown.filter(|sh| !sh.fullscreen && s.settings.minimap)?;
        Some((
            sh,
            s.settings,
            s.underground,
            s.session.as_ref().map(|se| *se.projection()),
            if s.settings.pins {
                s.pins()
            } else {
                Vec::new()
            },
        ))
    });
    let Some((shown, settings, underground, Some(projection), pins)) = shown else {
        return;
    };
    let corner = Corner::ALL[settings.corner];
    if !game.native_corner_shown(corner) {
        return;
    }
    let size = SIZES[settings.size];
    let here = [player.x, player.y, player.z];
    let mut lines = Vec::new();
    if settings.coordinates {
        lines.push(format!(
            "{} {} {}{}",
            player.x.floor() as i64,
            player.y.floor() as i64,
            player.z.floor() as i64,
            if underground { " (cave)" } else { "" }
        ));
    }
    if settings.biome
        && let Some(biome) = biome_at(game, here)
    {
        lines.push(biome);
    }
    if settings.clock
        && let Some(clock) = game.native_clock()
    {
        lines.push(clock_text(settings.clock_format, clock.day_time));
    }
    let text_h = 10.0 * lines.len() as f32;
    let (x, y) = corners.place(corner, size, size + text_h);
    let map_y = if corner.top() { y } else { y + text_h };
    if !settings.round && settings.frame != Frame::None {
        game.native_fill(
            batch,
            [x - 1.0, map_y - 1.0, size + 2.0, size + 2.0],
            0xFF00_0000,
        );
    }
    let img = [shown.view.width as f32, shown.view.height as f32];
    game.native_sprite(
        batch,
        IMAGE_SPRITE,
        [0.0, 0.0, img[0], img[1]],
        [x, map_y, size, size],
    );
    // The compass: the map looks toward its facing, so that is up, and the rest go clockwise.
    let letter = |f: Facing| match f {
        Facing::North => "N",
        Facing::East => "E",
        Facing::South => "S",
        Facing::West => "W",
    };
    let (mx, my, half) = (x + size / 2.0, map_y + size / 2.0, size / 2.0 - 5.0);
    let compass: &[(f32, f32)] = if settings.compass {
        &[(0.0, -1.0), (1.0, 0.0), (0.0, 1.0), (-1.0, 0.0)]
    } else {
        &[]
    };
    for (turn, &(dx, dy)) in compass.iter().enumerate() {
        let text = letter(projection.facing.next(turn as i32));
        let tw = game.native_text_width(text);
        let at = [mx + dx * half - tw / 2.0, my + dy * half - 4.0];
        let rgb = if turn == 0 { 0xFF_5555 } else { 0xE0_E0E0 };
        game.native_text(batch, text, at, rgb, true);
    }
    let per_px = img[0] / size;
    if settings.radar {
        let entities = radar_entities(game, settings.radar_items, here);
        let blips = radar::place(
            &shown.view,
            &projection,
            &entities,
            player.y,
            RADAR_RANGE,
            0.0,
        );
        let size_of = |k: radar::Kind| match k {
            radar::Kind::Item => 2.0,
            radar::Kind::Player => 5.0,
            _ => 3.0,
        };
        draw_blips(
            game,
            batch,
            &blips,
            &entities,
            [x, map_y],
            1.0 / per_px,
            size_of,
            false,
        );
    }
    // Waypoints: where they are, or held at the edge toward them.
    let inset = f64::from((PIN_H / 2.0 + 1.0) * per_px);
    for (_, w) in &pins {
        let [px, py, pz] = w.point();
        let (u, v) = (projection.u(px, pz), projection.v(px, py, pz));
        if let Some(spot) = waypoint::pin_spot(&shown.view, u, v, inset, true) {
            let at = [x + spot.x as f32 / per_px, map_y + spot.y as f32 / per_px];
            draw_pin(game, batch, w, at, true, spot.edge);
        }
    }
    arrow(
        game,
        batch,
        [x + size / 2.0, map_y + size / 2.0],
        player.yaw,
        projection.facing,
        8.0,
    );
    let top = if corner.top() { map_y + size + 2.0 } else { y };
    for (i, text) in lines.iter().enumerate() {
        let w = game.native_text_width(text);
        let ty = top + 10.0 * i as f32;
        game.native_text(batch, text, [x + (size - w) / 2.0, ty], 0xFFFFFF, true);
    }
}

/// What the fullscreen map shows over itself.
#[derive(Clone, Debug, Default)]
enum Panel {
    #[default]
    None,
    /// The waypoint list, from row `scroll`.
    List { scroll: usize },
    /// A waypoint's editor, and when the waypoint was made, to tell it is still the same one.
    Edit(Form, u64),
}

/// What the frame does for the screen hooks, which only read the game.
enum Outgoing {
    /// A chat line, or a command (`/` first), to the server.
    Chat(String),
    /// The player to a place, in a dimension.
    Teleport { at: [i32; 3], dimension: String },
    /// The map screen closes.
    Close,
}

const PANEL_BG: u32 = 0xEC14_171C;
const PANEL_EDGE: u32 = 0xFF3A_404A;
const BUTTON_BG: u32 = 0xFF2A_2F38;
const BUTTON_HOVER: u32 = 0xFF41_4858;
/// The map screen's button to the waypoint list, in its top right corner.
fn list_button(size: [f32; 2]) -> Rect {
    const W: f32 = 92.0;
    [size[0] - W - 6.0, 6.0, W, wui::BUTTON_H]
}

/// The map screen's button to show another dimension's map, left of the list button.
fn dimension_button(size: [f32; 2]) -> Rect {
    let list = list_button(size);
    const W: f32 = 120.0;
    [list[0] - W - 6.0, list[1], W, list[3]]
}

/// Shows the next mapped dimension on the fullscreen map, about the same place in it.
fn switch_dimension(s: &mut State) {
    let Some(root) = s.map_root.clone() else {
        return;
    };
    let facing = s.settings.facing;
    let mut dims = mapped_dimensions(&root, facing);
    if !dims.contains(&s.dimension) {
        dims.insert(0, s.dimension.clone());
    }
    let current = s.map_dimension().to_owned();
    let next = dims[dims
        .iter()
        .position(|d| *d == current)
        .map_or(0, |i| (i + 1) % dims.len())]
    .clone();
    if next == current {
        s.toast = Some((
            "No other dimension is mapped yet".to_owned(),
            Instant::now(),
        ));
        return;
    }
    // The world point at the map's middle, at sea level, in the next dimension's coordinates.
    let centre = s.map_session().map(|se| {
        let (x, z) = se
            .projection()
            .world_at(s.map.center.0, s.map.center.1, 64.0);
        let k = coordinate_scale(&current) / coordinate_scale(&next);
        [x * k, 64.0, z * k]
    });
    s.viewing = None;
    if next != s.dimension {
        let session = MapSession::new(SessionConfig {
            folder: map_folder(&root, &next, facing),
            min_y: -64,
            max_y: 320,
            roof: None,
            style: MapStyle::DEFAULT,
            facing,
            fast: s.settings.fast,
            threads: 1,
        });
        s.viewing = Some(Viewer {
            dimension: next.clone(),
            session,
        });
    }
    if let (Some(c), Some(se)) = (centre, s.map_session()) {
        s.map.center = se.uv(c);
    }
    s.map.follow = false;
    s.map.caves = false;
    s.shown = None;
    s.toast = Some((
        format!("Showing the {}", dimension_name(&next)),
        Instant::now(),
    ));
}

fn panel(game: &Game<'_>, batch: &mut HudBatch, r: Rect) {
    game.native_fill(
        batch,
        [r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0],
        PANEL_EDGE,
    );
    game.native_fill(batch, r, PANEL_BG);
}

fn button(game: &Game<'_>, batch: &mut HudBatch, r: Rect, label: &str, mouse: [f32; 2]) {
    let bg = if wui::inside(r, mouse) {
        BUTTON_HOVER
    } else {
        BUTTON_BG
    };
    game.native_fill(batch, r, bg);
    let tw = game.native_text_width(label);
    game.native_text(
        batch,
        label,
        [r[0] + (r[2] - tw) / 2.0, r[1] + (r[3] - 8.0) / 2.0],
        0xFFFFFF,
        true,
    );
}

/// The end of `text` that fits in `width` GUI pixels, or its start with `…` when `head`.
fn fit(game: &Game<'_>, text: &str, width: f32, head: bool) -> String {
    if game.native_text_width(text) <= width {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        if head {
            chars.pop();
            let t: String = chars.iter().collect::<String>() + "…";
            if game.native_text_width(&t) <= width {
                return t;
            }
        } else {
            chars.remove(0);
            let t: String = chars.iter().collect();
            if game.native_text_width(&t) <= width {
                return t;
            }
        }
    }
    String::new()
}

/// A dimension's name for people: Overworld, Nether, End, or a modded one's path.
fn dimension_name(id: &str) -> String {
    match id {
        "minecraft:overworld" => "Overworld".to_owned(),
        "minecraft:the_nether" => "Nether".to_owned(),
        "minecraft:the_end" => "End".to_owned(),
        _ => {
            let path = id.rsplit(':').next().unwrap_or(id).replace(['_', '/'], " ");
            let mut c = path.chars();
            c.next()
                .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_list(
    game: &Game<'_>,
    batch: &mut HudBatch,
    size: [f32; 2],
    mouse: [f32; 2],
    all: &[Waypoint],
    order: &[usize],
    scroll: usize,
    dimension: &str,
    player: Option<[f64; 3]>,
) {
    let l = wui::list_layout(size, order, scroll);
    panel(game, batch, l.panel);
    game.native_text(batch, "Waypoints", l.title, 0xFFFFFF, true);
    if !order.is_empty() {
        let range = format!(
            "{}-{} of {}",
            l.scroll + 1,
            l.scroll + l.rows.len(),
            order.len()
        );
        let tw = game.native_text_width(&range);
        game.native_text(
            batch,
            &range,
            [l.panel[0] + l.panel[2] - 8.0 - tw, l.title[1]],
            0x9098A0,
            false,
        );
    } else {
        let empty = "None yet: B adds one where you stand.";
        game.native_text(
            batch,
            empty,
            [l.panel[0] + 8.0, l.panel[1] + 25.0],
            0xA0A8B0,
            false,
        );
    }
    for (i, row, toggle) in &l.rows {
        let w = &all[*i];
        if wui::inside(*row, mouse) {
            game.native_fill(batch, *row, 0x30FF_FFFF);
        }
        let alpha: u32 = if w.visible { 0xFF } else { 0x60 };
        game.native_fill(
            batch,
            [row[0] + 2.0, row[1] + 2.0, 9.0, 9.0],
            (alpha << 24) | (w.color & 0xFF_FFFF),
        );
        let rgb = if w.visible { 0xFFFFFF } else { 0x80868C };
        game.native_text(batch, &w.label(), [row[0] + 15.0, row[1] + 3.0], rgb, true);
        let info = if w.dimension == dimension {
            player.map_or(String::new(), |p| format!("{:.0} m", w.distance_to(p)))
        } else {
            dimension_name(&w.dimension)
        };
        let iw = game.native_text_width(&info);
        game.native_text(
            batch,
            &info,
            [row[0] + row[2] - 3.0 - iw, row[1] + 3.0],
            0x9098A0,
            false,
        );
        let name = fit(game, &w.name, row[2] - 48.0 - iw - 8.0, true);
        game.native_text(batch, &name, [row[0] + 44.0, row[1] + 3.0], rgb, false);
        button(
            game,
            batch,
            *toggle,
            if w.visible { "Hide" } else { "Show" },
            mouse,
        );
    }
    button(game, batch, l.add, "Add where I stand", mouse);
    button(game, batch, l.done, "Done", mouse);
}

fn draw_editor(
    game: &Game<'_>,
    batch: &mut HudBatch,
    size: [f32; 2],
    mouse: [f32; 2],
    f: &Form,
    blink: bool,
) {
    let e = wui::editor_layout(size);
    panel(game, batch, e.panel);
    game.native_text(batch, "Edit waypoint", e.title, 0xFFFFFF, true);
    for i in 0..wui::FIELDS.len() {
        let r = e.fields[i];
        game.native_text(batch, wui::FIELDS[i], e.labels[i], 0xA0A8B0, false);
        let edge = if f.focus == i {
            0xFFE0_E4E8
        } else {
            0xFF5A_606A
        };
        game.native_fill(
            batch,
            [r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0],
            edge,
        );
        game.native_fill(batch, r, 0xFF0A_0C10);
        let mut text = fit(game, &f.fields[i], r[2] - 10.0, false);
        if f.focus == i && blink {
            text.push('_');
        }
        game.native_text(batch, &text, [r[0] + 3.0, r[1] + 3.0], 0xFFFFFF, false);
    }
    for (i, r) in e.swatches.iter().enumerate() {
        let c = wui::swatch(i);
        if c & 0xFF_FFFF == f.color & 0xFF_FFFF {
            game.native_fill(
                batch,
                [r[0] - 2.0, r[1] - 2.0, r[2] + 4.0, r[3] + 4.0],
                0xFFFF_FFFF,
            );
            game.native_fill(
                batch,
                [r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0],
                0xFF00_0000,
            );
        }
        game.native_fill(batch, *r, c);
    }
    for (b, r) in e.buttons {
        button(game, batch, r, b.label(f.visible), mouse);
    }
    if let Some(error) = f.error {
        game.native_text(batch, error, e.error, 0xFF5555, false);
    }
}

/// The fullscreen map, drawn over its screen.
pub(super) fn screen_draw(game: &Game<'_>, batch: &mut HudBatch, screen: &NativeScreenView<'_>) {
    if screen.id != "mod" || game.native_mod_screen() != Some(ID) {
        return;
    }
    let (w, h) = (batch.width, batch.height);
    game.native_fill(batch, [0.0, 0.0, w, h], 0xFF0E_1014);
    let player = game.native_player();
    let state = with_state_if(|s| {
        let shown = s.shown.filter(|sh| sh.fullscreen);
        let session = s.map_session();
        // The block under the pointer, as the map shows it.
        let under = shown.zip(session).and_then(|(sh, se)| {
            let img = [sh.view.width as f32, sh.view.height as f32];
            let (u, v) = sh.view.map_uv(
                f64::from(screen.mouse[0] * img[0] / sh.gui[0]),
                f64::from(screen.mouse[1] * img[1] / sh.gui[1]),
            );
            se.block_at(u, v, sh.view.layer)
        });
        let all = s
            .waypoints
            .as_ref()
            .map_or_else(Vec::new, |ws| ws.all().to_vec());
        let order = player.as_ref().map_or_else(
            || (0..all.len()).collect(),
            |p| wui::list_order(&all, &s.dimension, [p.x, p.y, p.z]),
        );
        (
            shown,
            session.map(|se| *se.projection()),
            s.map,
            s.map_pins(),
            under,
            s.toast
                .clone()
                .filter(|(_, at)| at.elapsed() < TOAST)
                .map(|(t, _)| t),
            s.panel.clone(),
            all,
            order,
            s.dimension.clone(),
            s.started.elapsed().as_millis() / 500 % 2 == 0,
            s.settings,
            s.viewing.as_ref().map(|v| v.dimension.clone()),
        )
    });
    let Some((
        shown,
        projection,
        map,
        pins,
        under,
        toast,
        panel_on,
        all,
        order,
        dimension,
        blink,
        settings,
        viewing,
    )) = state
    else {
        return;
    };
    let Some(shown) = shown else {
        let msg = "Diorama is getting the map ready...";
        game.native_text(
            batch,
            msg,
            [(w - game.native_text_width(msg)) / 2.0, h / 2.0],
            0xFFFFFF,
            true,
        );
        return;
    };
    let img = [shown.view.width as f32, shown.view.height as f32];
    game.native_sprite(
        batch,
        IMAGE_SPRITE,
        [0.0, 0.0, img[0], img[1]],
        [0.0, 0.0, w, h],
    );
    let to_gui = |u: f64, v: f64| {
        let (ix, iy) = shown.view.image_xy(u, v);
        [ix as f32 * w / img[0], iy as f32 * h / img[1]]
    };
    let on_map = matches!(panel_on, Panel::None);
    let scale = w / img[0];
    if let (Some(proj), Some(p)) = (projection, player.as_ref()) {
        if settings.grid && map.zoom >= 0.5 {
            let y = if viewing.is_some() { 64.0 } else { p.y.floor() };
            let (columns, rows) = view::chunk_grid(&shown.view, &proj, y);
            for cx in columns {
                game.native_fill(batch, [cx as f32 * scale, 0.0, 1.0, h], 0x38FF_FFFF);
            }
            for ry in rows {
                game.native_fill(batch, [0.0, ry as f32 * scale, w, 1.0], 0x38FF_FFFF);
            }
        }
        if settings.radar && viewing.is_none() {
            let entities = radar_entities(game, settings.radar_items, [p.x, p.y, p.z]);
            let blips = radar::place(
                &shown.view,
                &proj,
                &entities,
                p.y,
                RADAR_RANGE,
                8.0 / f64::from(scale),
            );
            let z = map.zoom as f32;
            let size_of = |k: radar::Kind| match k {
                radar::Kind::Item => z.clamp(2.0, 4.0),
                radar::Kind::Player => (z * 3.0).clamp(6.0, 12.0),
                _ => (z * 2.2).clamp(3.0, 8.0),
            };
            draw_blips(
                game,
                batch,
                &blips,
                &entities,
                [0.0, 0.0],
                scale,
                size_of,
                true,
            );
        }
    }
    // Waypoints, and the name of the one under the pointer.
    if let Some(proj) = projection {
        let placed = screen_pins(&shown, &proj, pins);
        let hovered = placed
            .iter()
            .rposition(|(_, wp, at)| on_map && pin_hit(game, wp, *at, screen.mouse));
        for (i, (_, wp, at)) in placed.iter().enumerate() {
            if Some(i) != hovered {
                draw_pin(game, batch, wp, *at, false, false);
            }
        }
        if let Some((_, wp, at)) = hovered.map(|i| &placed[i]) {
            let [x, y, bw, bh] = draw_pin(game, batch, wp, *at, false, false);
            let mut name = wp.name.clone();
            if let Some(p) = &player {
                name = format!("{name} ({:.0} m)", wp.distance_to([p.x, p.y, p.z]));
            }
            let tw = game.native_text_width(&name);
            let (tx, ty) = (x + bw / 2.0 - tw / 2.0, y + bh + 4.0);
            game.native_fill(batch, [tx - 2.0, ty - 1.0, tw + 4.0, 10.0], 0xA000_0000);
            game.native_text(batch, &name, [tx, ty], 0xFFFFFF, false);
        }
    }
    if let (Some(p), Some(proj), None) = (player.as_ref(), projection, &viewing) {
        let at = to_gui(proj.u(p.x, p.z), proj.v(p.x, p.y, p.z));
        arrow(game, batch, at, p.yaw, proj.facing, 10.0);
    }
    // What's under the pointer: the block the map shows there, else the ground at the player's height.
    let mut lines = vec![if let Some(d) = &viewing {
        format!("{} (saved map)", dimension_name(d))
    } else if shown.view.layer == TileKey::SURFACE {
        "Surface".to_owned()
    } else {
        format!("Caves at y {}", shown.view.layer)
    }];
    if let Some([x, y, z]) = under {
        lines.push(format!("{x} {y} {z}"));
    } else if let (Some(proj), Some(p)) = (projection, player.as_ref()) {
        let (u, v) = shown.view.map_uv(
            f64::from(screen.mouse[0] * img[0] / w),
            f64::from(screen.mouse[1] * img[1] / h),
        );
        let (x, z) = proj.world_at(u, v, p.y.floor());
        lines.push(format!("{} ~ {}", x.floor() as i64, z.floor() as i64));
    }
    lines.push(format!("Zoom {:.2} px per block", map.zoom));
    for (i, line) in lines.iter().enumerate() {
        game.native_text(batch, line, [6.0, 6.0 + 10.0 * i as f32], 0xFFFFFF, true);
    }
    let size = [w, h];
    let list_key = game.native_mod_key(ID, "waypoints", "key.keyboard.u");
    let list_label = format!("Waypoints ({})", key_letter(&list_key));
    button(game, batch, list_button(size), &list_label, screen.mouse);
    let shown_dimension = viewing.as_deref().unwrap_or(&dimension);
    let map_label = format!("Map: {}", dimension_name(shown_dimension));
    button(
        game,
        batch,
        dimension_button(size),
        &map_label,
        screen.mouse,
    );
    if on_map {
        let help = [
            "Drag: move · Scroll: zoom · Space: back to you · C: caves · PgUp/PgDn: cave height",
            "Click a pin: edit · Right-click: add or remove · Middle-click: temporary pin",
        ];
        for (i, line) in help.iter().enumerate() {
            let line = fit(game, line, w - 12.0, true);
            let y = h - 12.0 - 10.0 * (help.len() - 1 - i) as f32;
            game.native_text(
                batch,
                &line,
                [(w - game.native_text_width(&line)) / 2.0, y],
                0xC8C8C8,
                true,
            );
        }
    } else {
        game.native_fill(batch, [0.0, 0.0, w, h], 0x7000_0000);
    }
    let here = player.as_ref().map(|p| [p.x, p.y, p.z]);
    match &panel_on {
        Panel::None => {}
        Panel::List { scroll } => draw_list(
            game,
            batch,
            size,
            screen.mouse,
            &all,
            &order,
            *scroll,
            &dimension,
            here,
        ),
        Panel::Edit(form, _) => draw_editor(game, batch, size, screen.mouse, form, blink),
    }
    if let Some(toast) = toast {
        let tw = game.native_text_width(&toast);
        let at = [(w - tw) / 2.0, 26.0];
        game.native_fill(
            batch,
            [at[0] - 4.0, at[1] - 3.0, tw + 8.0, 14.0],
            0xC000_0000,
        );
        game.native_text(batch, &toast, at, 0xFFFFFF, false);
    }
}

/// A key's letter, `U` for `key.keyboard.u`.
fn key_letter(key: &str) -> String {
    key.rsplit('.').next().unwrap_or(key).to_uppercase()
}

/// The fullscreen map's pins in view: each waypoint's index, the waypoint, and its point in GUI
/// pixels.
fn screen_pins(
    shown: &Shown,
    projection: &Projection,
    pins: Vec<(usize, Waypoint)>,
) -> Vec<(usize, Waypoint, [f32; 2])> {
    let img = [shown.view.width as f64, shown.view.height as f64];
    let scale = [
        f64::from(shown.gui[0]) / img[0],
        f64::from(shown.gui[1]) / img[1],
    ];
    pins.into_iter()
        .filter_map(|(i, w)| {
            let [px, py, pz] = w.point();
            let (u, v) = (projection.u(px, pz), projection.v(px, py, pz));
            let spot = waypoint::pin_spot(&shown.view, u, v, 0.0, false)?;
            let at = [(spot.x * scale[0]) as f32, (spot.y * scale[1]) as f32];
            Some((i, w, at))
        })
        .collect()
}

/// Whether the pointer is on a standing pin: its box, down to the point it stands on.
fn pin_hit(game: &Game<'_>, w: &Waypoint, at: [f32; 2], mouse: [f32; 2]) -> bool {
    let [x, y, bw, _] = pin_box(game, &w.label(), at, false);
    (x - 1.0..=x + bw + 1.0).contains(&mouse[0]) && (y - 1.0..=at[1] + 1.0).contains(&mouse[1])
}

/// The pin under the pointer on the fullscreen map, the topmost: its waypoint's index.
fn pin_under(game: &Game<'_>, s: &State, mouse: [f32; 2]) -> Option<usize> {
    let shown = s.shown.filter(|sh| sh.fullscreen)?;
    let projection = *s.map_session()?.projection();
    screen_pins(&shown, &projection, s.map_pins())
        .iter()
        .rev()
        .find(|(_, w, at)| pin_hit(game, w, *at, mouse))
        .map(|(i, _, _)| *i)
}

fn own_screen(game: &Game<'_>, screen: &NativeScreenView<'_>) -> bool {
    screen.id == "mod" && game.native_mod_screen() == Some(ID)
}

/// The editor for waypoint `index`.
fn editor(s: &State, index: usize) -> Option<Panel> {
    let w = s.waypoints.as_ref()?.all().get(index)?;
    Some(Panel::Edit(Form::of(index, w), w.created))
}

pub(super) fn screen_press(
    game: &Game<'_>,
    screen: &NativeScreenView<'_>,
    button: i32,
    canceled: &mut bool,
) {
    if !own_screen(game, screen) {
        return;
    }
    let Some((_, size, _)) = gui(game) else {
        return;
    };
    let mouse = screen.mouse;
    let player = game.native_player().map(|p| [p.x, p.y, p.z]);
    *canceled = true;
    with_state_if(|s| {
        let mut panel = std::mem::take(&mut s.panel);
        match &mut panel {
            Panel::Edit(form, created) => {
                if button == 0 {
                    match wui::editor_layout(size).hit(mouse) {
                        Some(EditorHit::Field(i)) => form.focus = i,
                        Some(EditorHit::Swatch(i)) => form.color = wui::swatch(i),
                        Some(EditorHit::Button(b)) => {
                            if let Some(next) = editor_button(s, form, *created, b) {
                                panel = next;
                            }
                        }
                        None => {}
                    }
                }
            }
            Panel::List { scroll } => {
                if button == 0 {
                    let order = list_order(s, player);
                    match wui::list_layout(size, &order, *scroll).hit(mouse) {
                        Some(ListHit::Open(i)) => {
                            if let Some(p) = editor(s, i) {
                                panel = p;
                            }
                        }
                        Some(ListHit::Toggle(i)) => {
                            if let Some(ws) = s.waypoints.as_mut()
                                && let Err(e) = ws.edit(i, |w| w.visible = !w.visible)
                            {
                                s.toast = Some((format!("Couldn't save: {e}"), Instant::now()));
                            }
                        }
                        Some(ListHit::Add) => {
                            if let Some(p) = player {
                                let at = p.map(|c| c.floor() as i32);
                                if let Some(w) =
                                    s.waypoints.as_ref().map(|ws| ws.next_at(at, &s.dimension))
                                {
                                    let line = s.add_waypoint(w);
                                    s.toast = Some((line, Instant::now()));
                                }
                            }
                        }
                        Some(ListHit::Done) => panel = Panel::None,
                        None => {}
                    }
                }
            }
            Panel::None => match button {
                0 if wui::inside(list_button(size), mouse) => {
                    panel = Panel::List { scroll: 0 };
                }
                0 if wui::inside(dimension_button(size), mouse) => switch_dimension(s),
                0 => {
                    s.map.dragging = true;
                    s.press = Some((mouse, false));
                }
                1 => {
                    if let Some(line) = right_click(game, s, mouse, player.map(|p| p[1])) {
                        s.toast = Some((line, Instant::now()));
                    }
                }
                2 => {
                    if let Some(at) = map_block(s, mouse, player.map(|p| p[1])) {
                        let mut w = Waypoint::new("Temporary", at, s.map_dimension(), 0xFFFF_FFFF);
                        w.kind = waypoint::Kind::Temporary;
                        w.initials = "T".to_owned();
                        let line = s.add_waypoint(w);
                        s.toast = Some((format!("{line} (gone when you leave)"), Instant::now()));
                    }
                }
                _ => {}
            },
        }
        s.panel = panel;
    });
}

/// The list's order for the player at `player`.
fn list_order(s: &State, player: Option<[f64; 3]>) -> Vec<usize> {
    let all = s.waypoints.as_ref().map_or(&[][..], |ws| ws.all());
    match player {
        Some(p) => wui::list_order(all, &s.dimension, p),
        None => (0..all.len()).collect(),
    }
}

/// An editor button pressed: the panel to show next, if it changes.
fn editor_button(s: &mut State, form: &mut Form, created: u64, b: Button) -> Option<Panel> {
    // The waypoint as edited, if it is still there.
    let index = form.index;
    let current = s
        .waypoints
        .as_ref()
        .and_then(|ws| ws.all().get(index))
        .filter(|w| w.created == created)
        .cloned();
    let Some(mut edited) = current else {
        s.toast = Some(("That waypoint is gone".to_owned(), Instant::now()));
        return Some(Panel::List { scroll: 0 });
    };
    let applied = form.apply(&mut edited);
    match b {
        Button::Visible => {
            form.visible = !form.visible;
            None
        }
        Button::Cancel => Some(Panel::List { scroll: 0 }),
        Button::Delete => {
            let line = match s.waypoints.as_mut()?.remove(index) {
                Ok(Some(w)) => format!("Removed {}", w.name),
                Ok(None) => return Some(Panel::List { scroll: 0 }),
                Err(e) => format!("Removed, but the list couldn't be saved: {e}"),
            };
            s.toast = Some((line, Instant::now()));
            Some(Panel::List { scroll: 0 })
        }
        _ if applied.is_err() => {
            form.error = applied.err();
            None
        }
        Button::Save => {
            if let Err(e) = s.waypoints.as_mut()?.edit(index, |w| *w = edited) {
                s.toast = Some((format!("Couldn't save: {e}"), Instant::now()));
            }
            Some(Panel::List { scroll: 0 })
        }
        Button::Share => {
            match share::format(&edited) {
                Some(line) => {
                    s.outbox.push(Outgoing::Chat(line));
                    s.toast = Some(("Shared in chat".to_owned(), Instant::now()));
                }
                None => form.error = Some("Too long to share in chat"),
            }
            None
        }
        Button::Teleport => {
            s.outbox.push(Outgoing::Teleport {
                at: [edited.x, edited.y, edited.z],
                dimension: edited.dimension,
            });
            s.outbox.push(Outgoing::Close);
            Some(Panel::None)
        }
    }
}

/// The block a waypoint placed at the pointer stands on: on what the map shows there, else at the
/// player's height.
fn map_block(s: &State, mouse: [f32; 2], player_y: Option<f64>) -> Option<[i32; 3]> {
    let shown = s.shown.filter(|sh| sh.fullscreen)?;
    let session = s.map_session()?;
    let img = [shown.view.width as f32, shown.view.height as f32];
    let (u, v) = shown.view.map_uv(
        f64::from(mouse[0] * img[0] / shown.gui[0]),
        f64::from(mouse[1] * img[1] / shown.gui[1]),
    );
    Some(match session.block_at(u, v, shown.view.layer) {
        Some([x, y, z]) => [x, y + 1, z],
        None => {
            let y = player_y?.floor();
            let (x, z) = session.projection().world_at(u, v, y);
            [x.floor() as i32, y as i32, z.floor() as i32]
        }
    })
}

/// A right click on the fullscreen map: on a pin it removes that waypoint; anywhere else it adds
/// one on the block the map shows there. The line to tell the player.
fn right_click(
    game: &Game<'_>,
    s: &mut State,
    mouse: [f32; 2],
    player_y: Option<f64>,
) -> Option<String> {
    if let Some(index) = pin_under(game, s, mouse) {
        return Some(match s.waypoints.as_mut()?.remove(index) {
            Ok(Some(w)) => format!("Removed {}", w.name),
            Ok(None) => return None,
            Err(e) => {
                log::warn!("Diorama: saving waypoints: {e}");
                format!("Removed, but the list couldn't be saved: {e}")
            }
        });
    }
    let at = map_block(s, mouse, player_y)?;
    let w = s.waypoints.as_ref()?.next_at(at, s.map_dimension());
    Some(s.add_waypoint(w))
}

pub(super) fn screen_release(
    game: &Game<'_>,
    screen: &NativeScreenView<'_>,
    button: i32,
    canceled: &mut bool,
) {
    if !own_screen(game, screen) {
        return;
    }
    if button == 0 {
        with_state_if(|s| {
            s.map.dragging = false;
            // A click on a pin, not a drag, opens its editor.
            if let Some((_, false)) = s.press.take()
                && matches!(s.panel, Panel::None)
                && let Some(i) = pin_under(game, s, screen.mouse)
                && let Some(p) = editor(s, i)
            {
                s.panel = p;
            }
        });
        *canceled = true;
    }
}

pub(super) fn screen_drag(
    game: &Game<'_>,
    screen: &NativeScreenView<'_>,
    delta: [f32; 2],
    canceled: &mut bool,
) {
    if !own_screen(game, screen) {
        return;
    }
    with_state_if(|s| {
        if s.map.dragging && matches!(s.panel, Panel::None) {
            if let Some((from, moved)) = &mut s.press {
                let (dx, dy) = (screen.mouse[0] - from[0], screen.mouse[1] - from[1]);
                *moved |= dx * dx + dy * dy > 9.0;
            }
            s.map.follow = false;
            s.map.center.0 -= f64::from(delta[0]) / s.map.zoom;
            s.map.center.1 -= f64::from(delta[1]) / s.map.zoom;
        }
    });
    *canceled = true;
}

pub(super) fn screen_scroll(
    game: &Game<'_>,
    screen: &NativeScreenView<'_>,
    scroll: [f64; 2],
    canceled: &mut bool,
) {
    if !own_screen(game, screen) {
        return;
    }
    let Some((_, gui_size, _)) = gui(game) else {
        return;
    };
    *canceled = true;
    with_state_if(|s| match &mut s.panel {
        Panel::List { scroll: row } => {
            let step = scroll[1].round() as i64;
            *row = (*row as i64 - step).max(0) as usize;
        }
        Panel::Edit(..) => {}
        Panel::None => {
            let before = s.map.zoom;
            let after = (before * 1.25f64.powf(scroll[1])).clamp(MAP_MIN_ZOOM, MAP_MAX_ZOOM);
            // Keep the point under the pointer where it is.
            let (mx, my) = (
                f64::from(screen.mouse[0] - gui_size[0] / 2.0),
                f64::from(screen.mouse[1] - gui_size[1] / 2.0),
            );
            s.map.center.0 += mx / before - mx / after;
            s.map.center.1 += my / before - my / after;
            s.map.zoom = after;
            if mx.abs() > 1.0 || my.abs() > 1.0 {
                s.map.follow = false;
            }
        }
    });
}

pub(super) fn screen_key(
    game: &Game<'_>,
    screen: &NativeScreenView<'_>,
    key: &str,
    canceled: &mut bool,
) {
    if !own_screen(game, screen) {
        return;
    }
    let player_y = game.native_player().map_or(64.0, |p| p.y);
    let list_key = game.native_mod_key(ID, "waypoints", "key.keyboard.u");
    let handled = with_state_if(|s| {
        let mut panel = std::mem::take(&mut s.panel);
        let handled = match &mut panel {
            // The editor takes every key, so typing never reaches the map or closes the screen.
            Panel::Edit(form, created) => {
                match key {
                    "key.keyboard.backspace" => form.backspace(),
                    "key.keyboard.tab" => form.tab(false),
                    "key.keyboard.up" => form.tab(true),
                    "key.keyboard.down" => form.tab(false),
                    "key.keyboard.enter" | "key.keyboard.keypad.enter" => {
                        if let Some(next) = editor_button(s, form, *created, Button::Save) {
                            panel = next;
                        }
                    }
                    "key.keyboard.escape" => panel = Panel::List { scroll: 0 },
                    _ => {}
                }
                true
            }
            Panel::List { scroll } => match key {
                "key.keyboard.escape" => {
                    panel = Panel::None;
                    true
                }
                "key.keyboard.up" => {
                    *scroll = scroll.saturating_sub(1);
                    true
                }
                "key.keyboard.down" => {
                    *scroll += 1;
                    true
                }
                k if k == list_key => {
                    panel = Panel::None;
                    true
                }
                _ => false,
            },
            Panel::None => match key {
                "key.keyboard.c" => {
                    s.map.caves = !s.map.caves;
                    true
                }
                "key.keyboard.space" => {
                    s.map.follow = true;
                    if s.viewing.take().is_some() {
                        s.shown = None;
                    }
                    true
                }
                "key.keyboard.page.up" | "key.keyboard.page.down" => {
                    let step = if key == "key.keyboard.page.up" { 4 } else { -4 };
                    let layer = s
                        .map
                        .layer
                        .unwrap_or_else(|| session::cave_layer_for(player_y))
                        + step;
                    s.map.layer = Some(layer);
                    s.map.caves = true;
                    true
                }
                k if k == list_key => {
                    panel = Panel::List { scroll: 0 };
                    true
                }
                _ => false,
            },
        };
        s.panel = panel;
        handled
    });
    if handled == Some(true) {
        *canceled = true;
    }
}

/// A character typed on the fullscreen map: into the editor's field.
pub(super) fn screen_char(
    game: &Game<'_>,
    screen: &NativeScreenView<'_>,
    c: char,
    canceled: &mut bool,
) {
    if !own_screen(game, screen) {
        return;
    }
    with_state_if(|s| {
        if let Panel::Edit(form, _) = &mut s.panel {
            form.type_char(c);
            *canceled = true;
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Chat: shared waypoints and the commands their links run

pub(super) fn commands(commands: &mut Vec<&'static str>) {
    commands.push("diorama");
}

/// `/diorama waypoint add ...` (the [Add] link's) and `/diorama waypoint list`.
pub(super) fn command(game: &mut Game<'_>, command: &str, ran: &mut bool) {
    let command = command.trim();
    if command != "diorama" && !command.starts_with("diorama ") {
        return;
    }
    *ran = true;
    let player_y = game.native_player().map_or(64, |p| p.y.floor() as i32);
    let line = if command == "diorama waypoint list" {
        with_state(game, |s| {
            s.panel = Panel::List { scroll: 0 };
            s.open_map = true;
        });
        None
    } else if let Some(shared) = share::parse_add_command(command) {
        with_state(game, |s| Some(s.add_waypoint(shared.waypoint(player_y))))
    } else {
        Some(
            "Usage: /diorama waypoint list, or /diorama waypoint add <x> <y> <z> <colour 0-15> \"<dimension>\" \"<letters>\" <name>"
                .to_owned(),
        )
    };
    if let Some(line) = line {
        game.native_chat(&format!("Diorama: {line}"));
    }
    open_map_if_asked(game);
}

/// A chat line with a shared waypoint in it gets an [Add] link.
pub(super) fn chat_received(game: &mut Game<'_>, message: &mut Text) {
    if !game.native_on(ID) {
        return;
    }
    let Some(shared) = share::parse(&message.plain()) else {
        return;
    };
    let y = shared
        .y
        .unwrap_or_else(|| game.native_player().map_or(64, |p| p.y.floor() as i32));
    let hover = format!(
        "Add {} at {} {} {} ({})",
        shared.name,
        shared.x,
        y,
        shared.z,
        dimension_name(&shared.dimension)
    );
    let style = Style {
        color: Some(0x55FF55),
        click: Some(ClickEvent::RunCommand(format!(
            "/{}",
            share::add_command(&shared, y)
        ))),
        hover: Some(Box::new(Text::literal(hover))),
        ..Style::default()
    };
    message.push(Text::literal(" "));
    message.push(Text::styled("[Add]", style));
}

// ---------------------------------------------------------------------------------------------
// Leaving

/// The client dropped a level: the map of that dimension is saved and closed.
pub(super) fn level_unload(_game: &mut Game<'_>, _dimension: &str) {
    with_state_if(State::end_session);
}

pub(super) fn disconnecting(_game: &Game<'_>) {
    with_state_if(State::end_session);
}

/// Resource packs changed: block colours are built again on the next tick.
pub(super) fn resources_reloaded(_game: &mut Game<'_>) {
    with_state_if(|s| s.looks_generation = 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let mut s = Settings::read(|_| None);
        assert_eq!(s, Settings::default());
        for i in 0..22 {
            s.click(i);
        }
        let stored = s.stored();
        let back = Settings::read(|k| {
            stored
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        });
        assert_eq!(back, s);
        assert_eq!(s.labels().len(), 22);
        assert_eq!(s.stored().len(), 22);
        // Every choice of each new setting comes back as it went.
        for _ in 0..8 {
            for i in 16..22 {
                s.click(i);
            }
            let stored = s.stored();
            let back = Settings::read(|k| {
                stored
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.clone())
            });
            assert_eq!(back, s);
        }
        // A saved percentage between steps snaps to the nearest.
        let s = Settings::read(|k| (k == "opacity").then(|| "55".to_owned()));
        assert_eq!(s.opacity, 60);
        assert!((s.opacity() - 0.6).abs() < 1e-6);
    }

    #[test]
    fn clock_formats() {
        assert_eq!(clock_text(ClockFormat::Only24h, 0), "06:00");
        assert_eq!(clock_text(ClockFormat::Day24h, 0), "Day 1, 06:00");
        assert_eq!(clock_text(ClockFormat::Day24h, 18_000), "Day 2, 00:00");
        assert_eq!(clock_text(ClockFormat::Only12h, 18_000), "12:00 AM");
        assert_eq!(clock_text(ClockFormat::Day12h, 6_000), "Day 1, 12:00 PM");
        assert_eq!(clock_text(ClockFormat::Only12h, 13_500), "07:30 PM");
    }
}
