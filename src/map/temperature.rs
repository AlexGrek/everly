//! Ground-floor **tile-resolution** temperature (`f32` per world tile, `0.0..=1.0`).

use std::collections::HashSet;
use std::sync::Mutex;

use bevy::prelude::*;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;

use crate::map::hypermap::{ChunkCoord, Hypermap, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::tile_field::TileFieldMap;
use crate::map::world_map::CellType;
use crate::menu::main_menu::GameState;

pub use crate::map::tile_field::TILE_FIELD_OVERLAY_RES as TEMPERATURE_OVERLAY_RES;

const TEMPERATURE_CLAMP_MAX: f32 = 1.0;

/// Probability that a non-void ground tile receives a warm patch when its chunk is first seeded.
const WARM_TILE_CHANCE: f32 = 0.05;

/// Tile-resolution temperature hypermap (double-buffered).
#[derive(Resource)]
pub struct TemperatureMap {
    field: TileFieldMap,
    seeded_chunks: Mutex<HashSet<ChunkCoord>>,
}

impl TemperatureMap {
    pub fn field(&self) -> &TileFieldMap {
        &self.field
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

    pub fn add_tile(&self, world_x: i32, world_y: i32, delta: f32) {
        self.field.add_tile(world_x, world_y, delta);
    }

    pub fn mark_dirty(&self, coord: ChunkCoord) {
        self.field.mark_dirty(coord);
    }

    pub fn take_dirty_chunks(&self) -> HashSet<ChunkCoord> {
        self.field.take_dirty_chunks()
    }

    pub fn ensure_chunk_seeded(&self, world: &Hypermap<CellType>, coord: ChunkCoord) {
        {
            let seeded = self
                .seeded_chunks
                .lock()
                .expect("temperature seeded_chunks lock poisoned");
            if seeded.contains(&coord) {
                return;
            }
        }

        let Some(()) = world.with_chunk_read(coord, |_| ()) else {
            return;
        };

        let seed = temperature_chunk_seed(coord);
        let mut rng = StdRng::seed_from_u64(seed);

        world.with_chunk_read(coord, |wchunk| {
            self.field.inner().with_chunk_write(coord, |tchunk| {
                for ly in 0..HYPERMAP_CHUNK_SIZE {
                    for lx in 0..HYPERMAP_CHUNK_SIZE {
                        let local = LocalCoord::new(lx, ly);
                        let cell = *wchunk.get_local_floor(local, 0);
                        if matches!(cell, CellType::Void) {
                            continue;
                        }
                        if rng.gen_range(0.0..1.0) >= WARM_TILE_CHANCE {
                            continue;
                        }
                        let level = rng.gen_range(0.15..=0.4);
                        tchunk.set_local(local, level);
                    }
                }
            });
        });

        self.seeded_chunks
            .lock()
            .expect("temperature seeded_chunks lock poisoned")
            .insert(coord);
        self.mark_dirty(coord);
    }
}

pub struct TemperatureMapPlugin;

impl Plugin for TemperatureMapPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TemperatureMap {
            field: TileFieldMap::new(0.0, TEMPERATURE_CLAMP_MAX),
            seeded_chunks: Mutex::new(HashSet::new()),
        })
        .add_systems(
            Update,
            (seed_temperature_for_visible_chunks, flush_temperature_map)
                .chain()
                .run_if(in_state(GameState::InGame)),
        );
    }
}

pub(crate) fn flush_temperature_map(temperature: Res<TemperatureMap>) {
    temperature.field.flush_if_pending();
}

pub(crate) fn seed_temperature_for_visible_chunks(
    runtime: Res<HypermapRuntime>,
    temperature: Res<TemperatureMap>,
) {
    for coord in runtime.desired_chunk_coords() {
        temperature.ensure_chunk_seeded(&runtime.map, coord);
    }
}

fn temperature_chunk_seed(coord: ChunkCoord) -> u64 {
    let x = coord.x as u64;
    let y = coord.y as u64;
    x.wrapping_mul(0x9E37_79B9_85F3_7D87)
        ^ y.wrapping_mul(0xC2B2_AE3D_27D4_F4F5)
        ^ 0x7E4D_0000_0002
}
