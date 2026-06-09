//! Hypermap: an infinite, chunked, concurrent tile store.
//!
//! World space is addressed by signed integer tile coordinates. Tiles are grouped
//! into fixed 128x128 chunks that are allocated lazily on first write.
//! Each chunk has its own lock so disconnected regions can be accessed concurrently.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;

pub const HYPERMAP_CHUNK_SIZE: i32 = 128;
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

/// Random `u64` for [`crate::rng::seeded`] when a chunk is first filled procedurally.
///
/// Not derived from chunk coordinates — each first-time generation pass gets a fresh layout.
pub fn random_rng_seed() -> u64 {
    crate::rng::fresh_seed()
}

/// Local coordinate inside one 128x128 chunk. Components are `i32` so
/// callers can do unrestricted arithmetic; values must be in
/// `0..HYPERMAP_CHUNK_SIZE` when used to index a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCoord {
    pub x: i32,
    pub y: i32,
}

impl LocalCoord {
    pub const fn new(x: i32, y: i32) -> Self {
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
    pub fn get_local_floor(&self, local: LocalCoord, floor: i32) -> &T {
        &self.cells[local_floor_to_index(local, floor)]
    }

    #[inline]
    pub fn set_local_floor(&mut self, local: LocalCoord, floor: i32, value: T) {
        self.cells[local_floor_to_index(local, floor)] = value;
    }

    /// Ground floor (`0`); same as [`Self::get_local_floor`](Self::get_local_floor)(`local`, `0`).
    pub fn get_local(&self, local: LocalCoord) -> &T {
        self.get_local_floor(local, 0)
    }

    /// Mutable ground-floor (`0`) cell reference for in-place updates without
    /// cloning the cell value (hot path for subtile footprint stamping).
    #[inline]
    pub fn get_local_mut(&mut self, local: LocalCoord) -> &mut T {
        &mut self.cells[local_floor_to_index(local, 0)]
    }

    /// Writes ground floor (`0`).
    pub fn set_local(&mut self, local: LocalCoord, value: T) {
        self.set_local_floor(local, 0, value);
    }
}

pub type HypermapChunkHandle<T> = Arc<RwLock<HypermapChunk<T>>>;

type ChunkTable<T> = HashMap<ChunkCoord, HypermapChunkHandle<T>>;

/// Infinite chunked tile map optimized for sparse, large worlds.
///
/// The chunk table is published as a **lock-free read snapshot** (`snapshot`)
/// alongside the authoritative, mutable `chunks` map. All reads
/// ([`get_chunk`](Self::get_chunk), [`has_chunk`](Self::has_chunk), etc.) load
/// the immutable snapshot with no lock and no shared-atomic-counter contention,
/// so many cores (parallel actors + async pathfind workers) resolve chunks
/// without bouncing a `RwLock` word between caches. The `chunks` write lock is
/// taken only on a *structural* change (creating, draining, or replacing chunk
/// handles), which then republishes the snapshot with a single atomic
/// `ArcSwap::store` — making the wholesale flush (`replace_chunks`) atomic for
/// concurrent readers (a worker always sees a fully-old or fully-new table).
#[derive(Debug)]
pub struct Hypermap<T>
where
    T: Clone + Send + Sync + 'static,
{
    chunks: RwLock<ChunkTable<T>>,
    snapshot: ArcSwap<ChunkTable<T>>,
    default_tile: T,
}

/// Two [`Hypermap`]s in a front/back arrangement.
///
/// Reads hit the **read** buffer; writes hit the **write** buffer.
/// [`flush`](Self::flush) atomically promotes the write buffer to the read
/// side and resets the write buffer to its clean (default-tile) state.
#[derive(Debug)]
pub struct DoubleBufferedHypermap<T>
where
    T: Clone + Send + Sync + 'static,
{
    read: Hypermap<T>,
    write: Hypermap<T>,
}

impl<T> Hypermap<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new(default_tile: T) -> Self {
        Self {
            chunks: RwLock::new(HashMap::new()),
            snapshot: ArcSwap::from_pointee(HashMap::new()),
            default_tile,
        }
    }

