# Chunk Rendering Pipeline

This is the current chunk rendering flow in `src/map/hypermap_world.rs`.

## 1) Planning (main update)

- Read strategy camera focus.
- Resolve current chunk and local position.
- Compute three-chunk target set (current + one X neighbor + one Y neighbor; see `hypermap.md`).
- Queue despawns for chunks leaving visibility.
- For new targets:
  - Ensure chunk is generated (sync): new chunks use [`map_generator`](map-generator.md)
    (seed rooms, walls, doors on a draft grid, then tile dump); chunk `(0,0)` then
    receives the `world_map.txt` overlay when present.
  - Clone chunk cells for rendering.
  - Spawn async task to build render payload.

## 2) 30 FPS Render Tick

Render/despawn application runs on a dedicated `30 Hz` timer.

- Process limited despawns (`MAX_DESPAWNS_PER_TICK`).
- Poll async tasks; completed payloads go to ready queue.
- Spawn limited new chunk visuals (`MAX_SPAWNS_PER_TICK`).

## 3) Chunk Visuals

Each visible chunk is rendered with batched meshes (see `src/map/hypermap_world.rs`):

- **Floor 0:** separate road mesh (non-void cells) and wall mesh; upper floors split similarly so HUD floor changes do not rebake floor 0.
- **Road / floor quads:** one batched mesh for **all non-void cells** — both **`ROAD`** and **`WALL`** get a horizontal floor quad at the storey base so open parts of wall tiles match road material (wall tops alone would backface-cull from above when slabs are thin).
- **Wall mesh:** vertical slabs from wall bitmask edges only.
- **Charging-station meshes:** `Charger` cells keep their normal floor quad and add
  **three** batched meshes, each with its own material: a **metal pad** (elevated,
  inset; `charger_metal_material`), a **glowing-blue cube** (`charger_glow_material`,
  emissive HDR so the camera Bloom makes it glow), and a bulky **matte-black
  transformer connector** (`charger_connector_material`) that is larger than the cube
  and `CHARGER_CONNECTOR_DEPTH` deep (`1.0 - WALL_THICKNESS` = 0.8 m, four subtiles),
  reaching across the neighboring wall cell to the slab's inner face. Floor 0 and the
  active upper floor each get their own pad + connector entity
  (`build_*_charger_metal_mesh` / `build_*_charger_connector_mesh`). Each charger
  also gets a per-station **glow cube + `PointLight`** pair (each station owns its
  own `StandardMaterial`; cool blue emissive when idle, green when docked — see
  [`InteractiveEntityMap`](../interactive-entities.md) occupant). The point light
  is off while idle and only turns on for the docked station; light sits on the
  room-facing cube face (no shadows).
- Optional **water** tile mesh when floor `0` has interior void; the plane is
  inset from chunk edges so border cells stay dry — see `hypermap.md`.

This avoids per-tile entity churn and reduces frame spikes.

## 4) Wall Geometry

- Each wall cell has a **bitmask** (`MASK_NORTH` … `MASK_WEST`); geometry is
  one slab per bit — world **XZ** offsets per `docs/tilemap.md` § Wall bitmask and
  `world_map::for_each_wall_segment`.
  **`Corner`** cells add one 0.2×0.2 m column at a chosen cell corner
  (`WallCorner::xz_offset_from_cell_center`).
- **Slab thickness** is **one-fifth of a cell** (**0.2 m**); **slab height** is **`HYPERMAP_WALL_HEIGHT` (3.0 m)** — `src/map/floor_level.rs`, used by `hypermap_world` and `world_map` wall meshes.
- **Storey vertical spacing** uses **`HYPERMAP_FLOOR_HEIGHT`** (slightly **> 3 m**, = wall height + a small clearance) for floor quad Y positions and camera floor level so meshes do not z-fight at storey boundaries.

## Notes

- **`append_box` winding quirk — box meshes must use `cull_mode: None`.** The
  shared `append_box` helper (`src/map/hypermap_world.rs`) winds its **±X and ±Y**
  faces outward but its **±Z** faces *inward* (the +Z/−Z triangle front faces point
  opposite their stored normals). Stored normals are correct, so **lighting** is
  fine, but the default backface culling drops the two Z faces and the box reads as
  inside-out. Any material applied to box geometry — `wall_material`,
  `glass_wall_material`, and the charger `charger_metal_material` /
  `charger_glow_material` — therefore sets **`cull_mode: None`**. Floor quads
  (`append_quad`, used for road/floor meshes) only emit the +Y face, which *is*
  wound correctly, so they keep default culling. If you add a new box-based mesh,
  set `cull_mode: None` on its material (or fix the Z-face winding in `append_box`,
  which is shared by walls).
