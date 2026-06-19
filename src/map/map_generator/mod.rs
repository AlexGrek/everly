//! Procedural single-chunk map generator (seeds → subseed rooms → union shell → door).
//!
//! Generation runs on a [`MapDraft`] intermediate grid. Each pipeline step mutates
//! that draft; [`MapDraft::finish`] is the only place [`CellType`] tiles are written.
//! See `docs/map-generator.md`.

mod corner_pillars;
mod draft;
pub mod grid_fill;
mod house;
mod step_chunk_connectors;
mod step_carpet;
mod step_charging_stations;
mod step_parts_depot;
mod step_place_lamps;
mod step_ponds;
mod step_corners;
mod step_door;
mod step_home_crawler;
mod step_houses;
mod step_inner_doors;
mod step_inner_walls;
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
    ChunkRoadConnectors, GeneratedChunkMetadata, GeneratedHouse, HouseEntrypoint,
    MapGeneratorConfig, RoadConnector, BORDER_CLEARANCE, CHUNK_CONNECTOR_WIDTH_MAX,
    CHUNK_CONNECTOR_WIDTH_MIN, CHUNK_VOID_MARGIN, CONNECTORS_PER_SIDE_MAX,
    CONNECTORS_PER_SIDE_MIN, GENERATED_CHUNK_METADATA_VERSION, MIN_ROOM_AREA, MIN_ROOM_DIM,
    MIN_SEED_DISTANCE,
};

use draft::{DraftTile, Room};
use house::House;

use crate::map::hypermap::{random_rng_seed, ChunkCoord, Hypermap, HypermapChunk};
use crate::map::level::encode_chunk_geometry;
use crate::map::world_map::{CellType, LampDecoration, TileStyle};

/// Smallest boundary side (in cells) the editor "House" tool accepts.
pub const MIN_HOUSE_TOOL_SIDE: i32 = MIN_ROOM_DIM * 4;
/// Exterior road ring kept around a tool-generated house so doors can open
/// onto road on every side (matches the procedural carpet that surrounds
/// houses in a full chunk).
const HOUSE_TOOL_PAD: i32 = 4;

impl MapDraft {
    /// Runs the full pipeline and returns finished floor-0 tiles.
    pub fn generate(config: MapGeneratorConfig) -> Vec<Vec<CellType>> {
        let mut draft = Self::new(config);
        draft.run_pipeline();
        draft.finish()
    }

    pub(crate) fn run_pipeline(&mut self) {
        self.step_init_carpet();
        self.step_place_primary_seeds();
        self.step_separate_primary_seeds();
        self.step_spawn_subseeds();
        self.step_grow_rooms();
        self.step_cluster_houses();
        self.build_house_structures();
        self.step_place_ponds();
        self.step_stamp_chunk_connectors();
    }

    /// Turns already-populated `self.houses` (sitting on an `Open` carpet) into
    /// finished buildings: outer shell walls, inner corner pillars, one or two doors per
    /// house, inner room walls + doors, floor-style crawlers, and chargers.
    ///
    /// Shared by the procedural pipeline (after seed clustering) and the editor
    /// "House" tool (a single hand-placed footprint) so both paths produce
    /// identical building geometry.
    pub(crate) fn build_house_structures(&mut self) {
        self.step_paint_union_interior();
        self.step_build_union_outer_walls();
        self.step_stamp_union_inner_corner_pillars();
        self.step_place_house_doors();
        self.step_split_houses_into_rooms();
        self.step_place_inner_doors();
        self.step_home_crawlers();
        self.step_place_charging_stations();
        self.step_place_parts_depots();
        self.step_place_lamps();
    }

    fn run_into_chunk(
        mut self,
        chunk: &mut HypermapChunk<CellType>,
        map: &Hypermap<CellType>,
        style_floor_map: &Hypermap<TileStyle>,
        decoration_lamp_map: &Hypermap<LampDecoration>,
        coord: ChunkCoord,
    ) -> GeneratedChunkMetadata {
        self.connector_plan = step_chunk_connectors::plan_chunk_connectors(
            map,
            coord,
            self.margin,
            self.size,
            self.generator_seed,
        );
        self.run_pipeline();
        let meta = self.build_metadata();
        style_floor_map.with_chunk_write(coord, |style_chunk| {
            decoration_lamp_map.with_chunk_write(coord, |lamp_chunk| {
                self.write_chunk_floor0_and_styles(chunk, style_chunk, lamp_chunk);
            });
        });
        meta
    }
}

