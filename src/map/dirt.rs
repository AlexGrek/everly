//! Per-subtile dirt amounts stored in a double-buffered hypermap.
//!
//! Resolution is [`DIRT_SUBDIV`]×[`DIRT_SUBDIV`] samples per world tile (10×10 → 0.1 m
//! texels), twice the passability / generic overlay grid ([`SUBTILE_COUNT`] = 5).
//!
//! ## Frame lifecycle
//!
//! 1. Field writers ([`crate::map::field_interactions`], seeding) — **write** buffer only.
//! 2. [`flush_dirt_map`] after writers — merges write→read when the write buffer is non-empty.
//! 3. Overlay and gameplay queries read the **read** buffer via [`DirtMap::read_map`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;

use crate::map::hypermap::{
    world_to_chunk_local, ChunkCoord, DoubleBufferedHypermap, Hypermap, LocalCoord,
    HYPERMAP_CHUNK_SIZE,
};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::world_map::CellType;
use crate::menu::main_menu::GameState;

/// Dirt samples per world-tile axis (0.1 m per sample).
pub const DIRT_SUBDIV: usize = 10;

/// Overlay / hypermap texels per chunk edge (`128 × DIRT_SUBDIV`).
pub const DIRT_OVERLAY_RES: u32 = crate::map::hypermap::HYPERMAP_CHUNK_SIZE as u32 * DIRT_SUBDIV as u32;

/// Probability that a non-void ground tile receives a dirty patch when its chunk is first seeded.
const DIRTY_TILE_CHANCE: f32 = 0.06;

/// Dirt added to each sample in a tile when an actor leaves it (see `field_interactions`).
pub const DIRT_TRACK_DEPOSIT: f32 = 0.01;

/// Chunk-local hypermap cell: flat `DIRT_SUBDIV²` grid of dirt in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirtTile {
    pub cells: [f32; DIRT_SUBDIV * DIRT_SUBDIV],
}

impl DirtTile {
    pub const CLEAN: Self = Self { cells: [0.0; DIRT_SUBDIV * DIRT_SUBDIV] };

    #[inline]
    pub fn at(&self, row: usize, col: usize) -> f32 {
        self.cells[row * DIRT_SUBDIV + col]
    }

    #[inline]
    pub fn set(&mut self, row: usize, col: usize, value: f32) {
        self.cells[row * DIRT_SUBDIV + col] = value.clamp(0.0, 1.0);
    }

    #[inline]
    pub fn fill(&mut self, value: f32) {
        let v = value.clamp(0.0, 1.0);
        self.cells.fill(v);
    }
}

/// Ground-floor dirt hypermap (double-buffered) plus seeding / overlay dirty tracking.
#[derive(Resource)]
pub struct DirtMap {
    inner: Arc<DoubleBufferedHypermap<DirtTile>>,
    seeded_chunks: Mutex<HashSet<ChunkCoord>>,
    dirty_chunks: Mutex<HashSet<ChunkCoord>>,
}

impl DirtMap {
    pub fn inner(&self) -> &DoubleBufferedHypermap<DirtTile> {
        &self.inner
    }

    /// Committed dirt (last [`flush`](Self::flush)). Safe for parallel reads while writers use the write buffer.
    pub fn read_map(&self) -> &Hypermap<DirtTile> {
        self.inner.read_map()
    }

    /// Merges pending write-buffer chunks into read and marks them dirty for the overlay.
    pub fn flush(&self) {
        let pending = self.inner.write_map().loaded_chunks();
        self.inner.flush_merge();
        let mut dirty = self
            .dirty_chunks
            .lock()
            .expect("dirt dirty_chunks lock poisoned");
        dirty.extend(pending);
    }

    pub fn get_subtile(&self, world_x: i32, world_y: i32, row: usize, col: usize) -> f32 {
        debug_assert!(row < DIRT_SUBDIV && col < DIRT_SUBDIV);
        let (chunk, local) = world_to_chunk_local(world_x, world_y);
        self.inner
            .with_chunk_read(chunk, |chunk| chunk.get_local(local).at(row, col))
            .unwrap_or(0.0)
    }

