---
name: map-generator
description: >-
  Procedural hypermap chunk generation in Rust (`src/map/map_generator/`):
  MapDraft pipeline, seed rooms, perimeter wall bitmasks, doors, and runtime
  fill via `fill_procedural_chunk`. Use when editing or debugging procedural
  geometry, room outlines, corner gaps, `ensure_chunk_generated` fallback,
  or `docs/map-generator.md`.
paths:
  - "docs/map-generator.md"
  - "docs/corners.md"
  - "docs/hypermap.md"
  - "docs/tilemap.md"
  - "docs/map-editor.md"
  - "docs/rendering-pipeline.md"
  - "docs/level-persistence.md"
  - "src/map/map_generator/**"
  - "src/map/world_map.rs"
  - "src/map/hypermap_world.rs"
  - "src/edit/map_edit.rs"
---

# Map generator (Everly procedural chunks)

## Before editing

1. Read **`docs/map-generator.md`** — scope, persistence, pipeline summary.
2. When editing **inner corner pillars** (`corner_pillars.rs`, `step_corners.rs`, or `c*` placement), read **`docs/corners.md`** first.
3. Read **`docs/tilemap.md`** § Wall bitmask and § Corner pillar — encoding rules the mesher expects.
4. Skim **`docs/hypermap.md`** § Generation — when procedural fill runs vs disk geometry.
5. For the editor **Room** brush (reference behavior), skim **`docs/map-editor.md`** and **`src/edit/map_edit.rs`** (`MapTileKind::Room`, `perimeter_wall_mask`).
6. Read **`.claude/SKILLS/bevy-engineer/SKILL.md`** if touching Rust outside this module (Bevy 0.18, wall height constants).
7. For hand-authored `world_map.txt` / tokens only, use **`.claude/SKILLS/map-creator/SKILL.md`** instead — do not use Python for procedural chunks (**`CLAUDE.md`**: never use Python for map generation).

## Source of truth

| Piece | Path |
|-------|------|
| Generator entry + pipeline orchestration | `src/map/map_generator/mod.rs` |
| `MapDraft`, `DraftTile`, finish | `src/map/map_generator/draft.rs` |
| Config + persisted metadata types | `src/map/map_generator/types.rs` |
| Union shell helpers | `src/map/map_generator/union.rs` |
| Concave corner pillar detection (walls only) | `corner_pillars.rs` — see **`docs/corners.md`** |
| Pipeline steps | `step_carpet.rs`, `step_seeds.rs`, `step_rooms.rs`, `step_shell.rs`, `step_corners.rs`, `step_door.rs` |
| `perimeter_wall_mask`, `CellType`, `WallMask`, `for_each_wall_segment` | `src/map/world_map.rs` |
| Runtime chunk fill hook | `src/map/hypermap_world.rs` → `ensure_chunk_generated` → `fill_procedural_chunk` |
| Editor Room brush (must match generator walls) | `src/edit/map_edit.rs` |
| Chunk encode/decode for Save | `src/map/level.rs` (`encode_chunk_geometry`, geometry `.txt`) |
| Wall meshing | `src/map/hypermap_world.rs` (`build_floor0_wall_mesh`, `for_each_wall_segment`) |

## Related docs

| Doc | Why |
|-----|-----|
| `docs/map-generator.md` | Procedural overview and config |
| `docs/corners.md` | Inner `c*` detection algorithm, variant mapping, tests |
| `docs/tilemap.md` | Token format, bitmask → world XZ, corner pillars vs combined masks |
| `docs/hypermap.md` | Chunk lazy gen, procedural vs `levels/.../geometry/` |
| `docs/map-editor.md` | Room / Wall / Corner brushes; Save persists procedural chunks |
| `docs/level-persistence.md` | Geometry on disk; procedural until Save |
| `docs/rendering-pipeline.md` | Floor + wall batch meshes after chunk gen |

## Runtime behavior

