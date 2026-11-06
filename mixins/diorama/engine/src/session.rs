//! The map of one dimension while the player is in it: the chunks the game has loaded, the tiles
//! to bake again as chunks arrive and blocks change, the tiles kept for drawing and the store
//! that remembers them.

use crate::baker::{self, BakeSettings};
use crate::bakery::{BakeContext, Bakery};
use crate::chunk::ChunkSnapshot;
use crate::looks::BlockLooks;
use crate::projection::{Facing, Projection, TILE_UNITS};
use crate::source::{ChunkSource, LiveChunks};
use crate::store::TileStore;
use crate::style::MapStyle;
use crate::tile::TileKey;
use crate::view::{self, Fog, TileCache, View};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Chunk light usually lands a moment after the blocks, so new chunks wait this long.
pub const LOAD_DELAY: Duration = Duration::from_millis(750);
pub const CHANGE_DELAY: Duration = Duration::from_millis(250);
/// The longest a tile waits while chunks keep arriving under it.
const MAX_WAIT: Duration = Duration::from_secs(3);
/// Tiles held for drawing: about 1.3 MiB each with their mips.
const CACHE_TILES: usize = 96;

/// The cave slice for a player standing at `feet_y`: a few blocks above their head.
pub fn cave_layer_for(feet_y: f64) -> i32 {
    (feet_y.floor() as i32 + 5).div_euclid(4) * 4
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Where this dimension's tiles are saved.
    pub folder: PathBuf,
    pub min_y: i32,
    pub max_y: i32,
    pub roof: Option<i32>,
    pub style: MapStyle,
    pub facing: Facing,
    /// One sample per pixel instead of four.
    pub fast: bool,
    pub threads: usize,
}

impl SessionConfig {
    /// Bake threads for this machine: half the cores less one, one to six.
    pub fn default_threads() -> usize {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        (cores / 2).saturating_sub(1).clamp(1, 6)
    }
}

struct Loader {
    requests: Sender<TileKey>,
    results: Receiver<(TileKey, Option<Arc<Vec<u32>>>)>,
}

pub struct MapSession {
    config: SessionConfig,
    live: Arc<LiveChunks>,
    store: Arc<TileStore>,
    bakery: Option<Bakery>,
    looks: Option<Arc<BlockLooks>>,
    projection: Projection,
    cache: TileCache,
    /// Tiles to bake again, and when they were first and are next due.
    dirty: HashMap<TileKey, (Instant, Instant)>,
    /// Cave slices drawn besides the surface.
    cave_layer: Option<i32>,
    loader: Loader,
    /// Tiles asked of the loader and not back yet.
    loading: HashSet<TileKey>,
    /// Tiles the store doesn't have; asked again only after something new could be there.
    absent: HashSet<TileKey>,
    /// Absent tiles baked once on being drawn; again only after a chunk under them changes.
    tried: HashSet<TileKey>,
    /// Overview tiles the store rebuilt, from its writer thread.
    rebuilt: Arc<Mutex<Vec<TileKey>>>,
    /// Overview tiles made here from finer tiles held for drawing, standing in until the
    /// store's own load.
    stand_ins: HashSet<TileKey>,
    generation: u64,
    player: (f64, f64),
}

