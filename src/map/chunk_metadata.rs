//! Procedural chunk metadata: per-house centers, entries, save/load.
//!
//! Written to `levels/level_{name}/metadata/{chunk_x}_{chunk_y}.json` on Save when
//! the chunk was procedurally generated this session. See `docs/map-generator.md`.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::map::hypermap::{ChunkCoord, HYPERMAP_CHUNK_SIZE};
pub use crate::map::map_generator::{
    GeneratedChunkMetadata, GeneratedHouse, HouseEntrypoint, GENERATED_CHUNK_METADATA_VERSION,
};

/// In-memory metadata for procedurally generated chunks (chunk-local tile coordinates).
/// Stored on [`crate::map::hypermap_world::HypermapRuntime::procedural_metadata`].
#[derive(Debug, Default, Clone)]
pub struct ChunkGeneratorMetadata {
    pub by_chunk: HashMap<ChunkCoord, GeneratedChunkMetadata>,
}

impl ChunkGeneratorMetadata {
    pub fn insert(&mut self, coord: ChunkCoord, meta: GeneratedChunkMetadata) {
        self.by_chunk.insert(coord, meta);
    }

    pub fn get(&self, coord: ChunkCoord) -> Option<&GeneratedChunkMetadata> {
        self.by_chunk.get(&coord)
    }

    pub fn remove(&mut self, coord: ChunkCoord) {
        self.by_chunk.remove(&coord);
    }
}

impl GeneratedChunkMetadata {
    /// First house main entry as world tile coordinates (road tile outside the door).
    pub fn entrypoint_world(&self, chunk: ChunkCoord) -> Option<(i32, i32)> {
        self.houses
            .first()
            .map(|h| h.entry.walk_world(chunk))
    }

    /// Main entry for one house in world tile coordinates.
    pub fn house_entry_world(&self, house_index: usize, chunk: ChunkCoord) -> Option<(i32, i32)> {
        self.houses
            .get(house_index)
            .map(|h| h.entry.walk_world(chunk))
    }

    /// House center in world tile coordinates.
    pub fn house_center_world(&self, house_index: usize, chunk: ChunkCoord) -> Option<(i32, i32)> {
        self.houses
            .get(house_index)
            .map(|h| h.center_world(chunk))
    }

    /// Deprecated alias for [`Self::house_center_world`].
    pub fn room_center_world(&self, house_index: usize, chunk: ChunkCoord) -> Option<(i32, i32)> {
        self.house_center_world(house_index, chunk)
    }
}

impl GeneratedHouse {
    pub fn center_world(&self, chunk: ChunkCoord) -> (i32, i32) {
        local_tile_to_world(chunk, self.center_x, self.center_z)
    }
}

impl HouseEntrypoint {
    /// Road tile just outside the doorway, in world tiles.
    pub fn walk_world(&self, chunk: ChunkCoord) -> (i32, i32) {
        local_tile_to_world(chunk, self.walk_x, self.walk_z)
    }

    pub fn wall_world(&self, chunk: ChunkCoord) -> (i32, i32) {
        local_tile_to_world(chunk, self.wall_x, self.wall_z)
    }
}

#[inline]
pub fn local_tile_to_world(chunk: ChunkCoord, local_x: i32, local_z: i32) -> (i32, i32) {
    (
        chunk.x * HYPERMAP_CHUNK_SIZE + local_x,
        chunk.y * HYPERMAP_CHUNK_SIZE + local_z,
    )
}

pub fn metadata_dir(level_name: &str) -> PathBuf {
    PathBuf::from("levels")
        .join(format!("level_{level_name}"))
        .join("metadata")
}

pub fn chunk_metadata_path(level_name: &str, coord: ChunkCoord) -> PathBuf {
    metadata_dir(level_name).join(format!("{}_{}.json", coord.x, coord.y))
}

pub fn try_load_chunk_metadata(
    level_name: &str,
    coord: ChunkCoord,
) -> io::Result<Option<GeneratedChunkMetadata>> {
    let path = chunk_metadata_path(level_name, coord);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let meta: GeneratedChunkMetadata = serde_json::from_str(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(meta))
}

pub fn save_chunk_metadata(level_name: &str, coord: ChunkCoord, meta: &GeneratedChunkMetadata) -> io::Result<()> {
    let dir = metadata_dir(level_name);
    fs::create_dir_all(&dir)?;
    let path = chunk_metadata_path(level_name, coord);
    let text = serde_json::to_string_pretty(meta)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, text)
}

/// Saves metadata JSON for every chunk present in `store` that also exists in `cell_map`.
pub fn save_level_chunk_metadata(
    level_name: &str,
    store: &ChunkGeneratorMetadata,
    cell_map: &crate::map::hypermap::Hypermap<crate::map::world_map::CellType>,
) -> io::Result<usize> {
    let mut count = 0usize;
    for coord in cell_map.loaded_chunks() {
        let Some(meta) = store.get(coord) else {
            continue;
        };
        save_chunk_metadata(level_name, coord, meta)?;
        count += 1;
    }
    Ok(count)
}
