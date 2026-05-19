//! Procedural single-chunk map generator (seeds → subseed rooms → union shell → door).
//!
//! Generation runs on a [`MapDraft`] intermediate grid. Each pipeline step mutates
//! that draft; [`MapDraft::finish`] is the only place [`CellType`] tiles are written.
//! See `docs/map-generator.md`.

mod corner_pillars;
mod draft;
pub mod grid_fill;
mod house;
mod step_carpet;
mod step_corners;
mod step_door;
mod step_home_crawler;
mod step_houses;
mod step_rooms;
mod step_seeds;
mod step_shell;
mod types;
mod union;

#[cfg(test)]
mod tests;

pub use corner_pillars::{detect_corner_pillars, CornerPillarPlacement, WallField};
pub use draft::MapDraft;
pub use grid_fill::{count_region_area, flood_fill_area};
pub use types::{
    GeneratedChunkMetadata, GeneratedHouse, HouseEntrypoint, MapGeneratorConfig,
    BORDER_CLEARANCE, CHUNK_VOID_MARGIN, GENERATED_CHUNK_METADATA_VERSION, MIN_SEED_DISTANCE,
};

use crate::map::hypermap::{random_rng_seed, ChunkCoord, Hypermap, HypermapChunk};
use crate::map::level::encode_chunk_geometry;
use crate::map::world_map::{CellType, TileStyle};

impl MapDraft {
    /// Runs the full pipeline and returns finished floor-0 tiles.
    pub fn generate(config: MapGeneratorConfig) -> Vec<Vec<CellType>> {
        let mut draft = Self::new(config);
        draft.run_pipeline();
        draft.finish()
    }

    fn run_pipeline(&mut self) {
        self.step_init_carpet();
        self.step_place_primary_seeds();
        self.step_separate_primary_seeds();
        self.step_spawn_subseeds();
        self.step_grow_rooms();
        self.step_cluster_houses();
        self.step_paint_union_interior();
        self.step_build_union_outer_walls();
        self.step_stamp_union_inner_corner_pillars();
        self.step_place_house_doors();
        self.step_home_crawlers();
    }

    fn run_into_chunk(
        mut self,
        chunk: &mut HypermapChunk<CellType>,
        style_floor_map: &Hypermap<TileStyle>,
        coord: ChunkCoord,
    ) -> GeneratedChunkMetadata {
        self.run_pipeline();
        let meta = self.build_metadata();
        style_floor_map.with_chunk_write(coord, |style_chunk| {
            self.write_chunk_floor0_and_styles(chunk, style_chunk);
        });
        meta
    }
}

pub(crate) fn fill_procedural_chunk(
    chunk: &mut HypermapChunk<CellType>,
    style_floor_map: &Hypermap<TileStyle>,
    coord: ChunkCoord,
    metadata: &mut crate::map::chunk_metadata::ChunkGeneratorMetadata,
) -> GeneratedChunkMetadata {
    let config = MapGeneratorConfig {
        seed: random_rng_seed(),
        ..Default::default()
    };
    let meta = MapDraft::new(config).run_into_chunk(chunk, style_floor_map, coord);
    metadata.insert(coord, meta.clone());
    meta
}

/// Builds one floor-0 chunk and returns level geometry text (`# floor 0` …).
pub fn generate_chunk_geometry(config: &MapGeneratorConfig) -> String {
    let map = Hypermap::new(CellType::Void);
    let style_map = Hypermap::new(TileStyle::DEFAULT);
    map.with_chunk_write(ChunkCoord::new(0, 0), |chunk| {
        MapDraft::new(config.clone()).run_into_chunk(chunk, &style_map, ChunkCoord::new(0, 0));
    });
    let chunk = map
        .get_chunk(ChunkCoord::new(0, 0))
        .expect("generator wrote origin chunk");
    encode_chunk_geometry(&chunk.read().expect("chunk lock"))
}