    pub fn set_subtile(&self, world_x: i32, world_y: i32, row: usize, col: usize, value: f32) {
        debug_assert!(row < DIRT_SUBDIV && col < DIRT_SUBDIV);
        let (coord, _) = world_to_chunk_local(world_x, world_y);
        self.inner.update(world_x, world_y, |tile| tile.set(row, col, value));
        self.mark_dirty(coord);
    }

    /// Adds `delta` to every dirt sample in the world tile (write buffer).
    pub fn add_tile_dirt(&self, world_x: i32, world_y: i32, delta: f32) {
        let (coord, _) = world_to_chunk_local(world_x, world_y);
        self.inner.update(world_x, world_y, |tile| {
            for cell in &mut tile.cells {
                *cell = (*cell + delta).clamp(0.0, 1.0);
            }
        });
        self.mark_dirty(coord);
    }

    pub fn mark_dirty(&self, coord: ChunkCoord) {
        self.dirty_chunks
            .lock()
            .expect("dirt dirty_chunks lock poisoned")
            .insert(coord);
    }

    /// Removes and returns chunk coords that need an overlay repaint.
    pub fn take_dirty_chunks(&self) -> HashSet<ChunkCoord> {
        std::mem::take(
            &mut *self
                .dirty_chunks
                .lock()
                .expect("dirt dirty_chunks lock poisoned"),
        )
    }

    /// Seeds scattered dirty patches for `coord` once, using `world` ground-floor cells.
    /// Skips void tiles; no-op if the world chunk is not loaded yet. Writes the **write** buffer.
    pub fn ensure_chunk_seeded(&self, world: &Hypermap<CellType>, coord: ChunkCoord) {
        {
            let seeded = self.seeded_chunks.lock().expect("dirt seeded_chunks lock poisoned");
            if seeded.contains(&coord) {
                return;
            }
        }

        let Some(()) = world.with_chunk_read(coord, |_| ()) else {
            return;
        };

        let seed = dirt_chunk_seed(coord);
        let mut rng = StdRng::seed_from_u64(seed);

        world.with_chunk_read(coord, |wchunk| {
            self.inner.with_chunk_write(coord, |dchunk| {
                for ly in 0..HYPERMAP_CHUNK_SIZE {
                    for lx in 0..HYPERMAP_CHUNK_SIZE {
                        let local = LocalCoord::new(lx, ly);
                        let cell = *wchunk.get_local_floor(local, 0);
                        if matches!(cell, CellType::Void) {
                            continue;
                        }
                        if rng.gen_range(0.0..1.0) >= DIRTY_TILE_CHANCE {
                            continue;
                        }
                        let level = rng.gen_range(0.1..=0.3);
                        let mut tile = *dchunk.get_local(local);
                        tile.fill(level);
                        dchunk.set_local(local, tile);
                    }
                }
            });
        });

        self.seeded_chunks
            .lock()
            .expect("dirt seeded_chunks lock poisoned")
            .insert(coord);
        self.mark_dirty(coord);
    }
}

pub struct DirtMapPlugin;

impl Plugin for DirtMapPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(DirtMap {
            inner: Arc::new(DoubleBufferedHypermap::new(DirtTile::CLEAN)),
            seeded_chunks: Mutex::new(HashSet::new()),
            dirty_chunks: Mutex::new(HashSet::new()),
        })
        .add_systems(
            Update,
            (seed_dirt_for_visible_chunks, flush_dirt_map)
                .chain()
                .run_if(in_state(GameState::InGame)),
        );
    }
}

pub(crate) fn flush_dirt_map(dirt: Res<DirtMap>) {
    if dirt.inner.write_map().loaded_chunk_count() == 0 {
        return;
    }
    dirt.flush();
}

pub(crate) fn seed_dirt_for_visible_chunks(runtime: Res<HypermapRuntime>, dirt: Res<DirtMap>) {
    for coord in runtime.desired_chunk_coords() {
        dirt.ensure_chunk_seeded(&runtime.map, coord);
    }
}

fn dirt_chunk_seed(coord: ChunkCoord) -> u64 {
    let x = coord.x as u64;
    let y = coord.y as u64;
    x.wrapping_mul(0x9E37_79B9_85F3_7D87)
        ^ y.wrapping_mul(0xC2B2_AE3D_27D4_F4F5)
        ^ 0xD1A7_0000_0001
}
