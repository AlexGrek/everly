//! World map: data structures, text format, runtime rendering, pathfinding.
//!
//! `hypermap` is the generic chunked tile store. `world_map` parses the
//! `.txt` format and defines `CellType` / wall masks. `hypermap_world`
//! is the renderer that spawns chunk meshes around the camera, and
//! `hypermap_pathfind` runs A* over the same data. `floor_level` owns
//! the active vertical level shared by camera, HUD, and renderer. `level`
//! stores on-disk geometry under `levels/level_{name}/geometry/`.

pub mod floor_level;
pub mod hypermap;
pub mod level;
pub mod hypermap_pathfind;
pub mod hypermap_world;
pub mod world_map;
