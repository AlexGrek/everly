//! Ground-floor **tile-resolution** dirt (`f32` per world tile, `0.0..=1.0`).
//!
//! See `docs/field-interactions.md` for actor deposits and `docs/chunk-overlay.md` for rendering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use bevy::prelude::*;
use crate::hud::perf_timings::{SystemTimings, TimedSystem};
use crate::rng;

use crate::map::hypermap::{random_rng_seed, ChunkCoord, Hypermap, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_pathfind::passability_walkable;
use crate::map::level::LevelName;
use crate::map::tile_field_level::{dirt_bin_path, load_tile_field_bin};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::tile_field::TileFieldMap;
use crate::map::world_map::CellType;
use crate::menu::main_menu::GameState;

pub use crate::map::tile_field::TILE_FIELD_OVERLAY_RES as DIRT_OVERLAY_RES;

const DIRT_CLAMP_MAX: f32 = 1.0;

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
        passability: &Hypermap<f32>,
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
            let mut rng = rng::seeded(seed);

            let chunk_origin_x = coord.x * HYPERMAP_CHUNK_SIZE;
            let chunk_origin_y = coord.y * HYPERMAP_CHUNK_SIZE;

            // Collect all passable tile positions within this chunk.
            let mut passable_tiles: Vec<(i32, i32)> = Vec::new();
            for ly in 0..HYPERMAP_CHUNK_SIZE {
                for lx in 0..HYPERMAP_CHUNK_SIZE {
                    let wx = chunk_origin_x + lx;
                    let wy = chunk_origin_y + ly;
                    if passability_walkable(passability.get(wx, wy)) {
                        passable_tiles.push((wx, wy));
                    }
                }
            }

            if passable_tiles.len() >= 2 {
                // Each hotspot BFS-floods outward with a damped cosine wave:
                // strong peak at center, a dead ring at ~WAVE_HALF_PERIOD tiles,
                // a faint secondary ring beyond that, then gone. Walls block spread.
                const HOTSPOT_COUNT: usize = 200;
                const HOTSPOT_INTENSITY: f32 = 0.35;
                const WAVE_DECAY: f32 = 0.2;       // exponential amplitude decay per tile
                const WAVE_HALF_PERIOD: f32 = 4.0; // cosine gap at this BFS distance
                const MAX_RADIUS: u32 = 20;

                let count = HOTSPOT_COUNT.min(passable_tiles.len());
                let mut hit_counts: HashMap<(i32, i32), f32> = HashMap::new();

                for _ in 0..count {
                    let center = *rng::pick(&mut rng, &passable_tiles);
                    let mut queue: VecDeque<((i32, i32), u32)> = VecDeque::new();
                    let mut visited: HashSet<(i32, i32)> = HashSet::new();
                    queue.push_back((center, 0));
                    visited.insert(center);

                    while let Some((tile, dist)) = queue.pop_front() {
                        let d = dist as f32;
                        let wave = (0.5 + 0.5 * (d * std::f32::consts::PI / WAVE_HALF_PERIOD).cos())
                            * (-d * WAVE_DECAY).exp();
                        *hit_counts.entry(tile).or_insert(0.0) += HOTSPOT_INTENSITY * wave;

                        if dist < MAX_RADIUS {
                            for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                                let neighbor = (tile.0 + dx, tile.1 + dy);
                                if !visited.contains(&neighbor)
                                    && passability_walkable(passability.get(neighbor.0, neighbor.1))
                                {
                                    visited.insert(neighbor);
                                    queue.push_back((neighbor, dist + 1));
                                }
                            }
                        }
                    }
                }

                // Write dirt only for tiles within this chunk.
                self.field.inner().with_chunk_write(coord, |dchunk| {
                    for ly in 0..HYPERMAP_CHUNK_SIZE {
                        for lx in 0..HYPERMAP_CHUNK_SIZE {
                            let wx = chunk_origin_x + lx;
                            let wy = chunk_origin_y + ly;
                            if let Some(&dirt) = hit_counts.get(&(wx, wy)) {
                                let local = LocalCoord::new(lx, ly);
                                dchunk.set_local(local, dirt.min(1.0));
                            }
                        }
                    }
                });
                self.field.flush_if_pending();
            }
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

pub(crate) fn flush_dirt_map(dirt: Res<DirtMap>, timings: Res<SystemTimings>) {
    let _t = timings.scope(TimedSystem::DirtFlush);
    dirt.field.flush_if_pending();
}

pub(crate) fn seed_dirt_for_visible_chunks(
    runtime: Res<HypermapRuntime>,
    dirt: Res<DirtMap>,
    level: Res<LevelName>,
) {
    for coord in runtime.desired_chunk_coords() {
        dirt.ensure_chunk_seeded(&runtime.map, &runtime.static_passability_map, coord, level.0.as_str());
    }
}

fn dirt_chunk_seed(_coord: ChunkCoord) -> u64 {
    random_rng_seed()
}
