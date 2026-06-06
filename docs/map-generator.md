# Procedural map generator

Agent checklist: **`.claude/SKILLS/map-generator/SKILL.md`** (pipeline, corner-gap pitfall, linked files).

Rust implementation in `src/map/map_generator/` (entry point [`mod.rs`](../../src/map/map_generator/mod.rs)).
Used at runtime when a chunk has no level geometry file (`ensure_chunk_generated` → [`fill_procedural_chunk`](../../src/map/map_generator/mod.rs)).

**Persistence:** procedural fill is in-memory only until the map editor **Save** button
writes the level. See [`level-persistence.md`](level-persistence.md).

## Intermediate representation

Generation runs on a [`MapDraft`](../../src/map/map_generator/draft.rs) grid (`DraftTile`), then
[`finish`](../../src/map/map_generator/draft.rs) emits [`CellType`](../../src/map/world_map.rs) tiles.

## Building footprint

1. **Subseed rooms** — axis-aligned rectangles grow only from subseed centers (internal to the pipeline).
2. **Houses** — only **overlapping** subseed rects merge into one [`House`](../../src/map/map_generator/house.rs) (edge-touching rects stay separate; subseed identity discarded). More primaries and subseeds per chunk increase house count.
3. **Union interior** — every tile inside any house is painted `Open` (road); merged footprints share one outer shell with **no inner walls** between overlapping parts.
4. **Outer shell** — union perimeter wall bitmasks (same rules as the editor **Room** brush).
5. **Inner corner pillars** — [`corner_pillars.rs`](../src/map/map_generator/corner_pillars.rs) + [`step_corners.rs`](../src/map/map_generator/step_corners.rs). See **[`corners.md`](corners.md)**.
6. **One door per house** — [`step_place_house_doors`](../../src/map/map_generator/step_door.rs) picks a validated site per house: exterior tile must be road (not inside any house footprint), interior must be open floor, single-bit wall slab (no L-corner traps), and the doorway must not face another house’s wall.
7. **Inner room walls** — [`step_split_houses_into_rooms`](../../src/map/map_generator/step_inner_walls.rs) runs only on houses with `footprint_area >= 30` (`MIN_HOUSE_AREA_FOR_CENTER_WAVE`). Budget is `floor(area / 80)` cuts (one per 80 sq units), split ceiling-to-horizontal / floor-to-vertical, capped at 3 each. A cut is kept only when every resulting sub-room is ≥ 2 cells in either direction, ≥ 6 cells in bbox area, and ≥ 2 cells away from every existing parallel wall (outer or inner). Stamps `MASK_NORTH` / `MASK_WEST` on cells along the line; concave voids and existing corner pillars are skipped, and the outer door cell is never re-sealed. Rooms are isolated — no inner doors are placed at this step.
8. **Inner doors** — [`step_place_inner_doors`](../../src/map/map_generator/step_inner_doors.rs) opens one slab edge at a time until every walkable cell of the house is reachable from the entry. Connectivity is **edge-based**: a `Wall(bits)` cell is walkable floor with slabs on the named edges (not a solid blocker), so passage between two in-house cells is blocked only when a slab sits on their shared edge. It floods the accessible region from the entry interior tile, then clears the slab bits on a random blocked edge bordering a not-yet-reachable in-house cell, repeating until the house is one connected region. Only interior edges (both cells in-house) are opened, so the outer shell is never breached.
9. **Home crawlers** — **marble** (`fm`) BFS wave from the main entry (Manhattan radius 3–5, rng) on all houses. **Glass** (`fg`) center wave only when footprint area ≥ 30 cells ([`grid_fill::count_region_area`](../../src/map/map_generator/grid_fill.rs) at cluster time). Walls, corners, and doors are unchanged for small houses.
10. **Charging stations** — [`step_place_charging_stations`](../../src/map/map_generator/step_charging_stations.rs) places **one** `Charger` per house: a walkable interior `Open` cell with **exactly one** orthogonal wall neighbor (so its back faces that wall and it is **not** in a corner), excluding the doorway cell and its inner tile. The lone wall side becomes the [`ChargerFacing`](../../src/map/world_map.rs). Runs **last** (after crawlers) so the marble/glass waves are unaffected; chargers stay passable. A house with no qualifying cell simply gets none.

Do **not** stamp per-rectangle [`perimeter_wall_mask`](../../src/map/world_map.rs) loops on overlapping rects (that recreates inner walls). Convex outer corners stay multi-bit wall cells only (no separate pillars there).

## Reusable building generation (editor "House" tool)

Steps 4–10 above (everything after houses are known) are factored into
`MapDraft::build_house_structures`, shared by the procedural pipeline and a
single-house entry point [`generate_house_tiles`](../../src/map/map_generator/mod.rs):

1. The map editor **House** brush (see [`map-editor.md`](map-editor.md)) drags a boundary rectangle.
2. On commit, `generate_house_tiles(width, height, seed)` builds a padded `MapDraft`
   pre-filled with `Open` road (so doors find exterior road on every side), seeds **one**
   [`House`](../../src/map/map_generator/house.rs) (`House::from_single_rect`) filling the
   boundary, runs `build_house_structures`, then returns the boundary-rectangle slice of
   finished `CellType` + `TileStyle` tiles (`HouseToolTiles`). Boundaries below
   `MIN_HOUSE_TOOL_SIDE` (`10`) on either side return `None`.
3. The editor writes those tiles into the live hypermap at the selected world offset.

This guarantees the hand-placed building uses the **same** shell/door/inner-room/charger
geometry as procedural chunks — no divergent copy of the wall logic.

## Metadata (`GeneratedChunkMetadata`, version 3)

After generation, reference data is stored in [`HypermapRuntime::procedural_metadata`](../../src/map/hypermap_world.rs) (chunk-local tile coords) and saved as `levels/level_{name}/metadata/{x}_{y}.yaml` on **Save**.

| Field | Meaning |
|-------|---------|
| `houses[]` | Each merged building: bounds, `center_x`/`center_z`, `area` (footprint cell count), `entry` |

World tiles: `runtime.procedural_metadata.get(coord)` then [`house_entry_world`](../../src/map/chunk_metadata.rs) / [`house_center_world`](../../src/map/chunk_metadata.rs).

## Related

| Topic | Doc / code |
|-------|------------|
| Tile encoding, bitmasks, `c*` pillars | [`tilemap.md`](tilemap.md), [`corners.md`](corners.md), [`world_map.rs`](../src/map/world_map.rs) |
| Chunk gen vs disk geometry | [`hypermap.md`](hypermap.md), [`hypermap_world.rs`](../src/map/hypermap_world.rs) |
| Editor Room brush (reference) | [`map-editor.md`](map-editor.md), [`map_edit.rs`](../src/edit/map_edit.rs) |
| Save procedural chunks | [`level-persistence.md`](level-persistence.md), [`map-editor.md`](map-editor.md) |
| Wall meshing | [`rendering-pipeline.md`](rendering-pipeline.md) |

## Config

- Size: `128×128` (`HYPERMAP_CHUNK_SIZE`)
- Margin: `CHUNK_VOID_MARGIN` (`2`)
- Seed: random on each procedural fill (`random_rng_seed`)