    /// Republishes the lock-free read snapshot from the authoritative table.
    /// Must be called while holding the `chunks` write guard so concurrent
    /// republishes stay ordered (the passed guard proves that here).
    fn republish(&self, chunks: &ChunkTable<T>) {
        self.snapshot.store(Arc::new(chunks.clone()));
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
    pub fn get_floor(&self, world_x: i32, world_y: i32, floor: i32) -> T {
        let (chunk, local) = world_to_chunk_local(world_x, world_y);
        if let Some(chunk_handle) = self.get_chunk(chunk) {
            let guard = chunk_handle.read().expect("chunk lock poisoned");
            guard.get_local_floor(local, floor).clone()
        } else {
            self.default_tile.clone()
        }
    }

    pub fn set_floor(&self, world_x: i32, world_y: i32, floor: i32, value: T) {
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
        self.snapshot.load().contains_key(&coord)
    }

    pub fn loaded_chunk_count(&self) -> usize {
        self.snapshot.load().len()
    }

    /// Snapshot of every chunk coordinate currently held in memory. Order is unspecified.
    pub fn loaded_chunks(&self) -> Vec<ChunkCoord> {
        self.snapshot.load().keys().copied().collect()
    }

    pub fn get_chunk(&self, coord: ChunkCoord) -> Option<HypermapChunkHandle<T>> {
        self.snapshot.load().get(&coord).cloned()
    }

    pub fn get_or_create_chunk(&self, coord: ChunkCoord) -> HypermapChunkHandle<T> {
        if let Some(existing) = self.get_chunk(coord) {
            return existing;
        }

        let mut chunks = self.chunks.write().expect("hypermap lock poisoned");
        let handle = chunks
            .entry(coord)
            .or_insert_with(|| Arc::new(RwLock::new(HypermapChunk::new(&self.default_tile))))
            .clone();
        self.republish(&chunks);
        handle
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

    /// Removes and returns all chunk handles, leaving the map empty.
    fn drain_chunks(&self) -> ChunkTable<T> {
        let mut chunks = self.chunks.write().expect("hypermap lock poisoned");
        let taken = std::mem::take(&mut *chunks);
        self.republish(&chunks);
        taken
    }

    /// Replaces the chunk table wholesale. Previous contents are dropped. The
    /// new table is published to readers with a single atomic snapshot store, so
    /// a concurrent reader never observes a partially-replaced table.
    fn replace_chunks(&self, new_chunks: ChunkTable<T>) {
        let published = Arc::new(new_chunks);
        let mut chunks = self.chunks.write().expect("hypermap lock poisoned");
        *chunks = (*published).clone();
        self.snapshot.store(published);
    }
}

impl<T> DoubleBufferedHypermap<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new(default_tile: T) -> Self {
        Self {
            read: Hypermap::new(default_tile.clone()),
            write: Hypermap::new(default_tile),
        }
    }

    // --- read-side delegates ---

    pub fn get(&self, world_x: i32, world_y: i32) -> T {
        self.read.get(world_x, world_y)
    }

    pub fn get_floor(&self, world_x: i32, world_y: i32, floor: i32) -> T {
        self.read.get_floor(world_x, world_y, floor)
    }

    pub fn has_chunk(&self, coord: ChunkCoord) -> bool {
        self.read.has_chunk(coord)
    }

    pub fn loaded_chunk_count(&self) -> usize {
        self.read.loaded_chunk_count()
    }

    pub fn loaded_chunks(&self) -> Vec<ChunkCoord> {
        self.read.loaded_chunks()
    }

    pub fn get_chunk(&self, coord: ChunkCoord) -> Option<HypermapChunkHandle<T>> {
        self.read.get_chunk(coord)
    }

    pub fn with_chunk_read<R, F>(&self, coord: ChunkCoord, f: F) -> Option<R>
    where
        F: FnOnce(&HypermapChunk<T>) -> R,
    {
        self.read.with_chunk_read(coord, f)
    }

    // --- write-side delegates ---

    pub fn set(&self, world_x: i32, world_y: i32, value: T) {
        self.write.set(world_x, world_y, value);
    }

    pub fn set_floor(&self, world_x: i32, world_y: i32, floor: i32, value: T) {
        self.write.set_floor(world_x, world_y, floor, value);
    }

    pub fn update<F>(&self, world_x: i32, world_y: i32, f: F)
    where
        F: FnOnce(&mut T),
    {
        self.write.update(world_x, world_y, f);
    }

    pub fn get_or_create_chunk(&self, coord: ChunkCoord) -> HypermapChunkHandle<T> {
        self.write.get_or_create_chunk(coord)
    }

    pub fn with_chunk_write<R, F>(&self, coord: ChunkCoord, f: F) -> R
    where
        F: FnOnce(&mut HypermapChunk<T>) -> R,
    {
        self.write.with_chunk_write(coord, f)
    }

    // --- double-buffer lifecycle ---

    /// Promotes the write buffer to the read side and resets the write
    /// buffer to a clean state (all chunks dropped, reads return `default_tile`).
    pub fn flush(&self) {
        let write_chunks = self.write.drain_chunks();
        self.read.replace_chunks(write_chunks);
    }

    /// Copies every write-buffer chunk into the matching read-buffer chunk, then
    /// clears the write buffer. Unchanged read chunks are preserved (unlike [`flush`]).
    pub fn flush_merge(&self) {
        let write_chunks = self.write.drain_chunks();
        for (coord, handle) in write_chunks {
            let src = handle.read().expect("chunk lock poisoned");
            self.read.with_chunk_write(coord, |dst| {
                for y in 0..HYPERMAP_CHUNK_SIZE {
                    for x in 0..HYPERMAP_CHUNK_SIZE {
                        let local = LocalCoord::new(x, y);
                        dst.set_local(local, src.get_local(local).clone());
                    }
                }
            });
        }
    }

    /// Direct access to the read-side [`Hypermap`].
    pub fn read_map(&self) -> &Hypermap<T> {
        &self.read
    }

    /// Direct access to the write-side [`Hypermap`].
    pub fn write_map(&self) -> &Hypermap<T> {
        &self.write
    }
}

pub fn world_to_chunk_local(world_x: i32, world_y: i32) -> (ChunkCoord, LocalCoord) {
    let chunk_x = floor_div(world_x, HYPERMAP_CHUNK_SIZE);
    let chunk_y = floor_div(world_y, HYPERMAP_CHUNK_SIZE);

    let local_x = floor_mod(world_x, HYPERMAP_CHUNK_SIZE);
    let local_y = floor_mod(world_y, HYPERMAP_CHUNK_SIZE);

    (ChunkCoord::new(chunk_x, chunk_y), LocalCoord::new(local_x, local_y))
}

fn local_to_index(local: LocalCoord) -> usize {
    debug_assert!(local.x >= 0 && local.x < HYPERMAP_CHUNK_SIZE);
    debug_assert!(local.y >= 0 && local.y < HYPERMAP_CHUNK_SIZE);
    local.y as usize * HYPERMAP_CHUNK_SIZE as usize + local.x as usize
}

#[inline]
fn local_floor_to_index(local: LocalCoord, floor: i32) -> usize {
    debug_assert!(floor >= 0 && (floor as usize) < HYPERMAP_FLOOR_COUNT);
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
        assert_eq!(local, LocalCoord::new(127, 127));

        let (chunk, local) = world_to_chunk_local(-128, -128);
        assert_eq!(chunk, ChunkCoord::new(-1, -1));
        assert_eq!(local, LocalCoord::new(0, 0));

        let (chunk, local) = world_to_chunk_local(-129, 128);
        assert_eq!(chunk, ChunkCoord::new(-2, 1));
        assert_eq!(local, LocalCoord::new(127, 0));
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

    // --- DoubleBufferedHypermap ---

    #[test]
    fn double_buf_reads_default_before_flush() {
        let db = DoubleBufferedHypermap::new(0i32);
        db.set(5, 5, 42);
        assert_eq!(db.get(5, 5), 0, "read side untouched before flush");
    }

    #[test]
    fn double_buf_flush_promotes_writes_to_read() {
        let db = DoubleBufferedHypermap::new(0i32);
        db.set(10, 10, 99);
        db.set_floor(10, 10, 2, 77);
        db.flush();

        assert_eq!(db.get(10, 10), 99);
        assert_eq!(db.get_floor(10, 10, 2), 77);
    }

    #[test]
    fn double_buf_write_resets_after_flush() {
        let db = DoubleBufferedHypermap::new(0i32);
        db.set(1, 1, 5);
        db.flush();
        assert_eq!(db.get(1, 1), 5);

        db.flush();
        assert_eq!(db.get(1, 1), 0, "second flush with no new writes resets read to default");
    }

    #[test]
    fn double_buf_loaded_chunks_reflects_read_side() {
        let db = DoubleBufferedHypermap::new(0i32);
        assert_eq!(db.loaded_chunk_count(), 0);

        db.set(0, 0, 1);
        assert_eq!(db.loaded_chunk_count(), 0, "write-side chunk not counted on read");

        db.flush();
        assert_eq!(db.loaded_chunk_count(), 1);
    }

    #[test]
    fn double_buf_flush_merge_preserves_untouched_read_chunks() {
        let db = DoubleBufferedHypermap::new(0i32);
        db.read_map().set(0, 0, 11);
        db.write_map().set(200, 0, 22);
        db.flush_merge();

        assert_eq!(db.get(0, 0), 11);
        assert_eq!(db.get(200, 0), 22);
        assert_eq!(db.write_map().loaded_chunk_count(), 0);
    }
}
