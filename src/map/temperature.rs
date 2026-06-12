//! Ground-floor **tile-resolution** temperature in degrees Celsius.

use std::collections::HashSet;
use std::sync::Mutex;

use bevy::prelude::*;
use crate::hud::perf_timings::{SystemTimings, TimedSystem};
use crate::rng;

use crate::map::hypermap::{random_rng_seed, ChunkCoord, Hypermap, LocalCoord, HYPERMAP_CHUNK_SIZE};
use crate::map::level::LevelName;
use crate::map::tile_field_level::{load_tile_field_bin, temperature_bin_path};
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::tile_field::TileFieldMap;
use crate::map::world_map::CellType;
use crate::menu::main_menu::GameState;

pub use crate::map::tile_field::TILE_FIELD_OVERLAY_RES as TEMPERATURE_OVERLAY_RES;

/// Colormap cold end (°C) — rendered blue.
pub const TEMP_MIN_C: f32 = -30.0;
/// Neutral (°C) — rendered white.
pub const TEMP_ZERO_C: f32 = 0.0;
/// Colormap hot end (°C) — rendered red (via yellow).
pub const TEMP_MAX_C: f32 = 30.0;

/// How often standing bots heat their current main tile (see `field_interactions`).
pub const BOT_OCCUPANCY_HEAT_INTERVAL_S: f32 = 1.0;
/// °C added per interval to each main tile occupied by at least one bot.
pub const BOT_OCCUPANCY_HEAT_DELTA_C: f32 = 3.0;

