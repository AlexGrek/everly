//! Working grid (`MapDraft`) and geometry helpers used across pipeline steps.

use crate::rng::{self, StdRng};

use crate::map::hypermap::{HypermapChunk, LocalCoord};
use crate::map::world_map::{CellType, ChargerFacing, TileStyle, WallCorner, WallMask};

use super::house::House;
use super::types::{GeneratedChunkMetadata, MapGeneratorConfig};
use super::types::{BORDER_CLEARANCE, GENERATED_CHUNK_METADATA_VERSION};

/// In-bounds ranges for seed placement and room growth (derived from config).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DraftBounds {
    pub place_lo: i32,
    pub place_hi: i32,
    pub grow_lo: i32,
    pub grow_hi: i32,
}

impl DraftBounds {
    pub fn from_config(config: &MapGeneratorConfig) -> Self {
        let m = config.margin;
        let sz = config.size;
        Self {
            place_lo: m + BORDER_CLEARANCE,
            place_hi: sz - m - BORDER_CLEARANCE - 1,
            grow_lo: m + 1,
            grow_hi: sz - m - 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Room {
    pub x0: i32,
    pub z0: i32,
    pub x1: i32,
    pub z1: i32,
}

impl Room {
    pub fn contains(&self, x: i32, z: i32) -> bool {
        x >= self.x0 && x <= self.x1 && z >= self.z0 && z <= self.z1
    }

}


#[derive(Debug, Clone, Copy)]
pub(crate) struct RoomRecord {
    pub bounds: Room,
}

/// Logical cell while the map is still being built (not yet [`CellType`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DraftTile {
    #[default]
    Void,
    Open,
    Wall(u8),
    Corner(WallCorner),
    /// Walkable charging station backing onto the named wall (see [`ChargerFacing`]).
    Charger(ChargerFacing),
}

/// Working map: all pipeline steps read and write this structure.
#[derive(Debug)]
pub struct MapDraft {
    pub(crate) size: i32,
    pub(crate) margin: i32,
    pub(crate) bounds: DraftBounds,
    pub(crate) rng: StdRng,
    pub(crate) cells: Vec<Vec<DraftTile>>,
    pub(crate) floor_styles: Vec<Vec<TileStyle>>,
    pub(crate) primary_seeds: Vec<(i32, i32)>,
    pub(crate) growth_centers: Vec<(i32, i32)>,
    /// Centers added beside primaries; rooms grow here only.
    pub(crate) subseed_centers: Vec<(i32, i32)>,
    pub(crate) room_records: Vec<RoomRecord>,
    pub(crate) houses: Vec<House>,
    pub(crate) generator_seed: u64,
}

impl MapDraft {
    pub fn new(config: MapGeneratorConfig) -> Self {
        let size = config.size;
        let margin = config.margin;
        let bounds = DraftBounds::from_config(&config);
        let sz = size as usize;
        Self {
            size,
            margin,
            bounds,
            rng: rng::seeded(config.seed),
            cells: vec![vec![DraftTile::Void; sz]; sz],
            floor_styles: vec![vec![TileStyle::DEFAULT; sz]; sz],
            primary_seeds: Vec::new(),
            growth_centers: Vec::new(),
            subseed_centers: Vec::new(),
            room_records: Vec::new(),
            houses: Vec::new(),
            generator_seed: config.seed,
        }
    }

    pub(crate) fn rooms(&self) -> Vec<Room> {
        super::house::all_house_rects(self)
    }

    pub fn build_metadata(&self) -> GeneratedChunkMetadata {
        GeneratedChunkMetadata {
            version: GENERATED_CHUNK_METADATA_VERSION,
            generator_seed: self.generator_seed,
            houses: self
                .houses
                .iter()
                .map(|h| h.to_generated())
                .collect(),
        }
    }

    /// Converts the draft grid into runtime [`CellType`] tiles.
    pub fn finish(self) -> Vec<Vec<CellType>> {
        let sz = self.size as usize;
        let mut out = vec![vec![CellType::Void; sz]; sz];
        for z in 0..sz {
            for x in 0..sz {
                out[z][x] = draft_tile_to_cell(self.cells[z][x]);
            }
        }
        out
    }

    pub fn write_chunk_floor0(self, chunk: &mut HypermapChunk<CellType>) {
        let size = self.size;
        let cells = self.finish();
        for z in 0..size {
            for x in 0..size {
                chunk.set_local(LocalCoord::new(x, z), cells[z as usize][x as usize]);
            }
        }
    }

    pub fn write_chunk_floor0_and_styles(
        mut self,
        chunk: &mut HypermapChunk<CellType>,
        style_chunk: &mut HypermapChunk<TileStyle>,
    ) {
        let size = self.size as usize;
        let styles = std::mem::take(&mut self.floor_styles);
        let cells = self.finish();
        for z in 0..size {
            for x in 0..size {
                let local = LocalCoord::new(x as i32, z as i32);
                chunk.set_local(local, cells[z][x]);
                style_chunk.set_local(local, styles[z][x]);
            }
        }
    }

    pub(crate) fn set_floor_style(&mut self, x: i32, z: i32, style: TileStyle) {
        self.floor_styles[z as usize][x as usize] = style;
    }

    pub(crate) fn get(&self, x: i32, z: i32) -> DraftTile {
        self.cells[z as usize][x as usize]
    }

    pub(crate) fn set(&mut self, x: i32, z: i32, tile: DraftTile) {
        self.cells[z as usize][x as usize] = tile;
    }

    pub(crate) fn in_bounds(&self, x: i32, z: i32) -> bool {
        x >= 0 && z >= 0 && x < self.size && z < self.size
    }
}

pub(crate) fn draft_tile_to_cell(tile: DraftTile) -> CellType {
    match tile {
        DraftTile::Void => CellType::Void,
        DraftTile::Open => CellType::Road,
        DraftTile::Wall(bits) => CellType::Wall(WallMask::from_bits(bits).expect("wall mask")),
        DraftTile::Corner(c) => CellType::Corner(c),
        DraftTile::Charger(facing) => CellType::Charger(facing),
    }
}