- **Trigger:** `ensure_chunk_generated` in `hypermap_world.rs` loads `levels/level_{name}/geometry/{x}_{y}.txt` when present; otherwise calls **`fill_procedural_chunk`** with a **new random seed** each time (in memory only).
- **Editor Re-gen:** map palette **Re-gen** calls [`regenerate_procedural_chunk`](../../src/map/hypermap_world.rs) on the camera chunk (always procedural, no disk / no center `world_map` overlay) and despawns actors on that chunk — see **`docs/map-editor.md`**.
- **Chunk size:** `128×128` (`HYPERMAP_CHUNK_SIZE`).
- **Void margin:** `CHUNK_VOID_MARGIN` (`2`) — road carpet inset from chunk edges.
- **Persistence:** Procedural tiles are **not** written to disk until map editor **Save** (`docs/level-persistence.md`).

## Pipeline (`MapDraft`)

Order in `MapDraft::generate` / `run_into_chunk` — **do not reorder** without revisiting doors and overlap:

1. `step_init_carpet` — `Open` (road) inside margin
2. `step_place_primary_seeds` — 3–5 random centers
3. `step_separate_primary_seeds` — push apart (`MIN_SEED_DISTANCE`, `BORDER_CLEARANCE`)
4. `step_spawn_subseeds` — 1–3 offsets per primary (`growth_centers`)
5. `step_grow_rooms` — axis-aligned rects from **`subseed_centers` only** (`area >= 4`; internal `room_records`)
6. `step_cluster_houses` — merge touching / overlapping rects into [`House`](../../src/map/map_generator/house.rs) footprints (subseed data dropped)
7. `step_paint_union_interior` — all house tiles → `Open` (no walls)
8. `step_build_union_outer_walls` — **`union_perimeter_wall_mask`** on the combined outer shell only
9. `step_stamp_union_inner_corner_pillars` — [`detect_corner_pillars`](../../src/map/map_generator/corner_pillars.rs) (see **`docs/corners.md`**)
10. `step_place_house_doors` — **one door per house** (`clear_wall_edge` / fallback carve)
11. `step_home_crawlers` — marble wave from main entry; glass center wave only if `footprint_area >= MIN_HOUSE_AREA_FOR_CENTER_WAVE` (30)
12. `finish` / `write_chunk_floor0_and_styles` — `DraftTile` → `CellType` + `TileStyle` chunk
13. `build_metadata` → [`GeneratedChunkMetadata`](../../src/map/chunk_metadata.rs) v2 (`houses[]` with embedded `entry`)

`DraftTile` is **not** `CellType`: `Void`, `Open`, `Wall(u8)`, `Corner(WallCorner)` during generation.

### Metadata fields (chunk-local tiles)

- **`houses[]`**: one entry per merged building — bounds, `center_x`/`center_z`, `area`, `entry`.
- **Area utils**: [`grid_fill.rs`](../../src/map/map_generator/grid_fill.rs) — `flood_fill_area` (connected), `count_region_area` (box); house footprint uses the latter at cluster time.
- World coords: `meta.house_entry_world(i, chunk)`, `meta.house_center_world(i, chunk)`; `entrypoint_world` = first house entry.

## Room outlines — critical pitfall (corner gaps)

**Wrong (causes visible gaps at every corner):**

- Put `CellType::Corner` / `c7` `c9` `c1` `c3` **pillar cells on the four vertices** of the room rectangle.
- Put **single-edge** `Wall` cells (`wn` / `we` / …) only on cells **between** corners (skipping corner tiles).

Slabs are offset to cell edges; pillars are 0.2×0.2 m posts. They **do not** bridge the space between a north slab on `(x0+1, z0)` and a NW pillar on `(x0, z0)`. The mesher is correct; the **layout** is wrong.

**Right (matches map editor Room brush):**

