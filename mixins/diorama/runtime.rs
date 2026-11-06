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
use diorama_engine::session::{self, MapSession, SessionConfig};
use diorama_engine::source::ChunkSource;
use diorama_engine::style::MapStyle;
use diorama_engine::tile::TileKey;
use diorama_engine::view::{self, Fog, View};
use diorama_engine::waypoint::{self, Waypoint, Waypoints};
use horizon_client::extras::Corner;
use horizon_client::extras_hud::Corners;
use horizon_client::hud::HudBatch;
use horizon_client::mod_api::{NativeMod, NativeScreenView};
use horizon_world::registry::BlockStateId;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
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
        ]
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
        }
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
    let player = game.native_player();
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
        if key == map {
            if let (Some(p), Some(session)) = (player, s.session.as_ref()) {
                s.map.center = session.uv([p.x, p.y, p.z]);
            }
            s.map.follow = true;
            s.map.dragging = false;
            s.open_map = true;
            None
        } else if key == zoom_in || key == zoom_out {
            let step = s.settings.zoom as i32 + if key == zoom_in { 1 } else { -1 };
            s.settings.zoom = step.clamp(0, ZOOMS.len() as i32 - 1) as usize;
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
    let open = with_state(game, |s| std::mem::take(&mut s.open_map));
    if open && !game.native_mod_screen_open(ID) {
        log::debug!("Diorama: the map can't open over this screen");
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
    let death = with_state(game, |s| {
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
            let folder: PathBuf = data.join("maps").join(&key).join(format!(
                "{}-{:08x}",
                s.settings.facing.id(),
                style.fingerprint()
            ));
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
        let session = s.session.as_mut()?;
        session.tick(now, [player.x, player.y, player.z]);
        let here = session.uv([player.x, player.y, player.z]);
        let cave = session.cave_layer();
        let view = if fullscreen {
            if s.map.follow {
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
        s.pixels.resize(view.width * view.height, 0);
        session.compose(&view, &fog, &mut s.pixels);
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

/// The minimap in its corner of the HUD.
pub(super) fn hud(game: &mut Game<'_>, batch: &mut HudBatch, corners: &mut Corners) {
    if !game.native_on(ID) || game.native_mod_screen() == Some(ID) {
        return;
    }
    let Some(player) = game.native_player() else {
        return;
    };
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
    let text_h = if settings.coordinates { 10.0 } else { 0.0 };
    let (x, y) = corners.place(corner, size, size + text_h);
    let map_y = if corner.top() { y } else { y + text_h };
    if !settings.round {
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
    for (turn, (dx, dy)) in [(0.0, -1.0), (1.0, 0.0), (0.0, 1.0), (-1.0, 0.0)]
        .into_iter()
        .enumerate()
    {
        let text = letter(projection.facing.next(turn as i32));
        let tw = game.native_text_width(text);
        let at = [mx + dx * half - tw / 2.0, my + dy * half - 4.0];
        let rgb = if turn == 0 { 0xFF_5555 } else { 0xE0_E0E0 };
        game.native_text(batch, text, at, rgb, true);
    }
    // Waypoints: where they are, or held at the edge toward them.
    let per_px = img[0] / size;
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
    if settings.coordinates {
        let text = format!(
            "{} {} {}{}",
            player.x.floor() as i64,
            player.y.floor() as i64,
            player.z.floor() as i64,
            if underground { " (cave)" } else { "" }
        );
        let w = game.native_text_width(&text);
        let ty = if corner.top() { map_y + size + 2.0 } else { y };
        game.native_text(batch, &text, [x + (size - w) / 2.0, ty], 0xFFFFFF, true);
    }
}

/// The fullscreen map, drawn over its screen.
pub(super) fn screen_draw(game: &Game<'_>, batch: &mut HudBatch, screen: &NativeScreenView<'_>) {
    if screen.id != "mod" || game.native_mod_screen() != Some(ID) {
        return;
    }
    let (w, h) = (batch.width, batch.height);
    game.native_fill(batch, [0.0, 0.0, w, h], 0xFF0E_1014);
    let state = with_state_if(|s| {
        let shown = s.shown.filter(|sh| sh.fullscreen);
        let session = s.session.as_ref();
        // The block under the pointer, as the map shows it.
        let under = shown.zip(session).and_then(|(sh, se)| {
            let img = [sh.view.width as f32, sh.view.height as f32];
            let (u, v) = sh.view.map_uv(
                f64::from(screen.mouse[0] * img[0] / sh.gui[0]),
                f64::from(screen.mouse[1] * img[1] / sh.gui[1]),
            );
            se.block_at(u, v, sh.view.layer)
        });
        (
            shown,
            session.map(|se| *se.projection()),
            s.map,
            s.pins(),
            under,
            s.toast
                .clone()
                .filter(|(_, at)| at.elapsed() < TOAST)
                .map(|(t, _)| t),
        )
    });
    let Some((shown, projection, map, pins, under, toast)) = state else {
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
    let player = game.native_player();
    // Waypoints, and the name of the one under the pointer.
    if let Some(proj) = projection {
        let placed = screen_pins(&shown, &proj, pins);
        let hovered = placed
            .iter()
            .rposition(|(_, wp, at)| pin_hit(game, wp, *at, screen.mouse));
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
    if let (Some(p), Some(proj)) = (player.as_ref(), projection) {
        let at = to_gui(proj.u(p.x, p.z), proj.v(p.x, p.y, p.z));
        arrow(game, batch, at, p.yaw, proj.facing, 10.0);
    }
    // What's under the pointer: the block the map shows there, else the ground at the player's height.
    let mut lines = vec![if shown.view.layer == TileKey::SURFACE {
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
    if let Some(toast) = toast {
        let tw = game.native_text_width(&toast);
        let at = [(w - tw) / 2.0, 24.0];
        game.native_fill(
            batch,
            [at[0] - 4.0, at[1] - 3.0, tw + 8.0, 14.0],
            0xA000_0000,
        );
        game.native_text(batch, &toast, at, 0xFFFFFF, false);
    }
    let help = "Drag to move · scroll to zoom · right-click to add or remove a waypoint · C caves · PgUp/PgDn slice · Space back to you";
    game.native_text(
        batch,
        help,
        [(w - game.native_text_width(help)) / 2.0, h - 12.0],
        0xC8C8C8,
        true,
    );
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

fn own_screen(game: &Game<'_>, screen: &NativeScreenView<'_>) -> bool {
    screen.id == "mod" && game.native_mod_screen() == Some(ID)
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
    match button {
        0 => {
            with_state_if(|s| s.map.dragging = true);
            *canceled = true;
        }
        1 => {
            let player_y = game.native_player().map(|p| p.y);
            with_state_if(|s| {
                let line = right_click(game, s, screen.mouse, player_y)?;
                s.toast = Some((line, Instant::now()));
                Some(())
            });
            *canceled = true;
        }
        _ => {}
    }
}

/// A right click on the fullscreen map: on a pin it removes that waypoint; anywhere else it adds
/// one on the block the map shows there. The line to tell the player.
fn right_click(
    game: &Game<'_>,
    s: &mut State,
    mouse: [f32; 2],
    player_y: Option<f64>,
) -> Option<String> {
    let shown = s.shown.filter(|sh| sh.fullscreen)?;
    let projection = *s.session.as_ref()?.projection();
    let placed = screen_pins(&shown, &projection, s.pins());
    if let Some((index, _, _)) = placed
        .iter()
        .rev()
        .find(|(_, w, at)| pin_hit(game, w, *at, mouse))
    {
        return Some(match s.waypoints.as_mut()?.remove(*index) {
            Ok(Some(w)) => format!("Removed {}", w.name),
            Ok(None) => return None,
            Err(e) => {
                log::warn!("Diorama: saving waypoints: {e}");
                format!("Removed, but the list couldn't be saved: {e}")
            }
        });
    }
    let img = [shown.view.width as f32, shown.view.height as f32];
    let (u, v) = shown.view.map_uv(
        f64::from(mouse[0] * img[0] / shown.gui[0]),
        f64::from(mouse[1] * img[1] / shown.gui[1]),
    );
    let at = match s.session.as_ref()?.block_at(u, v, shown.view.layer) {
        Some([x, y, z]) => [x, y + 1, z],
        None => {
            // Nothing loaded there: the ground at the player's height.
            let y = player_y?.floor();
            let (x, z) = projection.world_at(u, v, y);
            [x.floor() as i32, y as i32, z.floor() as i32]
        }
    };
    let w = s.waypoints.as_ref()?.next_at(at, &s.dimension);
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
        with_state_if(|s| s.map.dragging = false);
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
        if s.map.dragging {
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
    with_state_if(|s| {
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
    });
    *canceled = true;
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
    let handled = with_state_if(|s| match key {
        "key.keyboard.c" => {
            s.map.caves = !s.map.caves;
            true
        }
        "key.keyboard.space" => {
            s.map.follow = true;
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
        _ => false,
    });
    if handled == Some(true) {
        *canceled = true;
    }
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
        for i in 0..10 {
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
        assert_eq!(s.labels().len(), 10);
    }
}
