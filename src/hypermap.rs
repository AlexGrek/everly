//! Hypermap: an infinite, chunked, concurrent tile store.
//!
//! World space is addressed by signed integer tile coordinates. Tiles are grouped
//! into fixed 64x64 chunks that are allocated lazily on first write.
//! Each chunk has its own lock so disconnected regions can be accessed concurrently.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub const HYPERMAP_CHUNK_SIZE: i32 = 64;
const HYPERMAP_CHUNK_AREA: usize = (HYPERMAP_CHUNK_SIZE as usize) * (HYPERMAP_CHUNK_SIZE as usize);
/// Number of vertical floors per column (indices `0..HYPERMAP_FLOOR_COUNT`).
pub const HYPERMAP_FLOOR_COUNT: usize = 10;
const HYPERMAP_CHUNK_CELL_COUNT: usize = HYPERMAP_CHUNK_AREA * HYPERMAP_FLOOR_COUNT;

/// Chunk-space coordinate (can be positive or negative).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkCoord {
    pub x: i32,
    pub y: i32,
}

impl ChunkCoord {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Local coordinate inside one 64x64 chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCoord {
    pub x: u8,
    pub y: u8,
}

impl LocalCoord {
    pub const fn new(x: u8, y: u8) -> Self {
        Self { x, y }
    }
}

#[derive(Debug)]
pub struct HypermapChunk<T>
where
    T: Clone + Send + Sync + 'static,
{
    cells: Vec<T>,
}

impl<T> HypermapChunk<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn new(fill: &T) -> Self {
        Self {
            cells: vec![fill.clone(); HYPERMAP_CHUNK_CELL_COUNT],
        }
    }

    #[inline]
    pub fn get_local_floor(&self, local: LocalCoord, floor: u8) -> &T {
        &self.cells[local_floor_to_index(local, floor)]
    }

    #[inline]
    pub fn set_local_floor(&mut self, local: LocalCoord, floor: u8, value: T) {
        self.cells[local_floor_to_index(local, floor)] = value;
    }

    /// Ground floor (`0`); same as [`Self::get_local_floor`](Self::get_local_floor)(`local`, `0`).
    pub fn get_local(&self, local: LocalCoord) -> &T {
        self.get_local_floor(local, 0)
    }

    /// Writes ground floor (`0`).
    pub fn set_local(&mut self, local: LocalCoord, value: T) {
        self.set_local_floor(local, 0, value);
    }
}

pub type HypermapChunkHandle<T> = Arc<RwLock<HypermapChunk<T>>>;

/// Infinite chunked tile map optimized for sparse, large worlds.
#[derive(Debug)]
pub struct Hypermap<T>
where
    T: Clone + Send + Sync + 'static,
{
    chunks: RwLock<HashMap<ChunkCoord, HypermapChunkHandle<T>>>,
    default_tile: T,
}

impl<T> Hypermap<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new(default_tile: T) -> Self {
        Self {
            chunks: RwLock::new(HashMap::new()),
            default_tile,
        }
    }

    /// Returns tile value at world coordinate on **ground floor** (`0`). Missing chunks read as default tile.
    pub fn get(&self, world_x: i32, world_y: i32) -> T {
        self.get_floor(world_x, world_y, 0)
    }

    /// Writes tile value at world coordinate on **ground floor** (`0`), creating chunk lazily if needed.
    pub fn set(&self, world_x: i32, world_y: i32, value: T) {
        self.set_floor(world_x, world_y, 0, value);
    }

    /// Tile at world `(x, y)` and elevation `floor` in `0..HYPERMAP_FLOOR_COUNT`.
    pub fn get_floor(&self, world_x: i32, world_y: i32, floor: u8) -> T {
        let (chunk, local) = world_to_chunk_local(world_x, world_y);
        if let Some(chunk_handle) = self.get_chunk(chunk) {
            let guard = chunk_handle.read().expect("chunk lock poisoned");
            guard.get_local_floor(local, floor).clone()
        } else {
            self.default_tile.clone()
        }
    }

    pub fn set_floor(&self, world_x: i32, world_y: i32, floor: u8, value: T) {
        let (chunk_coord, local) = world_to_chunk_local(world_x, world_y);
        let chunk_handle = self.get_or_create_chunk(chunk_coord);
        let mut guard = chunk_handle.write().expect("chunk lock poisoned");
        guard.set_local_floor(local, floor, value);
    }

    /// Updates a world tile on **ground floor** (`0`) in place.
    pub fn update<F>(&self, world_x: i32, world_y: i32, f: F)
    where
        F: FnOnce(&mut T),
    {
        let (chunk_coord, local) = world_to_chunk_local(world_x, world_y);
        let chunk_handle = self.get_or_create_chunk(chunk_coord);
        let mut guard = chunk_handle.write().expect("chunk lock poisoned");
        let idx = local_floor_to_index(local, 0);
        f(&mut guard.cells[idx]);
    }

    pub fn has_chunk(&self, coord: ChunkCoord) -> bool {
        self.chunks
            .read()
            .expect("hypermap lock poisoned")
            .contains_key(&coord)
    }

    pub fn loaded_chunk_count(&self) -> usize {
        self.chunks.read().expect("hypermap lock poisoned").len()
    }

    pub fn get_chunk(&self, coord: ChunkCoord) -> Option<HypermapChunkHandle<T>> {
        self.chunks
            .read()
            .expect("hypermap lock poisoned")
            .get(&coord)
            .cloned()
    }

    pub fn get_or_create_chunk(&self, coord: ChunkCoord) -> HypermapChunkHandle<T> {
        if let Some(existing) = self.get_chunk(coord) {
            return existing;
        }

        let mut chunks = self.chunks.write().expect("hypermap lock poisoned");
        chunks
            .entry(coord)
            .or_insert_with(|| Arc::new(RwLock::new(HypermapChunk::new(&self.default_tile))))
            .clone()
    }

    /// Applies a closure to one chunk with read access if loaded.
    pub fn with_chunk_read<R, F>(&self, coord: ChunkCoord, f: F) -> Option<R>
    where
        F: FnOnce(&HypermapChunk<T>) -> R,
    {
        let handle = self.get_chunk(coord)?;
        let guard = handle.read().expect("chunk lock poisoned");
        Some(f(&guard))
    }

    /// Applies a closure to one chunk with write access, creating it lazily.
    pub fn with_chunk_write<R, F>(&self, coord: ChunkCoord, f: F) -> R
    where
        F: FnOnce(&mut HypermapChunk<T>) -> R,
    {
        let handle = self.get_or_create_chunk(coord);
        let mut guard = handle.write().expect("chunk lock poisoned");
        f(&mut guard)
    }
}

