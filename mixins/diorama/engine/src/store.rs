//! Baked tiles saved on disk, one file per tile, so the map remembers places after their chunks
//! unload and between sessions. One thread writes; overview tiles are rebuilt from their children
//! a moment after the first tile under them changes. Closing waits until everything is on disk.

use crate::tile::{self, PIXELS, TileImage, TileKey};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const MAGIC: &[u8; 4] = b"DTL2";
/// Overview tiles are rebuilt this long after the first write under them since their last rebuild.
const OVERVIEW_DELAY: Duration = Duration::from_secs(2);
/// DTL2 flags byte: bit 0 = has light, bit 1 = has spots.
const DTL2_FLAGS_LIGHT: u8 = 0x01;
const DTL2_FLAGS_SPOTS: u8 = 0x02;

enum Job {
    Write(TileKey),
    Flush(Sender<()>),
    Stop,
}

/// Told of each overview tile the writer rebuilt.
type OverviewListener = Box<dyn Fn(TileKey) + Send>;

struct Shared {
    folder: PathBuf,
    /// Tiles handed over and not yet on disk, newest pixels; reads see them first.
    pending: Mutex<HashMap<TileKey, Arc<TileImage>>>,
    /// Called with every overview tile the writer rebuilt.
    on_overview: Mutex<Option<OverviewListener>>,
}

pub struct TileStore {
    shared: Arc<Shared>,
    jobs: Sender<Job>,
    writer: Option<JoinHandle<()>>,
}

