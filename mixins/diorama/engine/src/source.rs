//! Where bake threads read chunk snapshots from.

use crate::chunk::ChunkSnapshot;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Chunks in world coordinates. Must be safe to call from any thread.
pub trait ChunkSource: Send + Sync {
    fn chunk(&self, cx: i32, cz: i32) -> Option<Arc<ChunkSnapshot>>;
    /// Lowest block y in this dimension.
    fn min_y(&self) -> i32;
    /// One above the highest block y in this dimension.
    fn max_y(&self) -> i32;
    /// One above the highest non-air block seen so far.
    fn top_y(&self) -> i32;
    /// In a dimension with a roof (the Nether), the y just above the space under it. The sky view
    /// of such a dimension looks down from under the roof.
    fn roof(&self) -> Option<i32> {
        None
    }
    /// Whether the player has seen this block, for cave fog of war. World coordinates.
    fn is_seen(&self, _x: i32, _y: i32, _z: i32) -> bool {
        true
    }
}

/// The chunks the game has loaded, as last snapshotted, shared by every bake.
#[derive(Debug)]
pub struct LiveChunks {
    chunks: RwLock<HashMap<(i32, i32), Arc<ChunkSnapshot>>>,
    min_y: i32,
    max_y: i32,
    roof: Option<i32>,
    top: std::sync::atomic::AtomicI32,
}

impl LiveChunks {
    pub fn new(min_y: i32, max_y: i32, roof: Option<i32>) -> LiveChunks {
        LiveChunks {
            chunks: RwLock::new(HashMap::new()),
            min_y,
            max_y,
            roof,
            top: std::sync::atomic::AtomicI32::new(min_y),
        }
    }

    pub fn put(&self, chunk: Arc<ChunkSnapshot>) {
        if chunk.top_y != i32::MIN {
            self.top.fetch_max(
                chunk.top_y.min(self.max_y),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        self.chunks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert((chunk.x, chunk.z), chunk);
    }

    pub fn remove(&self, cx: i32, cz: i32) -> Option<Arc<ChunkSnapshot>> {
        self.chunks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(cx, cz))
    }

    pub fn len(&self) -> usize {
        self.chunks.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, cx: i32, cz: i32) -> bool {
        self.chunks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&(cx, cz))
    }

    /// The loaded chunks' coordinates.
    pub fn keys(&self) -> Vec<(i32, i32)> {
        self.chunks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect()
    }
}

impl ChunkSource for LiveChunks {
    fn chunk(&self, cx: i32, cz: i32) -> Option<Arc<ChunkSnapshot>> {
        self.chunks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(cx, cz))
            .cloned()
    }

    fn min_y(&self) -> i32 {
        self.min_y
    }

    fn max_y(&self) -> i32 {
        self.max_y
    }

    fn top_y(&self) -> i32 {
        self.top.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn roof(&self) -> Option<i32> {
        self.roof
    }
}
