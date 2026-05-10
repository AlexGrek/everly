# Chunk Rendering Pipeline

This is the current chunk rendering flow in `src/hypermap_world.rs`.

## 1) Planning (main update)

- Read strategy camera focus.
- Resolve current chunk and local position.
- Compute directional target set (4 chunks).
- Queue despawns for chunks leaving visibility.
- For new targets:
  - Ensure chunk is generated (sync): new chunks start as a void ring plus
    streets and **procedural houses** (perimeter, interior partitions, door
    gaps); chunk `(0,0)` then receives the `world_map.txt` overlay when present.
  - Clone chunk cells for rendering.
  - Spawn async task to build render payload.

## 2) 30 FPS Render Tick

Render/despawn application runs on a dedicated `30 Hz` timer.

- Process limited despawns (`MAX_DESPAWNS_PER_TICK`).
- Poll async tasks; completed payloads go to ready queue.
- Spawn limited new chunk visuals (`MAX_SPAWNS_PER_TICK`).

## 3) Chunk Visuals

Each visible chunk is rendered with batched meshes (see `src/hypermap_world.rs`):

- **Floor 0:** separate road mesh (non-void cells) and wall mesh; upper floors split similarly so HUD floor changes do not rebake floor 0.
- **Road / floor quads:** one batched mesh for **all non-void cells** — both **`ROAD`** and **`WALL`** get a horizontal floor quad at the storey base so open parts of wall tiles match road material (wall tops alone would backface-cull from above when slabs are thin).
- **Wall mesh:** vertical slabs from wall bitmask edges only.
- Optional **water** tile mesh per chunk when floor-0 void is present.

This avoids per-tile entity churn and reduces frame spikes.

## 4) Wall Geometry

- Each wall cell has a **bitmask** (north / south / east / west); geometry is
  one slab per bit (see `docs/tilemap.md` and `world_map::for_each_wall_segment`).
  **`Corner`** cells add one 0.2×0.2 m column at a chosen cell corner
  (`WallCorner::xz_offset_from_cell_center`).
- **Slab thickness** is **one-fifth of a cell** (**0.2 m**); **slab height** is **`HYPERMAP_WALL_HEIGHT` (3.0 m)** — `src/floor_level.rs`, used by `hypermap_world` and `world_map` wall meshes.
- **Storey vertical spacing** uses **`HYPERMAP_FLOOR_HEIGHT`** (slightly **> 3 m**, = wall height + a small clearance) for floor quad Y positions and camera floor level so meshes do not z-fight at storey boundaries.

## Notes

- Wall material currently uses `cull_mode: None` for robustness with custom
  mesh winding.