impl TileStore {
    pub fn open(folder: impl Into<PathBuf>) -> TileStore {
        let shared = Arc::new(Shared {
            folder: folder.into(),
            pending: Mutex::new(HashMap::new()),
            on_overview: Mutex::new(None),
        });
        let (jobs, rx) = mpsc::channel::<Job>();
        let s = shared.clone();
        let writer = std::thread::Builder::new()
            .name("diorama-tile-writer".into())
            .spawn(move || {
                let mut overviews: HashSet<TileKey> = HashSet::new();
                // When the oldest overview waiting to be rebuilt was asked for: they are rebuilt
                // this long after, even while tiles keep coming.
                let mut since: Option<std::time::Instant> = None;
                loop {
                    let job = match since {
                        None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                        Some(t) => rx.recv_timeout(
                            (t + OVERVIEW_DELAY)
                                .saturating_duration_since(std::time::Instant::now()),
                        ),
                    };
                    match job {
                        Ok(Job::Write(key)) => {
                            write_pending(&s, key);
                            if !key.is_cave() && key.lod < TileKey::MAX_LOD {
                                overviews.insert(key.parent());
                                since.get_or_insert_with(std::time::Instant::now);
                            }
                        }
                        Ok(Job::Flush(done)) => {
                            let keys: Vec<TileKey> = lock(&s.pending).keys().copied().collect();
                            for k in keys {
                                write_pending(&s, k);
                            }
                            rebuild_overviews(&s, &mut overviews);
                            since = None;
                            let _ = done.send(());
                        }
                        Ok(Job::Stop) | Err(RecvTimeoutError::Disconnected) => {
                            let keys: Vec<TileKey> = lock(&s.pending).keys().copied().collect();
                            for k in keys {
                                write_pending(&s, k);
                            }
                            rebuild_overviews(&s, &mut overviews);
                            return;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    if since.is_some_and(|t| t.elapsed() >= OVERVIEW_DELAY) {
                        rebuild_overviews(&s, &mut overviews);
                        since = None;
                    }
                }
            })
            .expect("spawn tile writer");
        TileStore {
            shared,
            jobs,
            writer: Some(writer),
        }
    }

    pub fn folder(&self) -> &Path {
        &self.shared.folder
    }

    /// Tells `listener` of each overview tile rebuilt, on the writer thread.
    pub fn on_overview(&self, listener: impl Fn(TileKey) + Send + 'static) {
        *lock(&self.shared.on_overview) = Some(Box::new(listener));
    }

    /// The saved tile (pixels + optional light + optional spots), including ones handed over and not yet written.
    pub fn read(&self, key: TileKey) -> Option<Arc<TileImage>> {
        if let Some(p) = lock(&self.shared.pending).get(&key) {
            return Some(p.clone());
        }
        read_file(&path(&self.shared.folder, key)).map(Arc::new)
    }

    /// Hands a tile over to be written.
    pub fn put(&self, image: &TileImage) {
        lock(&self.shared.pending).insert(image.key, Arc::new(image.clone()));
        let _ = self.jobs.send(Job::Write(image.key));
    }

    /// Waits until everything handed over, and the overviews above it, is on disk.
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        if self.jobs.send(Job::Flush(tx)).is_ok() {
            let _ = rx.recv();
        }
    }
}

impl Drop for TileStore {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Stop);
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Rebuilds out-of-date overview tiles, finest level first, so each level reads the one below as written.
fn rebuild_overviews(s: &Shared, dirty: &mut HashSet<TileKey>) {
    while !dirty.is_empty() {
        let lod = dirty.iter().map(|k| k.lod).min().expect("non-empty");
        let level: Vec<TileKey> = dirty.iter().copied().filter(|k| k.lod == lod).collect();
        for key in level {
            dirty.remove(&key);
            let children: Vec<Option<Vec<u32>>> = key
                .children()
                .iter()
                .map(|c| {
                    lock(&s.pending)
                        .get(c)
                        .map(|p| p.to_vec())
                        .or_else(|| read_file(&path(&s.folder, *c)))
                })
                .collect();
            let refs: [Option<&[u32]>; 4] = std::array::from_fn(|i| children[i].as_deref());
            if refs.iter().all(Option::is_none) {
                continue;
            }
            let image = tile::overview(key, refs);
            if write_file(&path(&s.folder, key), &image).is_ok() {
                if let Some(l) = lock(&s.on_overview).as_ref() {
                    l(key);
                }
                if key.lod < TileKey::MAX_LOD {
                    dirty.insert(key.parent());
                }
            }
        }
    }
}

fn write_pending(s: &Shared, key: TileKey) {
    let Some(image) = lock(&s.pending).get(&key).cloned() else {
        return;
    };
    if write_file(&path(&s.folder, key), &image).is_ok() {
        let mut pending = lock(&s.pending);
        // Remove the entry since it's now on disk.
        // A new bake may have come in while writing, handled by the caller.
        pending.remove(&key);
    }
}

/// Where a tile lives under the store's folder.
pub fn path(folder: &Path, key: TileKey) -> PathBuf {
    let layer = if key.is_cave() {
        format!("cave{}", key.layer)
    } else {
        "surface".to_owned()
    };
    folder
        .join(layer)
        .join(key.lod.to_string())
        .join(format!("{}.{}.dtile", key.u, key.v))
}

fn write_file(path: &Path, image: &TileImage) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut out = Vec::new();
    // DTL2 magic + flags byte
    out.extend_from_slice(MAGIC);
    let flags: u8 = (image.light.is_some() as u8) | ((image.spots.is_some() as u8) << 1);
    out.push(flags);
    // Compressed pixel data
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
    out.extend_from_slice(&lz4_flex::compress_prepend_size(&bytes));
    // Compressed light data if present
    if let Some(ref light) = image.light {
        out.extend_from_slice(&lz4_flex::compress_prepend_size(light));
    }
    // Compressed spots data if present
    if let Some(ref spots) = image.spots {
        out.extend_from_slice(&lz4_flex::compress_prepend_size(spots));
    }
    // Write beside it and rename, so a crash never leaves half a tile.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, path)
}