impl MapSession {
    pub fn new(config: SessionConfig) -> MapSession {
        let store = Arc::new(TileStore::open(config.folder.clone()));
        let rebuilt = Arc::new(Mutex::new(Vec::new()));
        let r = rebuilt.clone();
        store.on_overview(move |k| r.lock().unwrap_or_else(|e| e.into_inner()).push(k));
        let (req_tx, req_rx) = mpsc::channel::<TileKey>();
        let (res_tx, res_rx) = mpsc::channel();
        let s = store.clone();
        std::thread::Builder::new()
            .name("diorama-tile-loader".into())
            .spawn(move || {
                while let Ok(key) = req_rx.recv() {
                    if res_tx.send((key, s.read(key))).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn tile loader");
        let live = Arc::new(LiveChunks::new(config.min_y, config.max_y, config.roof));
        MapSession {
            projection: Projection::new(config.style.pitch_degrees, config.facing),
            config,
            live,
            store,
            bakery: None,
            looks: None,
            cache: TileCache::new(CACHE_TILES),
            dirty: HashMap::new(),
            cave_layer: None,
            loader: Loader {
                requests: req_tx,
                results: res_rx,
            },
            loading: HashSet::new(),
            absent: HashSet::new(),
            tried: HashSet::new(),
            rebuilt,
            stand_ins: HashSet::new(),
            generation: 0,
            player: (0.0, 0.0),
        }
    }

    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    pub fn looks(&self) -> Option<&Arc<BlockLooks>> {
        self.looks.as_ref()
    }

    /// The chunks loaded, as snapshotted.
    pub fn chunks(&self) -> &Arc<LiveChunks> {
        &self.live
    }

    /// Tiles held for drawing; goes up when one is added or replaced.
    pub fn cache_version(&self) -> u64 {
        self.cache.version
    }

    /// Tiles waiting to bake, and baking.
    pub fn busy(&self) -> (usize, usize, usize) {
        let (w, b) = self.bakery.as_ref().map_or((0, 0), Bakery::busy);
        (self.dirty.len(), w, b)
    }

    fn context(&mut self) -> Option<Arc<BakeContext>> {
        let looks = self.looks.clone()?;
        self.generation += 1;
        Some(Arc::new(BakeContext {
            source: self.live.clone() as Arc<dyn ChunkSource>,
            looks,
            settings: BakeSettings {
                style: self.config.style.clone(),
                projection: self.projection,
                fast: self.config.fast,
                fog_of_war: false,
            },
            store: self.store.clone(),
            generation: self.generation,
        }))
    }

    /// The block looks to bake with: built once the game's resources are loaded, and again after
    /// each resource reload. Everything loaded is baked again with them.
    pub fn set_looks(&mut self, looks: Arc<BlockLooks>) {
        self.looks = Some(looks);
        let context = self.context().expect("looks set");
        match &self.bakery {
            Some(b) => b.set_context(context),
            None => self.bakery = Some(Bakery::new(self.config.threads, context)),
        }
        let now = Instant::now();
        for (cx, cz) in self.live.keys() {
            self.dirty_chunk(cx, cz, now, Duration::ZERO);
        }
    }

    /// Bakes with one sample per pixel, or four.
    pub fn set_fast(&mut self, fast: bool) {
        if self.config.fast == fast {
            return;
        }
        self.config.fast = fast;
        if let Some(c) = self.context()
            && let Some(b) = &self.bakery
        {
            b.set_context(c);
        }
    }

    /// The game loaded a chunk, or sent it again.
    pub fn chunk_loaded(&mut self, chunk: Arc<ChunkSnapshot>, now: Instant) {
        let (cx, cz) = (chunk.x, chunk.z);
        self.live.put(chunk);
        self.dirty_chunk(cx, cz, now, LOAD_DELAY);
    }

    /// The game unloaded a chunk. Its tiles stay as saved.
    pub fn chunk_unloaded(&mut self, cx: i32, cz: i32) {
        self.live.remove(cx, cz);
    }

    /// Blocks in a world box changed; `chunk` is their chunk snapshotted again.
    pub fn blocks_changed(
        &mut self,
        chunk: Arc<ChunkSnapshot>,
        box_min: [i32; 3],
        box_max: [i32; 3],
        now: Instant,
    ) {
        self.live.put(chunk);
        for layer in self.layers() {
            if let Some(cut) = (layer != TileKey::SURFACE).then_some(layer)
                && (box_min[1] > cut || box_max[1] < cut - baker::CAVE_DEPTH)
            {
                continue;
            }
            for key in baker::tiles_touched(&self.projection, layer, box_min, box_max) {
                self.mark(key, now, CHANGE_DELAY);
            }
        }
    }

    fn layers(&self) -> Vec<i32> {
        let mut l = vec![TileKey::SURFACE];
        l.extend(self.cave_layer);
        l
    }

    fn dirty_chunk(&mut self, cx: i32, cz: i32, now: Instant, delay: Duration) {
        let Some(chunk) = self.live.chunk(cx, cz) else {
            return;
        };
        let top = if chunk.top_y == i32::MIN {
            self.config.min_y
        } else {
            chunk.top_y
        };
        for layer in self.layers() {
            let (y0, y1) = if layer == TileKey::SURFACE {
                (self.config.min_y, top)
            } else {
                (
                    (layer - baker::CAVE_DEPTH).max(self.config.min_y),
                    layer.min(top),
                )
            };
            if y1 < y0 {
                continue;
            }
            for key in baker::tiles_touched(
                &self.projection,
                layer,
                [cx * 16, y0, cz * 16],
                [cx * 16 + 15, y1, cz * 16 + 15],
            ) {
                self.mark(key, now, delay);
            }
        }
    }

    /// Something under a tile changed: bake it again after `delay`.
    fn mark(&mut self, key: TileKey, now: Instant, delay: Duration) {
        self.absent.remove(&key);
        self.tried.remove(&key);
        self.schedule(key, now, delay);
    }

    fn schedule(&mut self, key: TileKey, now: Instant, delay: Duration) {
        let due = now + delay;
        self.dirty
            .entry(key)
            .and_modify(|(first, d)| *d = (*d).max(due).min(*first + MAX_WAIT))
            .or_insert((now, due));
    }

    /// Shows the cave slice at `layer` besides the surface, or none. A new slice is baked from
    /// every chunk loaded, nearest first.
    pub fn set_cave_layer(&mut self, layer: Option<i32>) {
        if self.cave_layer == layer {
            return;
        }
        self.cave_layer = layer;
        if let Some(b) = &self.bakery {
            // Slices no longer shown needn't bake.
            b.retain(|k| !k.is_cave() || Some(k.layer) == layer);
        }
        self.dirty
            .retain(|k, _| !k.is_cave() || Some(k.layer) == layer);
        if layer.is_some() {
            let now = Instant::now();
            for (cx, cz) in self.live.keys() {
                self.dirty_chunk(cx, cz, now, Duration::ZERO);
            }
        }
    }

    pub fn cave_layer(&self) -> Option<i32> {
        self.cave_layer
    }

    /// Hands due tiles to the bake threads and takes in what they and the loader finished. Call
    /// once a frame or tick with the player's world position.
    pub fn tick(&mut self, now: Instant, player: [f64; 3]) {
        let p = &self.projection;
        self.player = (
            p.u(player[0], player[2]),
            p.v(player[0], player[1], player[2]),
        );
        let units = f64::from(TILE_UNITS);
        let (pu, pv) = (
            (self.player.0 / units).floor() as i64,
            (self.player.1 / units).floor() as i64,
        );
        let rank = move |k: &TileKey| {
            let (du, dv) = (i64::from(k.u) - pu, i64::from(k.v) - pv);
            // Surface first at equal distance: it's what the player sees most.
            (du * du + dv * dv) * 2 + i64::from(k.is_cave())
        };
        if let Some(bakery) = &self.bakery {
            let due: Vec<TileKey> = self
                .dirty
                .iter()
                .filter(|(_, (_, d))| *d <= now)
                .map(|(k, _)| *k)
                .collect();
            if !due.is_empty() {
                for k in &due {
                    self.dirty.remove(k);
                }
                bakery.request(due.into_iter().map(|k| (k, rank(&k))));
            }
            bakery.reprioritize(rank);
            // Tiles from before a change of looks or style are still the best there is until
            // their bake again lands.
            for (key, image, _generation) in bakery.take_done() {
                self.cache.insert(&image);
                self.absent.remove(&key);
                // Stand-ins over it are out of date: made again as they're next drawn.
                let mut up = key;
                while up.lod < TileKey::MAX_LOD {
                    up = up.parent();
                    if self.stand_ins.remove(&up) {
                        self.cache.remove(&up);
                    }
                }
            }
        }
        while let Ok((key, pixels)) = self.loader.results.try_recv() {
            self.loading.remove(&key);
            match pixels {
                // A bake may have come in while it loaded: that one is newer. A stand-in gives way.
                Some(p) if !self.cache.contains(&key) || self.stand_ins.remove(&key) => {
                    self.cache.insert_pixels(key, &p)
                }
                Some(_) => {}
                None => {
                    self.absent.insert(key);
                }
            }
        }
        let rebuilt = std::mem::take(&mut *self.rebuilt.lock().unwrap_or_else(|e| e.into_inner()));
        if !rebuilt.is_empty() {
            let set: HashSet<TileKey> = rebuilt.into_iter().collect();
            for k in &set {
                self.absent.remove(k);
            }
            // Drawn overviews are reloaded as they're next drawn.
            self.cache.remove_where(|k| set.contains(k));
        }
    }

    /// Draws a view into `out` (ABGR pixels, `view.width * view.height`), asking for the tiles it
    /// lacks: from the store, or baked when there are chunks to bake them from.
    pub fn compose(&mut self, view: &View, fog: &Fog, out: &mut [u32]) {
        if view.lod > 0 {
            for key in view.tiles() {
                if !self.cache.contains(&key) {
                    self.stand_in(key, key.lod);
                }
            }
        }
        let missing = view::compose(view, &mut self.cache, fog, out);
        for key in missing {
            if self.loading.contains(&key) {
                continue;
            }
            if self.absent.contains(&key) {
                // Nothing saved: bake a detailed tile once if the game has chunks under it.
                if key.lod == 0
                    && !self.tried.contains(&key)
                    && self.bakery.is_some()
                    && self.has_chunks_under(key)
                {
                    self.tried.insert(key);
                    self.schedule(key, Instant::now(), Duration::ZERO);
                }
                continue;
            }
            if self.loader.requests.send(key).is_ok() {
                self.loading.insert(key);
            }
        }
    }

    /// Makes overview tile `key` from finer tiles held for drawing, down to level 0 (`depth` levels),
    /// if any are; true when it is held now. The store's own is still loaded and replaces it.
    fn stand_in(&mut self, key: TileKey, depth: u8) -> bool {
        if self.cache.contains(&key) {
            return true;
        }
        if key.lod == 0 || depth == 0 {
            return false;
        }
        let children = key.children();
        let mut any = false;
        for c in children {
            any |= self.stand_in(c, depth - 1);
        }
        if !any {
            return false;
        }
        let pixels: Vec<Option<Vec<u32>>> = children
            .iter()
            .map(|c| self.cache.pixels(c).map(<[u32]>::to_vec))
            .collect();
        let refs: [Option<&[u32]>; 4] = std::array::from_fn(|i| pixels[i].as_deref());
        let image = crate::tile::overview(key, refs);
        self.cache.insert(&image);
        self.stand_ins.insert(key);
        true
    }

    /// Whether any loaded chunk shows in a tile.
    fn has_chunks_under(&self, key: TileKey) -> bool {
        let p = &self.projection;
        let (u0, u1) = (key.min_u(), key.min_u() + key.units());
        let (v0, v1) = (key.min_v(), key.min_v() + key.units());
        let (y_lo, y_hi) = if key.is_cave() {
            (
                f64::from(key.layer - baker::CAVE_DEPTH),
                f64::from(key.layer),
            )
        } else {
            (f64::from(self.config.min_y), f64::from(self.live.top_y()))
        };
        // The turned z range the tile's rays cross, then each turned chunk column in it.
        let (z0, z1) = (p.z_at(v0, y_lo), p.z_at(v1, y_hi));
        let (cx0, cx1) = ((u0.floor() as i32) >> 4, (u1.floor() as i32) >> 4);
        let (cz0, cz1) = (
            (z0.min(z1).floor() as i32) >> 4,
            (z0.max(z1).floor() as i32) >> 4,
        );
        let f = p.facing;
        (cx0..=cx1).any(|cx| {
            (cz0..=cz1).any(|cz| {
                self.live
                    .contains(f.world_block_x(cx, cz), f.world_block_z(cx, cz))
            })
        })
    }

    /// The map point of a world position.
    pub fn uv(&self, pos: [f64; 3]) -> (f64, f64) {
        (
            self.projection.u(pos[0], pos[2]),
            self.projection.v(pos[0], pos[1], pos[2]),
        )
    }

    /// The first block a map point's ray meets among the loaded chunks, in world coordinates:
    /// from the top of the dimension down (from under its roof, where it has one), or from a cave
    /// slice's cut. None where it meets nothing loaded.
    pub fn block_at(&self, u: f64, v: f64, layer: i32) -> Option<[i32; 3]> {
        let looks = self.looks.as_ref()?;
        let p = &self.projection;
        let f = p.facing;
        let min_y = self.config.min_y;
        let (top, bottom) = if layer == TileKey::SURFACE {
            (self.config.roof.unwrap_or(self.live.top_y()), min_y)
        } else {
            (layer, (layer - baker::CAVE_DEPTH).max(min_y))
        };
        // Under a roof the ray starts inside it: it counts from the first open cell it finds.
        let mut open = self.config.roof.is_none() || layer != TileKey::SURFACE;
        let tx = u.floor() as i32;
        // The chunk last read, and where it is.
        let (mut at, mut chunk) = ((i32::MIN, i32::MIN), None);
        // Down the ray: z falls as it descends, so in each layer the south cell comes first.
        for y in (bottom..top).rev() {
            let z_in = p.z_at(v, f64::from(y + 1)).floor() as i32;
            let z_out = p.z_at(v, f64::from(y)).floor() as i32;
            for tz in (z_out..=z_in).rev() {
                let (wx, wz) = (f.world_block_x(tx, tz), f.world_block_z(tx, tz));
                if at != (wx >> 4, wz >> 4) {
                    at = (wx >> 4, wz >> 4);
                    chunk = self.live.chunk(at.0, at.1);
                }
                let Some(c) = chunk.as_ref() else {
                    continue;
                };
                let air =
                    looks.kind(c.state(wx & 15, y, wz & 15, looks.air)) == crate::looks::Kind::Air;
                if !open {
                    open = air;
                } else if !air {
                    return Some([wx, y, wz]);
                }
            }
        }
        None
    }

    /// Waits until every bake handed to the store is on disk.
    pub fn flush(&self) {
        self.store.flush();
    }
}

impl Drop for MapSession {
    fn drop(&mut self) {
        // Bake threads first, so nothing reaches the store after its last flush.
        self.bakery = None;
        self.store.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::looks::tests::test_looks;
    use crate::view::lod_for;

    #[test]
    fn bakes_loaded_chunks_and_draws_them() {
        let dir = std::env::temp_dir().join(format!("diorama-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let world = crate::baker::tests::flat_world(2, false);
        let mut s = MapSession::new(SessionConfig {
            folder: dir.clone(),
            min_y: -64,
            max_y: 320,
            roof: None,
            style: MapStyle::DEFAULT,
            facing: Facing::North,
            fast: true,
            threads: 2,
        });
        s.set_looks(Arc::new(test_looks()));
        let start = Instant::now();
        for (cx, cz) in world.keys() {
            s.chunk_loaded(world.chunk(cx, cz).unwrap(), start);
        }
        let player = [8.0, 64.0, 8.0];
        let (u, v) = s.uv(player);
        let view = View {
            center_u: u,
            center_v: v,
            zoom: 2.0,
            layer: TileKey::SURFACE,
            lod: lod_for(2.0, 2.0, false),
            width: 96,
            height: 96,
            round: false,
        };
        let mut out = vec![0u32; 96 * 96];
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            s.tick(Instant::now() + LOAD_DELAY, player);
            s.compose(&view, &Fog::default(), &mut out);
            let (d, w, b) = s.busy();
            if d + w + b == 0 && s.cache_version() > 0 {
                break;
            }
            assert!(Instant::now() < deadline, "bakes finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        s.compose(&view, &Fog::default(), &mut out);
        let centre = out[48 * 96 + 48];
        assert_eq!(centre >> 24, 255);
        let (r, g, b) = (centre & 0xFF, (centre >> 8) & 0xFF, (centre >> 16) & 0xFF);
        assert!(g > r && g > b, "grass under the player: {r} {g} {b}");
        drop(s);
        assert!(
            crate::store::path(&dir, TileKey::new(TileKey::SURFACE, 0, -1)).exists()
                || std::fs::read_dir(dir.join("surface/0")).is_ok_and(|mut d| d.next().is_some()),
            "tiles saved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zoomed_out_views_stand_in_for_overviews() {
        let dir = std::env::temp_dir().join(format!("diorama-standin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = MapSession::new(SessionConfig {
            folder: dir.clone(),
            min_y: -64,
            max_y: 320,
            roof: None,
            style: MapStyle::DEFAULT,
            facing: Facing::North,
            fast: true,
            threads: 1,
        });
        let key = TileKey::new(TileKey::SURFACE, 0, 0);
        let mut img = crate::tile::TileImage::blank(key);
        img.pixels.fill(0xFF00_FF00);
        s.cache.insert(&img);
        let zoom = 0.25;
        let view = View {
            center_u: 64.0,
            center_v: 64.0,
            zoom,
            layer: TileKey::SURFACE,
            lod: lod_for(zoom, zoom, false),
            width: 16,
            height: 16,
            round: false,
        };
        assert!(view.lod >= 1);
        let mut out = vec![0u32; 256];
        s.compose(&view, &Fog::default(), &mut out);
        assert_eq!(out[8 * 16 + 8], 0xFF00_FF00, "drawn from the level-0 tile");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rays_find_the_ground_and_tower_tops() {
        let dir = std::env::temp_dir().join(format!("diorama-blockat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let world = crate::baker::tests::flat_world(1, true);
        for facing in Facing::ALL {
            let mut s = MapSession::new(SessionConfig {
                folder: dir.clone(),
                min_y: -64,
                max_y: 320,
                roof: None,
                style: MapStyle::DEFAULT,
                facing,
                fast: true,
                threads: 1,
            });
            s.set_looks(Arc::new(test_looks()));
            let now = Instant::now();
            for (cx, cz) in world.keys() {
                s.chunk_loaded(world.chunk(cx, cz).unwrap(), now);
            }
            // The grass top at a block's middle maps back onto that block.
            let (u, v) = s.uv([-5.5, 64.0, 3.5]);
            assert_eq!(
                s.block_at(u, v, TileKey::SURFACE),
                Some([-6, 63, 3]),
                "{facing:?}"
            );
            // The tower's top hides the ground behind it.
            let (u, v) = s.uv([10.5, 80.0, 10.5]);
            assert_eq!(
                s.block_at(u, v, TileKey::SURFACE),
                Some([10, 79, 10]),
                "{facing:?}"
            );
            // Nothing loaded far away.
            let (u, v) = s.uv([500.0, 64.0, 500.0]);
            assert_eq!(s.block_at(u, v, TileKey::SURFACE), None);
            // A cave slice cut at y 40 starts in the stone.
            let (u, v) = s.uv([0.5, 40.0, 0.5]);
            assert_eq!(s.block_at(u, v, 40), Some([0, 39, 0]), "{facing:?}");
            drop(s);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cave_layers_step_by_four() {
        assert_eq!(cave_layer_for(64.0), 68);
        assert_eq!(cave_layer_for(-10.5), -8);
        assert_eq!(cave_layer_for(-12.0), -8);
        assert_eq!(cave_layer_for(-9.0), -4);
    }
}
