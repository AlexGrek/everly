//! Ground-floor **tile-resolution** dirt (`f32` per world tile, `0.0..=1.0`).
//!
//! See `docs/field-interactions.md` for actor deposits and `docs/chunk-overlay.md` for rendering.

use std::collections::HashSet;
use std::sync::Mutex;

use bevy::prelude::*;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;

use crate::map::hypermap::{random_rng_seed, ChunkCoord, Hypermap, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::level::LevelName;
use crate::map::tile_field_level::{dirt_bin_path, load_tile_field_bin};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::tile_field::TileFieldMap;
use crate::map::world_map::CellType;
use crate::menu::main_menu::GameState;

pub use crate::map::tile_field::TILE_FIELD_OVERLAY_RES as DIRT_OVERLAY_RES;

const DIRT_CLAMP_MAX: f32 = 1.0;

/// Probability that a non-void ground tile receives a dirty patch when its chunk is first seeded.
const DIRTY_TILE_CHANCE: f32 = 0.06;
/// Dirt added to a tile when an actor leaves it (see `field_interactions`).
pub const DIRT_TRACK_DEPOSIT: f32 = 0.01;

/// Tile-resolution dirt hypermap (double-buffered).
#[derive(Resource)]
pub struct DirtMap {
    field: TileFieldMap,
    seeded_chunks: Mutex<HashSet<ChunkCoord>>,
    /// Level whose `dirt.bin` has been merged into the hypermap this session.
    hydrated_level: Mutex<Option<String>>,
}

impl DirtMap {
    pub fn field(&self) -> &TileFieldMap {
        &self.field
    }

    pub fn inner(&self) -> &crate::map::hypermap::DoubleBufferedHypermap<f32> {
        self.field.inner()
    }

    pub fn read_map(&self) -> &Hypermap<f32> {
        self.field.read_map()
    }

    pub fn get_tile(&self, world_x: i32, world_y: i32) -> f32 {
        self.field.get_tile(world_x, world_y)
    }

    pub fn set_tile(&self, world_x: i32, world_y: i32, value: f32) {
        self.field.set_tile(world_x, world_y, value);
    }

    pub fn add_tile_dirt(&self, world_x: i32, world_y: i32, delta: f32) {
        self.field.add_tile(world_x, world_y, delta);
    }

    pub fn mark_dirty(&self, coord: ChunkCoord) {
        self.field.mark_dirty(coord);
    }

    pub fn take_dirty_chunks(&self) -> HashSet<ChunkCoord> {
        self.field.take_dirty_chunks()
    }

    fn hydrate_level_bin(&self, level_name: &str) {
        let mut slot = self
            .hydrated_level
            .lock()
            .expect("dirt hydrated_level lock poisoned");
        if slot.as_deref() == Some(level_name) {
            return;
        }
        *slot = Some(level_name.to_string());
        drop(slot);

        let path = dirt_bin_path(level_name);
        if let Err(e) = load_tile_field_bin(&path, self.field.inner().write_map()) {
            warn!("failed to load `{}`: {e}", path.display());
        }
        self.field.flush_if_pending();
    }

    pub fn ensure_chunk_seeded(
        &self,
        world: &Hypermap<CellType>,
        coord: ChunkCoord,
        level_name: &str,
    ) {
        {
            let seeded = self.seeded_chunks.lock().expect("dirt seeded_chunks lock poisoned");
            if seeded.contains(&coord) {
                return;
            }
        }

        let Some(()) = world.with_chunk_read(coord, |_| ()) else {
            return;
        };

        self.hydrate_level_bin(level_name);
        let from_bin = self.field.read_map().has_chunk(coord);

        if !from_bin {
            let seed = dirt_chunk_seed(coord);
            let mut rng = StdRng::seed_from_u64(seed);

            world.with_chunk_read(coord, |wchunk| {
                self.field.inner().with_chunk_write(coord, |dchunk| {
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
                            dchunk.set_local(local, level);
                        }
                    }
                });
            });
            self.field.flush_if_pending();
        }

        self.seeded_chunks
            .lock()
            .expect("dirt seeded_chunks lock poisoned")
            .insert(coord);
        self.mark_dirty(coord);
    }

    /// Clears procedural dirt for `coord` so [`Self::ensure_chunk_seeded`] can run again.
    pub fn reset_chunk_for_regeneration(&self, coord: ChunkCoord) {
        self.seeded_chunks
            .lock()
            .expect("dirt seeded_chunks lock poisoned")
            .remove(&coord);
        self.field.reset_chunk(coord);
    }
}

pub struct DirtMapPlugin;

impl Plugin for DirtMapPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(DirtMap {
            field: TileFieldMap::new(0.0, DIRT_CLAMP_MAX),
            seeded_chunks: Mutex::new(HashSet::new()),
            hydrated_level: Mutex::new(None),
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
    dirt.field.flush_if_pending();
}

pub(crate) fn seed_dirt_for_visible_chunks(
    runtime: Res<HypermapRuntime>,
    dirt: Res<DirtMap>,
    level: Res<LevelName>,
) {
    for coord in runtime.desired_chunk_coords() {
        dirt.ensure_chunk_seeded(&runtime.map, coord, level.0.as_str());
    }
}

fn dirt_chunk_seed(_coord: ChunkCoord) -> u64 {
    random_rng_seed()
}
