# Hypermap Runtime Behavior

This document describes the current runtime behavior of `HypermapWorldPlugin`.

## Core Model

- World space is infinite and addressed with signed integer coordinates.
- Data is chunked into fixed `64x64` chunks.
- Chunks are lazily generated on first access.
- Once generated, chunk data remains in memory (no eviction).
- **Vertical:** each column stores **10 floors** (`0..=9`). **1 world unit = 1 m**; see `docs/tilemap.md` § World units for wall thickness, wall height (`HYPERMAP_WALL_HEIGHT`), and storey spacing (`HYPERMAP_FLOOR_HEIGHT`) in `src/floor_level.rs`.
- **HUD floor level** chooses which floors are drawn (active floor and above; floor 0 visibility rules differ when the active level is 0 versus a higher floor — see `HypermapWorldPlugin` / `ActiveFloorLevel`).

## Generation

- New chunks are generated synchronously.
- Tiles are deterministic random (`ROAD` + directional `WALL`) by chunk seed.
- Chunk `(0,0)` receives an overlay from `world_map.txt` after random fill (floor **0**).
- When **`world_map_floor1.txt`** exists, the same center chunk’s **floor 1** is overwritten from that file (same rectangular size as floor 0 is recommended).

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

- Water is spawned per rendered chunk only if that chunk contains at least one
  `VOID` cell.

## Concurrency and Locking

- Chunk map storage is concurrent (per-chunk lock handles).
- Render prep uses a cloned snapshot of chunk cells at task start.
- Rendering work does not hold chunk locks.