pub(crate) fn fill_procedural_chunk(
    chunk: &mut HypermapChunk<CellType>,
    map: &Hypermap<CellType>,
    style_floor_map: &Hypermap<TileStyle>,
    decoration_lamp_map: &Hypermap<LampDecoration>,
    coord: ChunkCoord,
    metadata: &mut crate::map::chunk_metadata::ChunkGeneratorMetadata,
) -> GeneratedChunkMetadata {
    let config = MapGeneratorConfig {
        seed: random_rng_seed(),
        ..Default::default()
    };
    let meta = MapDraft::new(config).run_into_chunk(
        chunk,
        map,
        style_floor_map,
        decoration_lamp_map,
        coord,
    );
    metadata.insert(coord, meta.clone());
    meta
}

/// One building generated to fill a chosen boundary rectangle (editor "House" tool).
///
/// `cells[z][x]` / `floor_styles[z][x]` are row-major over the boundary rectangle;
/// the caller offsets `(x, z)` by the boundary's min corner to reach world tiles.
pub struct HouseToolTiles {
    pub width: i32,
    pub height: i32,
    pub cells: Vec<Vec<CellType>>,
    pub floor_styles: Vec<Vec<TileStyle>>,
}

/// Generates a single building filling a `width × height` boundary using the same
/// steps as the procedural generator (outer shell, door, inner rooms/doors, floor
/// waves, charger). Returns `None` when either side is below [`MIN_HOUSE_TOOL_SIDE`].
///
/// The result spans exactly the boundary rectangle; the surrounding road context the
/// generator needs for door placement is internal padding and is not returned.
pub fn generate_house_tiles(width: i32, height: i32, seed: u64) -> Option<HouseToolTiles> {
    if width < MIN_HOUSE_TOOL_SIDE || height < MIN_HOUSE_TOOL_SIDE {
        return None;
    }

    let pad = HOUSE_TOOL_PAD;
    let size = width.max(height) + pad * 2;
    let mut draft = MapDraft::new(MapGeneratorConfig {
        size,
        margin: CHUNK_VOID_MARGIN,
        seed,
    });

    // Surround the footprint with open road so doors find exterior road on any side.
    for z in 0..size {
        for x in 0..size {
            draft.set(x, z, DraftTile::Open);
        }
    }

    let rect = Room {
        x0: pad,
        z0: pad,
        x1: pad + width - 1,
        z1: pad + height - 1,
    };
    draft.houses = vec![House::from_single_rect(rect)];
    draft.build_house_structures();

    let styles = std::mem::take(&mut draft.floor_styles);
    let cells = draft.finish();

    let mut out_cells = vec![vec![CellType::Void; width as usize]; height as usize];
    let mut out_styles = vec![vec![TileStyle::DEFAULT; width as usize]; height as usize];
    for lz in 0..height as usize {
        for lx in 0..width as usize {
            let sx = pad as usize + lx;
            let sz = pad as usize + lz;
            out_cells[lz][lx] = cells[sz][sx];
            out_styles[lz][lx] = styles[sz][sx];
        }
    }

    Some(HouseToolTiles {
        width,
        height,
        cells: out_cells,
        floor_styles: out_styles,
    })
}

/// Builds one floor-0 chunk and returns level geometry text (`# floor 0` …).
pub fn generate_chunk_geometry(config: &MapGeneratorConfig) -> String {
    let map = Hypermap::new(CellType::Void);
    let style_map = Hypermap::new(TileStyle::DEFAULT);
    let lamp_map = Hypermap::new(LampDecoration::None);
    map.with_chunk_write(ChunkCoord::new(0, 0), |chunk| {
        MapDraft::new(config.clone()).run_into_chunk(
            chunk,
            &map,
            &style_map,
            &lamp_map,
            ChunkCoord::new(0, 0),
        );
    });
    let chunk = map
        .get_chunk(ChunkCoord::new(0, 0))
        .expect("generator wrote origin chunk");
    encode_chunk_geometry(&chunk.read().expect("chunk lock"))
}
