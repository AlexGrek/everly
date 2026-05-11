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

- New chunks are generated synchronously.
- Tiles are deterministic random (`ROAD` + directional `WALL`) by chunk seed.
- Chunk `(0,0)` receives an overlay from `world_map.txt` after random fill (floor **0**).
- When **`world_map_floor1.txt`** exists, the same center chunk’s **floor 1** is overwritten from that file (same rectangular size as floor 0 is recommended).

## Level geometry on disk

Before procedural fill, `ensure_chunk_generated` looks for
`levels/level_{name}/geometry/{chunk_x}_{chunk_y}.txt` (see `src/map/level.rs` and
`LevelName`, default `default`). If the file exists and parses, that chunk is filled from
disk only — **no** `world_map.txt` / `world_map_floor1.txt` overlay for that chunk. If the
file is missing or invalid, generation falls back to the procedural neighborhood plus the
center-chunk overlays described above.

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

## Visibility Window (Directional)

Exactly 4 chunks are targeted for rendering:

1. Camera current chunk
2. North of current chunk
3. West or east chunk (based on camera local X proximity)
4. North of that side chunk

South is intentionally excluded.

## Dead Zone

- A `20x20` center area inside the current chunk acts as a change-free zone.
- While camera remains in this area (and chunk unchanged), target chunk set is
  not recomputed.

## Water Rule

- A water plane is spawned only when floor `0` contains at least one `VOID` cell
  **strictly inside** an inset of `WATER_MESH_EDGE_STRIP` cells from each chunk
  edge (`2`, same as `PROCEDURAL_VOID_MARGIN` in `src/map/hypermap_world.rs`).
  Procedural chunks no longer have a void ring at all — the border band is
  always road — so chunks only get water when something authored or a
  procedural pond places interior void.
- The water mesh is sized to that **interior** square only, so water never covers
  the chunk border band even when interior void triggers it.
- Rare interior ponds (`PROCEDURAL_POND_CHUNK_CHANCE`) carve void on roads and
  still qualify for water when the void lies inside the strip inset.

## Concurrency and Locking

- Chunk map storage is concurrent (per-chunk lock handles).
- Render prep uses a cloned snapshot of chunk cells at task start.
- Rendering work does not hold chunk locks.