const COLD_PATCH_CHANCE: f32 = 0.04;
const WARM_PATCH_CHANCE: f32 = 0.04;
/// Maps stored °C to heatmap RGBA: blue (−30) → white (0) → yellow → red (+30).
pub fn temperature_celsius_to_rgba(celsius: f32) -> [u8; 4] {
    let c = celsius.clamp(TEMP_MIN_C, TEMP_MAX_C);
    let (r, g, b) = if c <= TEMP_ZERO_C {
        let t = (c - TEMP_MIN_C) / (TEMP_ZERO_C - TEMP_MIN_C);
        lerp3([40.0, 80.0, 255.0], [255.0, 255.0, 255.0], t)
    } else {
        let t = c / TEMP_MAX_C;
        if t <= 0.5 {
            lerp3([255.0, 255.0, 255.0], [255.0, 230.0, 40.0], t * 2.0)
        } else {
            lerp3([255.0, 230.0, 40.0], [255.0, 40.0, 30.0], (t - 0.5) * 2.0)
        }
    };
    [r.round() as u8, g.round() as u8, b.round() as u8, 235]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> (f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    (
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    )
}

/// Tile-resolution temperature hypermap (double-buffered), values in °C.
#[derive(Resource)]
pub struct TemperatureMap {
    field: TileFieldMap,
    seeded_chunks: Mutex<HashSet<ChunkCoord>>,
    hydrated_level: Mutex<Option<String>>,
}

impl TemperatureMap {
    pub fn field(&self) -> &TileFieldMap {
        &self.field
    }

    pub fn read_map(&self) -> &Hypermap<f32> {
        self.field.read_map()
    }

    pub fn get_tile_c(&self, world_x: i32, world_y: i32) -> f32 {
        self.field.get_tile(world_x, world_y)
    }

    pub fn set_tile_c(&self, world_x: i32, world_y: i32, celsius: f32) {
        self.field.set_tile(world_x, world_y, celsius);
    }

    pub fn add_tile_c(&self, world_x: i32, world_y: i32, delta_c: f32) {
        self.field.add_tile(world_x, world_y, delta_c);
    }

    pub fn mark_dirty(&self, coord: ChunkCoord) {
        self.field.mark_dirty(coord);
    }

    pub fn take_dirty_chunks(&self) -> HashSet<ChunkCoord> {
        self.field.take_dirty_chunks()
    }

    /// Pushes a GPU-diffused window back onto the CPU read buffer (source of truth).
    /// See [`TileFieldMap::apply_window_to_read`] and `src/map/temperature_diffusion.rs`.
    pub fn apply_window_readback(
        &self,
        origin_x: i32,
        origin_y: i32,
        width: usize,
        height: usize,
        data: &[f32],
    ) {
        self.field
            .apply_window_to_read(origin_x, origin_y, width, height, data);
    }

    fn hydrate_level_bin(&self, level_name: &str) {
        let mut slot = self
            .hydrated_level
            .lock()
            .expect("temperature hydrated_level lock poisoned");
        if slot.as_deref() == Some(level_name) {
            return;
        }
        *slot = Some(level_name.to_string());
        drop(slot);

        let path = temperature_bin_path(level_name);
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

        self.hydrate_level_bin(level_name);
        let from_bin = self.field.read_map().has_chunk(coord);

        if !from_bin {
            let seed = temperature_chunk_seed(coord);
            let mut rng = rng::seeded(seed);

            world.with_chunk_read(coord, |wchunk| {
                self.field.inner().with_chunk_write(coord, |tchunk| {
                    for ly in 0..HYPERMAP_CHUNK_SIZE {
                        for lx in 0..HYPERMAP_CHUNK_SIZE {
                            let local = LocalCoord::new(lx, ly);
                            let cell = *wchunk.get_local_floor(local, 0);
                            if matches!(cell, CellType::Void) {
                                continue;
                            }
                            let celsius = match rng::categorical(
                                &mut rng,
                                &[COLD_PATCH_CHANCE, COLD_PATCH_CHANCE + WARM_PATCH_CHANCE],
                            ) {
                                0 => rng::f32_in(&mut rng, -26.0, -6.0),
                                1 => rng::f32_in(&mut rng, 6.0, 26.0),
                                _ => TEMP_ZERO_C,
                            };
                            tchunk.set_local(local, celsius);
                        }
                    }
                });
            });
            self.field.flush_if_pending();
        }

        self.seeded_chunks
            .lock()
            .expect("temperature seeded_chunks lock poisoned")
            .insert(coord);
        self.mark_dirty(coord);
    }

    /// Clears procedural temperature for `coord` so [`Self::ensure_chunk_seeded`] can run again.
    pub fn reset_chunk_for_regeneration(&self, coord: ChunkCoord) {
        self.seeded_chunks
            .lock()
            .expect("temperature seeded_chunks lock poisoned")
            .remove(&coord);
        self.field.reset_chunk(coord);
    }
}

pub struct TemperatureMapPlugin;

impl Plugin for TemperatureMapPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(TemperatureMap {
            field: TileFieldMap::new_ranged(TEMP_ZERO_C, TEMP_MIN_C, TEMP_MAX_C),
            seeded_chunks: Mutex::new(HashSet::new()),
            hydrated_level: Mutex::new(None),
        })
        .add_systems(
            Update,
            (seed_temperature_for_visible_chunks, flush_temperature_map)
                .chain()
                .run_if(in_state(GameState::InGame)),
        );
    }
}

pub(crate) fn flush_temperature_map(temperature: Res<TemperatureMap>, timings: Res<SystemTimings>) {
    let _t = timings.scope(TimedSystem::TempFlush);
    temperature.field.flush_if_pending();
}

pub(crate) fn seed_temperature_for_visible_chunks(
    runtime: Res<HypermapRuntime>,
    temperature: Res<TemperatureMap>,
    level: Res<LevelName>,
) {
    for coord in runtime.desired_chunk_coords() {
        temperature.ensure_chunk_seeded(&runtime.map, coord, level.0.as_str());
    }
}

fn temperature_chunk_seed(_coord: ChunkCoord) -> u64 {
    random_rng_seed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colormap_endpoints() {
        assert_eq!(
            temperature_celsius_to_rgba(TEMP_MIN_C)[..3],
            temperature_celsius_to_rgba(-30.0)[..3]
        );
        let white = temperature_celsius_to_rgba(0.0);
        assert!(white[0] > 240 && white[1] > 240 && white[2] > 240);
        let red = temperature_celsius_to_rgba(TEMP_MAX_C);
        assert!(red[0] > 240 && red[2] < 80);
    }
}
