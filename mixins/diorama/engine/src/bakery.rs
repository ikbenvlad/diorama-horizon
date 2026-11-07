//! Bake threads: tiles asked for are baked nearest the player first, merged over what was saved,
//! handed to the store and kept for the session to pick up.

use crate::baker::{BakeSettings, Baker};
use crate::looks::BlockLooks;
use crate::source::ChunkSource;
use crate::store::TileStore;
use crate::tile::{Patches, TileImage, TileKey};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Everything a bake reads. Replaced whole when the looks or the style change.
pub struct BakeContext {
    pub source: Arc<dyn ChunkSource>,
    pub looks: Arc<BlockLooks>,
    pub settings: BakeSettings,
    pub store: Arc<TileStore>,
    /// Bumped with each new context; results from an older one are dropped.
    pub generation: u64,
}

/// Past this many patches a tile is baked whole: one baker over the tile costs less than many
/// small ones.
const MOST_PATCHES: u32 = 160;

struct Queue {
    /// Waiting tiles, their priority (lower first) and the patches to bake.
    waiting: HashMap<TileKey, (i64, Patches)>,
    /// Tiles being baked, and the patches asked for again meanwhile.
    baking: HashMap<TileKey, Patches>,
    context: Arc<BakeContext>,
    done: Vec<(TileKey, Arc<TileImage>, u64)>,
    stop: bool,
}

pub struct Bakery {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    workers: Vec<JoinHandle<()>>,
}

impl Bakery {
    pub fn new(threads: usize, context: Arc<BakeContext>) -> Bakery {
        let queue = Arc::new((
            Mutex::new(Queue {
                waiting: HashMap::new(),
                baking: HashMap::new(),
                context,
                done: Vec::new(),
                stop: false,
            }),
            Condvar::new(),
        ));
        let workers = (0..threads.max(1))
            .map(|i| {
                let q = queue.clone();
                std::thread::Builder::new()
                    .name(format!("diorama-bake-{i}"))
                    .spawn(move || work(&q))
                    .expect("spawn bake thread")
            })
            .collect();
        Bakery { queue, workers }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asks for tiles, with their priority (lower is sooner) and the patches of each to bake. A
    /// tile already waiting keeps the sooner of its priorities and bakes both sets; one being
    /// baked is baked again after.
    pub fn request(&self, tiles: impl IntoIterator<Item = (TileKey, i64, Patches)>) {
        let mut q = self.lock();
        let mut any = false;
        for (key, priority, patches) in tiles {
            if let Some(again) = q.baking.get_mut(&key) {
                *again = again.union(patches);
                continue;
            }
            let w = q.waiting.entry(key).or_insert((priority, Patches::NONE));
            w.0 = w.0.min(priority);
            w.1 = w.1.union(patches);
            any = true;
        }
        drop(q);
        if any {
            self.queue.1.notify_all();
        }
    }

    /// Re-ranks the waiting tiles.
    pub fn reprioritize(&self, rank: impl Fn(&TileKey) -> i64) {
        let mut q = self.lock();
        for (k, (p, _)) in q.waiting.iter_mut() {
            *p = rank(k);
        }
    }

    /// Drops waiting tiles the filter rejects (too far away now, say).
    pub fn retain(&self, keep: impl Fn(&TileKey) -> bool) {
        self.lock().waiting.retain(|k, _| keep(k));
    }

    pub fn set_context(&self, context: Arc<BakeContext>) {
        let mut q = self.lock();
        q.context = context;
    }

    pub fn context(&self) -> Arc<BakeContext> {
        self.lock().context.clone()
    }

    /// Finished tiles since the last call, with the generation of the context they were baked in.
    pub fn take_done(&self) -> Vec<(TileKey, Arc<TileImage>, u64)> {
        std::mem::take(&mut self.lock().done)
    }

    /// Tiles waiting and being baked.
    pub fn busy(&self) -> (usize, usize) {
        let q = self.lock();
        (q.waiting.len(), q.baking.len())
    }

    pub fn is_waiting(&self, key: &TileKey) -> bool {
        let q = self.lock();
        q.waiting.contains_key(key) || q.baking.contains_key(key)
    }

    /// The tiles waiting, for tests and debugging.
    pub fn waiting(&self) -> HashSet<TileKey> {
        self.lock().waiting.keys().copied().collect()
    }
}

impl Drop for Bakery {
    fn drop(&mut self) {
        self.lock().stop = true;
        self.queue.1.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn work(queue: &(Mutex<Queue>, Condvar)) {
    loop {
        let (key, patches, context) = {
            let mut q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if q.stop {
                    return;
                }
                if let Some((&key, _)) = q
                    .waiting
                    .iter()
                    .min_by_key(|(k, (p, _))| (*p, k.lod, k.v, k.u))
                {
                    let (_, patches) = q.waiting.remove(&key).expect("just found");
                    q.baking.insert(key, Patches::NONE);
                    break (key, patches, q.context.clone());
                }
                q = queue.1.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let saved = context.store.read(key);
        let image = match saved {
            // Part of a tile there is an image of: those patches over it.
            Some(saved) if !patches.is_all() && patches.count() <= MOST_PATCHES => {
                let mut pixels = saved.to_vec();
                Baker::bake_into(
                    key,
                    context.source.as_ref(),
                    &context.looks,
                    &context.settings,
                    &patches,
                    &mut pixels,
                );
                TileImage { key, pixels }
            }
            saved => {
                let mut image = Baker::bake(
                    key,
                    context.source.as_ref(),
                    &context.looks,
                    &context.settings,
                );
                if let Some(saved) = saved {
                    image.merge_under(&saved);
                }
                image
            }
        };
        let known = image.any_known();
        if known {
            context.store.put(&image);
        }
        let mut q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(again) = q.baking.remove(&key)
            && !again.is_empty()
        {
            let w = q
                .waiting
                .entry(key)
                .or_insert((i64::MIN / 2, Patches::NONE));
            w.1 = w.1.union(again);
            queue.1.notify_one();
        }
        if known {
            q.done.push((key, Arc::new(image), context.generation));
        }
    }
}
