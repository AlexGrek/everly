# Hypermap Runtime Behavior

This document describes the current runtime behavior of `HypermapWorldPlugin`.

## Core Model

- World space is infinite and addressed with signed integer coordinates.
- Data is chunked into fixed `128x128` chunks.
- Chunks are lazily generated on first access.
- Once generated, chunk data remains in memory (no eviction).
- **Vertical:** each column stores **10 floors** (`0..=9`). **1 world unit = 1 m**; see `docs/tilemap.md` § World units for wall thickness, wall height (`HYPERMAP_WALL_HEIGHT`), and storey spacing (`HYPERMAP_FLOOR_HEIGHT`) in `src/map/floor_level.rs`.
- **HUD floor level** chooses which floors are drawn (active floor and above; floor 0 visibility rules differ when the active level is 0 versus a higher floor — see `HypermapWorldPlugin` / `ActiveFloorLevel`).

## Generation

- New chunks are generated synchronously when the camera needs them (see **Visibility** below).
- If `levels/level_{name}/geometry/{x}_{y}.txt` is missing or invalid, the chunk is filled
  with the procedural map generator ([`map-generator.md`](map-generator.md)) using a fresh
  random seed (in memory only until the map editor **Save** button).
- Chunk `(0,0)` receives `world_map.txt` / `world_map_floor1.txt` overlays **only when**
  procedurally generated (no geometry file on disk yet).

## Level geometry on disk

`ensure_chunk_generated` tries `levels/level_{name}/geometry/{chunk_x}_{chunk_y}.txt` first.
If the file exists and parses, the chunk is loaded from disk. Otherwise the procedural
generator runs (random seed). Persist geometry, styles, dirt, temperature, actors, and
camera with the map editor **Save** button — see [`level-persistence.md`](level-persistence.md).

## Static Passability Mirror

`HypermapRuntime` carries a second hypermap, **`static_passability_map: Hypermap<f32>`**, that
shadows the world `Hypermap<CellType>`. Every cell stores the value of
`cell_passability(CellType)` (`1.0` for `Road`, `0.0` for `Void` / `Wall` / `Corner`).

- Default tile `1.0` so unallocated chunks read as walkable, mirroring the world map's `Road` default.
- Populated whenever `ensure_chunk_generated` finishes a world chunk: every `(x, y, floor)` is
  derived from the freshly written world cell.
- Edits go through `write_world_cell(runtime, x, y, floor, cell)`, which sets the world cell
  and the passability cell in lock-step. Edit systems must use this helper instead of
  touching `runtime.map` directly so the two maps cannot drift.
- Hypermap pathfinding (`astar_shortest_world_path`, `explore_walkable_tiles_limited` in
  `src/map/hypermap_pathfind.rs`) reads only from `static_passability_map`. A tile is walkable
  iff its sample is `> 0.0`.
- "Static" because no runtime obstacles (units, doors, etc.) participate — only the authored /
  procedural geometry. Dynamic obstacles use the separate `DynamicPassabilityMap` below.

## Dynamic Passability Map

`DynamicPassabilityMap` (resource, `src/map/passability.rs`) stores a
**`DoubleBufferedHypermap<SubtilePassability>`** for runtime obstacles.

- Each world tile is subdivided into a **5×5 micro-grid** of booleans
  (`SubtilePassability`; `true` = passable, `false` = blocked).
- Default tile is `ALL_PASSABLE` — unallocated chunks are fully walkable.
- Uses a **double-buffered** hypermap (`DoubleBufferedHypermap` in
  `src/map/hypermap.rs`): reads hit the **read** buffer; writes hit the
  **write** buffer. Calling `flush()` atomically promotes write→read and
  resets the write buffer to clean state (all chunks dropped, default tile
  returns).
- **Not yet wired into pathfinding.** The data store is ready for future
  integration.

## Visibility window (3 chunks)

Exactly **three** chunks are targeted for rendering at any time:

1. The chunk under the camera focus
2. One neighbor on the **X** axis — east if focus local `x ≥ 64`, else west
3. One neighbor on the **Y** axis — north if focus local `y ≥ 64`, else south

(World `x` / `z` map to chunk `x` / `y` via `world_to_chunk_local`.)

## Dead zone

- A `20×20` cell region centered in the current chunk acts as a change-free zone.
- While the camera stays inside it **and** the center chunk is unchanged, the
  three-chunk target set is not recomputed (avoids flicker when crossing the
  chunk midline).
- Leaving the dead zone toward a border updates which side chunk is prefetched.

## Water Rule

- A water plane is spawned only when floor `0` contains at least one `VOID` cell
  **strictly inside** an inset of `WATER_MESH_EDGE_STRIP` cells from each chunk
  edge (`2`, same as [`CHUNK_VOID_MARGIN`](../../src/map/map_generator/types.rs) /
  `WATER_MESH_EDGE_STRIP` in `src/map/hypermap_world.rs`). Procedural fill uses a
  **void** margin ring; only interior `Open` / room tiles are road — so water
  appears only when authored geometry or future generator steps place interior void.
- The water mesh is sized to that **interior** square only, so water never covers
  the chunk border band even when interior void triggers it.

## Concurrency and Locking

- Chunk map storage is concurrent (per-chunk lock handles).
- Render prep uses a cloned snapshot of chunk cells at task start.
- Rendering work does not hold chunk locks.