- Stamp **every perimeter cell** with **`perimeter_wall_mask(x, z, x0, x1, z0, z1)`** from `world_map.rs`.
- Corner tiles get **multiple bits** (e.g. NW → `MASK_NORTH | MASK_WEST` → `w9`), so two slabs meet in one cell.

Shared helper (single implementation):

```rust
// src/map/world_map.rs
pub(crate) fn perimeter_wall_mask(cx, cz, min_x, max_x, min_z, max_z) -> WallMask
```

Union shell uses `union_perimeter_wall_mask` in `union.rs` (not per-room `perimeter_wall_mask`). Editor `MapTileKind::Room` in `map_edit.rs` uses `perimeter_wall_mask` for single rectangles. Never duplicate bitmask logic in one place only.

### When `c*` corner pillars are used

- **Concave union corners** — `corner_pillars.rs` + **`docs/corners.md`** (exterior flood, H/V run endpoints, interior notch check, `WallCorner` mapping).
- **Manual** placement — editor **Corner** brush.
- **Not** on convex outer shell corners (those use multi-bit `Wall` on one perimeter cell).

## Union shell (do not regress to per-room walls)

**Wrong:** call `perimeter_wall_mask` / `stamp_room_walls` for **each** `Room` — overlapping rectangles get **inner walls** along shared edges.

**Right:** `union_contains` + `union_perimeter_wall_mask` — one bit per cardinal side that faces **outside the union**; interior tiles stay `Open`.

## Doors

- `is_valid_door_site` — walk tile is exterior road (not inside any house), inward is open floor, single-bit wall only (no L-corner slabs), must not face another house’s wall.
- `step_place_house_doors` — random pick among **valid** sites only (no bad doors left in place).
- Crawlers never modify walls.

## House count

- Primaries: `PRIMARY_SEED_COUNT_MIN`–`MAX` (5–8). Subseeds per primary: 2–4.
- Clustering merges **overlapping** rects only; edge-touching subseed rooms become separate houses.

## Overlapping rooms

Overlapping subseed rects are intentional: they merge into a single open floor. Only the **outer** perimeter gets walls.

## Config constants (`types.rs`)

| Constant | Role |
|----------|------|
| `MIN_SEED_DISTANCE` | Manhattan separation target for primary seeds |
| `BORDER_CLEARANCE` | Keep seeds away from playable border |
| `CHUNK_VOID_MARGIN` | Void ring; must stay aligned with hypermap void inset (`docs/hypermap.md`) |
| `MapGeneratorConfig` | `size`, `margin`, `seed` (`StdRng`, `rand 0.8` `gen_range`) |

## Tests and verification

```bash
cargo test map_generator
cargo check
```

Tests live in `src/map/map_generator/tests.rs`. After wall-outline changes, **run the game** and fly to a **new** chunk (no geometry file) — cached chunks keep old tiles until regenerated.

Optional: `generate_chunk_geometry(&MapGeneratorConfig { seed: N, .. })` for ASCII geometry snippets.

## Change checklist

- [ ] Room walls use `perimeter_wall_mask` (not corner pillars + single-edge loop).
- [ ] `map_edit.rs` still calls shared `perimeter_wall_mask` (no divergent copy).
- [ ] Updated **`docs/map-generator.md`** if pipeline or persistence behavior changed.
- [ ] Updated **`docs/corners.md`** if corner detection or stamping behavior changed.
- [ ] `cargo test map_generator` and `cargo check` pass.
- [ ] Did not add Python procedural generation (`CLAUDE.md` rule).

## When to touch other modules

| Change | Also update |
|--------|-------------|
| New `CellType` / token | `world_map.rs`, `docs/tilemap.md`, map-creator skill, mesher in `hypermap_world.rs` |
| Wall thickness / height | `floor_level.rs`, `docs/tilemap.md`, bevy-engineer skill |
| When procedural runs | `hypermap_world.rs`, `docs/hypermap.md` |
| Editor Room behavior | `map_edit.rs`, this skill, `docs/map-editor.md` |