fn read_file(path: &Path) -> Option<TileImage> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 8 || &data[..4] != MAGIC {
        return None;
    }
    let flags = data[4];
    let has_light = flags & DTL2_FLAGS_LIGHT != 0;
    let has_spots = flags & DTL2_FLAGS_SPOTS != 0;

    // Data layout after magic(4) + flags(1):
    // [compressed_pixels: 4-byte_size_prefix + bytes][compressed_light_if_has_light: 4-byte_size_prefix + bytes][compressed_spots_if_has_spots: 4-byte_size_prefix + bytes]

    // --- Read pixels ---
    // Pixels start at offset 5 (after magic + flags)
    let pixels_offset = 5usize;
    if data.len() < pixels_offset + 4 {
        return None;
    }
    let pixels_size =
        u32::from_le_bytes([data[pixels_offset], data[pixels_offset + 1], data[pixels_offset + 2], data[pixels_offset + 3]]) as usize;
    let pixels_data_start = pixels_offset + 4;
    let pixels_data_end = pixels_data_start + pixels_size;

    if pixels_data_end > data.len() {
        return None;
    }
    let pixels_compressed = &data[pixels_data_start..pixels_data_end];
    let pixels = lz4_flex::decompress_size_prepended(pixels_compressed).ok()?;
    if pixels.len() != PIXELS * 4 {
        return None;
    }

    let mut result = TileImage {
        key: TileKey::new(0, 0, 0, 0),
        pixels,
        light: None,
        spots: None,
    };

    // --- Read light if present ---
    let mut current_pos = pixels_data_end;
    if has_light && current_pos + 4 <= data.len() {
        let light_size =
            u32::from_le_bytes([data[current_pos], data[current_pos + 1], data[current_pos + 2], data[current_pos + 3]]) as usize;
        let light_data_start = current_pos + 4;
        let light_data_end = light_data_start + light_size;

        if light_data_end > data.len() {
            return None;
        }
        let light_decompressed = lz4_flex::decompress_size_prepended(&data[light_data_start..light_data_end]).ok()?;
        result.light = Some(light_decompressed);
        current_pos = light_data_end;
    } else if has_light {
        // No room for light section
        return None;
    }

    // --- Read spots if present ---
    if has_spots && current_pos + 4 <= data.len() {
        let spots_size =
            u32::from_le_bytes([data[current_pos], data[current_pos + 1], data[current_pos + 2], data[current_pos + 3]]) as usize;
        let spots_data_start = current_pos + 4;
        let spots_data_end = spots_data_start + spots_size;

        if spots_data_end > data.len() {
            return None;
        }
        let spots_compressed = &data[spots_data_start..spots_data_end];
        let spots_decompressed = lz4_flex::decompress_size_prepended(spots_compressed).ok()?;
        // Spots are u32 values
        if spots_decompressed.len() % 4 == 0 {
            let spots_count = spots_decompressed.len() / 4;
            result.spots = Some(
                spots_decompressed
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            );
        }
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("diorama-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn writes_reads_and_builds_overviews() {
        let dir = temp("rw");
        let key = TileKey::new(TileKey::SURFACE, 1, -1);
        let mut img = TileImage::blank(key);
        img.pixels[5] = 0xFF11_2233;
        {
            let store = TileStore::open(&dir);
            store.put(&img);
            let read_img = store.read(key).unwrap();
            assert_eq!((*read_img).pixels[5], 0xFF11_2233, "pending reads");
            store.flush();
        }
        let store = TileStore::open(&dir);
        let read_img = store.read(key).unwrap();
        assert_eq!((*read_img).pixels[5], 0xFF11_2233, "from disk");
        let parent = store.read(key.parent()).expect("overview built");
        assert_eq!(parent.len(), PIXELS);
        assert!(store.read(key.parent().parent()).is_some());
        assert!(store.read(TileKey::new(TileKey::SURFACE, 9, 9)).is_none());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overviews_rebuild_while_tiles_keep_coming() {
        let dir = temp("busy");
        let store = TileStore::open(&dir);
        let key = TileKey::new(TileKey::SURFACE, 0, 0);
        let started = std::time::Instant::now();
        let mut img = TileImage::blank(key);
        img.pixels[0] = 0xFF00_00FF;
        while store.read(key.parent()).is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "overview built within the delay"
            );
            store.put(&img);
            std::thread::sleep(Duration::from_millis(100));
        }
        // Verify the tile can be read back
        let read_img = store.read(key).unwrap();
        assert_eq!((*read_img).pixels[0], 0xFF00_00FF);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caves_have_no_overviews() {
        let dir = temp("cave");
        let key = TileKey::new(40, 0, 0);
        let store = TileStore::open(&dir);
        store.put(&TileImage::blank(key));
        store.flush();
        assert!(store.read(key).is_some());
        assert!(store.read(key.parent()).is_none());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
