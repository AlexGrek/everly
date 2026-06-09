//! Public config, constants, and persisted metadata types.

use crate::map::hypermap::HYPERMAP_CHUNK_SIZE;

/// Minimum Manhattan distance between primary seed positions after separation.
pub const MIN_SEED_DISTANCE: i32 = 15;
/// Seeds closer than this to the playable border are pushed inward.
pub const BORDER_CLEARANCE: i32 = 15;
/// Void ring at chunk edges (road carpet starts inside this inset).
pub const CHUNK_VOID_MARGIN: i32 = 2;
/// Primary building seeds placed per chunk (inclusive range).
pub const PRIMARY_SEED_COUNT_MIN: i32 = 5;
pub const PRIMARY_SEED_COUNT_MAX: i32 = 8;
/// Subseed room sprouts per primary (inclusive range).
pub const SUBSEEDS_PER_PRIMARY_MIN: i32 = 2;
pub const SUBSEEDS_PER_PRIMARY_MAX: i32 = 4;
/// Minimum house footprint (1 m² cells) for the center glass floor wave.
pub const MIN_HOUSE_AREA_FOR_CENTER_WAVE: i32 = 30;

pub const GENERATED_CHUNK_METADATA_VERSION: u32 = 4;

/// Main doorway for one house (chunk-local tiles).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct HouseEntrypoint {
    /// Road tile outside the building.
    pub walk_x: i32,
    pub walk_z: i32,
    /// Wall (or former wall) cell where the door gap was cut.
    pub wall_x: i32,
    pub wall_z: i32,
    /// [`MASK_NORTH`] … [`MASK_WEST`] bit opened toward `walk_*`.
    pub outward_edge: u8,
    /// Second wall cell of a 2-wide doorway (adjacent along the wall run).
    /// `None` when only a 1-wide opening could be cut (degenerate geometry).
    #[serde(default)]
    pub wall2: Option<(i32, i32)>,
}

/// One merged building footprint plus its single entry (chunk-local tiles).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct GeneratedHouse {
    pub x0: i32,
    pub z0: i32,
    pub x1: i32,
    pub z1: i32,
    pub center_x: i32,
    pub center_z: i32,
    /// Footprint size in 1×1 m cells (union of rects, no connectivity).
    pub area: i32,
    pub entry: HouseEntrypoint,
}

/// Procedural layout reference data for one chunk (persisted as `metadata/{x}_{y}.yaml`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct GeneratedChunkMetadata {
    pub version: u32,
    pub generator_seed: u64,
    pub houses: Vec<GeneratedHouse>,
}

#[derive(Debug, Clone)]
pub struct MapGeneratorConfig {
    pub size: i32,
    pub margin: i32,
    pub seed: u64,
}

impl Default for MapGeneratorConfig {
    fn default() -> Self {
        Self {
            size: HYPERMAP_CHUNK_SIZE,
            margin: CHUNK_VOID_MARGIN,
            seed: 0xE0E1_700D,
        }
    }
}
