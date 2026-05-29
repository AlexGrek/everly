//! World map: data structures, text format, runtime rendering, pathfinding.
//!
//! `hypermap` is the generic chunked tile store. `world_map` parses the
//! `.txt` format and defines `CellType` / wall masks. `hypermap_world`
//! is the renderer that spawns chunk meshes around the camera, and
//! `hypermap_pathfind` runs A* over the same data. `floor_level` owns
//! the active vertical level shared by camera, HUD, and renderer. `level`
//! stores on-disk geometry under `levels/level_{name}/geometry/`.

pub mod chunk_metadata;
pub mod chunk_overlay;
pub mod dirt;
pub mod dirt_overlay;
pub mod field_interactions;
pub mod temperature;
pub mod temperature_overlay;
pub mod tile_field;
pub mod tile_field_level;
pub mod floor_level;
pub mod hypermap;
pub mod interactive_entity;
pub mod level;
pub mod map_generator;
pub mod hypermap_pathfind;
pub mod hypermap_world;
pub mod passability;
pub mod world_map;
