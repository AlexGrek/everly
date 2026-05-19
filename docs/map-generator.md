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
7. **Home crawlers** — one BFS **wave** per house from that house's **main entry**, propagating to a random Manhattan distance of 3–5 tiles on open floor. Marble (`fm`) on [`DraftTile::Open`](../../src/map/map_generator/draft.rs) only; walls, corners, and doors are not styled.

Do **not** stamp per-rectangle [`perimeter_wall_mask`](../../src/map/world_map.rs) loops on overlapping rects (that recreates inner walls). Convex outer corners stay multi-bit wall cells only (no separate pillars there).

## Metadata (`GeneratedChunkMetadata`, version 2)

After generation, reference data is stored in [`HypermapRuntime::procedural_metadata`](../../src/map/hypermap_world.rs) (chunk-local tile coords) and saved as `levels/level_{name}/metadata/{x}_{y}.json` on **Save**.

| Field | Meaning |
|-------|---------|
| `houses[]` | Each merged building: bounds (`x0`…`z1`), `center_x`/`center_z`, `entry` (walk / wall / outward edge) |

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