pub fn world_to_chunk_local(world_x: i32, world_y: i32) -> (ChunkCoord, LocalCoord) {
    let chunk_x = floor_div(world_x, HYPERMAP_CHUNK_SIZE);
    let chunk_y = floor_div(world_y, HYPERMAP_CHUNK_SIZE);

    let local_x = floor_mod(world_x, HYPERMAP_CHUNK_SIZE) as u8;
    let local_y = floor_mod(world_y, HYPERMAP_CHUNK_SIZE) as u8;

    (ChunkCoord::new(chunk_x, chunk_y), LocalCoord::new(local_x, local_y))
}

fn local_to_index(local: LocalCoord) -> usize {
    local.y as usize * HYPERMAP_CHUNK_SIZE as usize + local.x as usize
}

#[inline]
fn local_floor_to_index(local: LocalCoord, floor: u8) -> usize {
    debug_assert!((floor as usize) < HYPERMAP_FLOOR_COUNT);
    local_to_index(local) * HYPERMAP_FLOOR_COUNT + floor as usize
}

fn floor_div(a: i32, b: i32) -> i32 {
    a.div_euclid(b)
}

fn floor_mod(a: i32, b: i32) -> i32 {
    a.rem_euclid(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn maps_negative_world_coords_to_correct_chunk_and_local() {
        let (chunk, local) = world_to_chunk_local(-1, -1);
        assert_eq!(chunk, ChunkCoord::new(-1, -1));
        assert_eq!(local, LocalCoord::new(63, 63));

        let (chunk, local) = world_to_chunk_local(-64, -64);
        assert_eq!(chunk, ChunkCoord::new(-1, -1));
        assert_eq!(local, LocalCoord::new(0, 0));

        let (chunk, local) = world_to_chunk_local(-65, 64);
        assert_eq!(chunk, ChunkCoord::new(-2, 1));
        assert_eq!(local, LocalCoord::new(63, 0));
    }

    #[test]
    fn reads_default_from_unallocated_space() {
        let map = Hypermap::new(7i32);
        assert_eq!(map.get(10_000, -10_000), 7);
        assert_eq!(map.loaded_chunk_count(), 0);
    }

    #[test]
    fn allocates_chunks_lazily_on_write() {
        let map = Hypermap::new(0i32);
        map.set(0, 0, 1);
        map.set(200, 0, 2);
        map.set(-200, -200, 3);

        assert_eq!(map.get(0, 0), 1);
        assert_eq!(map.get(200, 0), 2);
        assert_eq!(map.get(-200, -200), 3);
        assert_eq!(map.loaded_chunk_count(), 3);
    }

    #[test]
    fn reads_and_writes_upper_floors_independently() {
        let map = Hypermap::new(0u8);
        map.set_floor(5, 5, 0, 1);
        map.set_floor(5, 5, 3, 2);
        assert_eq!(map.get_floor(5, 5, 0), 1);
        assert_eq!(map.get_floor(5, 5, 1), 0);
        assert_eq!(map.get_floor(5, 5, 3), 2);
        assert_eq!(map.get(5, 5), 1);
    }

    #[test]
    fn allows_parallel_writes_to_disconnected_chunks() {
        let map = Arc::new(Hypermap::new(0u32));
        let left = map.clone();
        let right = map.clone();

        let t1 = thread::spawn(move || {
            for x in -256..-192 {
                left.set(x, 0, 1);
            }
        });
        let t2 = thread::spawn(move || {
            for x in 192..256 {
                right.set(x, 0, 2);
            }
        });

        t1.join().expect("thread 1 should complete");
        t2.join().expect("thread 2 should complete");

        assert_eq!(map.get(-200, 0), 1);
        assert_eq!(map.get(200, 0), 2);
    }
}
