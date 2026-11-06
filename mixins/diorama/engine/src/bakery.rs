//! Bake threads: tiles asked for are baked nearest the player first, merged over what was saved,
//! handed to the store and kept for the session to pick up.

use crate::baker::{BakeSettings, Baker};
use crate::looks::BlockLooks;
use crate::source::ChunkSource;
use crate::store::TileStore;
use crate::tile::{TileImage, TileKey};
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

struct Queue {
    /// Waiting tiles and their priority (lower first).
    waiting: HashMap<TileKey, i64>,
    /// Tiles being baked, and whether they were asked for again meanwhile.
    baking: HashMap<TileKey, bool>,
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

    /// Asks for tiles, with their priority (lower is sooner). A tile already waiting keeps the
    /// sooner of its priorities; one being baked is baked again after.
    pub fn request(&self, tiles: impl IntoIterator<Item = (TileKey, i64)>) {
        let mut q = self.lock();
        let mut any = false;
        for (key, priority) in tiles {
            if let Some(again) = q.baking.get_mut(&key) {
                *again = true;
                continue;
            }
            let p = q.waiting.entry(key).or_insert(priority);
            *p = (*p).min(priority);
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
        for (k, p) in q.waiting.iter_mut() {
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
        let (key, context) = {
            let mut q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if q.stop {
                    return;
                }
                if let Some((&key, _)) =
                    q.waiting.iter().min_by_key(|(k, p)| (**p, k.lod, k.v, k.u))
                {
                    q.waiting.remove(&key);
                    q.baking.insert(key, false);
                    break (key, q.context.clone());
                }
                q = queue.1.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let mut image = Baker::bake(
            key,
            context.source.as_ref(),
            &context.looks,
            &context.settings,
        );
        if let Some(saved) = context.store.read(key) {
            image.merge_under(&saved);
        }
        let known = image.any_known();
        if known {
            context.store.put(&image);
        }
        let mut q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
        if q.baking.remove(&key) == Some(true) {
            q.waiting.entry(key).or_insert(i64::MIN / 2);
            queue.1.notify_one();
        }
        if known {
            q.done.push((key, Arc::new(image), context.generation));
        }
    }
}
