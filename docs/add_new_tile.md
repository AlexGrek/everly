# Adding a New Tile Type

How to add a new `CellType` to Everly end-to-end: encoding, passability,
procedural generation, the map editor, and rendering. The **charging station**
(`CellType::Charger`, tokens `cn`/`cs`/`ce`/`cw`) is the reference example —
grep for `Charger` to see every touch point in one diff.

> **Read first:** `docs/tilemap.md` (token format + world units),
> `docs/rendering-pipeline.md` (chunk meshing), `docs/map-generator.md`
> (pipeline), `.claude/SKILLS/bevy-engineer/SKILL.md` (Bevy 0.18 idioms).

A tile lives in one enum, but the compiler will **not** catch most of the work:
many `match` sites have a `_ =>` catch-all (rendering, partitioning) that
silently drops a new variant. Walk the whole checklist.

## 1. Define the variant — `src/map/world_map.rs`

1. If the tile has orientation/sub-kinds, add a small enum first
   (e.g. `ChargerFacing { North, South, East, West }`) with any geometry
   helpers (`wall_dir`, `wall_delta`).
2. Add the case to `enum CellType`.
3. **Passability:** add the variant to `cell_passability` (`1.0` walkable / `0.0` blocked).
4. **Tokens** (the 2-char map encoding, used by `world_map.txt` *and* level save/load):
   - `parse_cell_token` — decode the token(s). Reuse a free prefix; `c`+letter
     does not collide with the `c`+digit corner pillars. Accept the uppercase
     form too if the existing tokens do.
   - `cell_to_token` — the inverse. **Must round-trip** (`parse(to_token(c)) == c`);
     this is how `encode_chunk_geometry` in `level.rs` persists the tile.
5. Update the module-header doc comment listing the token forms.
6. Add unit tests: parse, round-trip, passability.

## 2. Subtile collision — `src/map/passability.rs`

Add the variant to `cell_subtile_flags`. Walkable floor → `0`; solid →
`FLAG_BLOCKED` on the occupied subtiles; no-floor → `FLAG_VOID`. This feeds the
static collision cache that actors query, so getting it wrong makes units walk
through walls (or get stuck on floor).

## 3. Procedural generation (optional) — `src/map/map_generator/`

Only if the generator should place the tile automatically.

1. `draft.rs` — add a matching `DraftTile` variant and map it in
   `draft_tile_to_cell` (the draft grid is the intermediate representation;
   `finish` is the only place `CellType` is emitted).
2. **Fix the non-exhaustive matches the compiler now flags** in the existing
   steps (`step_door.rs`, `step_inner_doors.rs`, `step_inner_walls.rs`, and the
   generator `tests.rs`). Treat the new variant per its semantics (a walkable
   tile usually groups with `DraftTile::Open`).
3. Add a step file `step_<name>.rs` with `impl MapDraft { pub fn step_…() }`,
   declare `mod` it in `mod.rs`, and call it from `run_pipeline` **in the right
   order** — placement that must not disturb earlier passes (e.g. crawler waves
   read `Open`) goes **last**.
4. Add a generator test asserting the placement invariants and update
   `docs/map-generator.md` + the map-generator skill pipeline list.

## 4. Map editor — `src/edit/map_edit.rs`

1. Add a `MapTileKind` variant and a palette button in `spawn_map_edit_palette`.
2. `resolved_cell` — map the brush (+ scroll `variant`) to the `CellType`.
3. `stroke_world_cells` — choose the stroke shape (single cell, line, rect, …).
4. If the brush has variants, set its range in `map_edit_scroll_variants` and
   add it to the zoom-suppression list in `src/scene/camera.rs::zoom_camera`
   (so the wheel cycles the variant instead of zooming).
5. **Preview:** add an arm to *both* the floor-0 and upper `match kind` blocks in
   `map_edit_update_preview` (these are exhaustive — the build will fail until
   you do). Reuse a `build_*` mesh fn from `hypermap_world.rs`.
6. `floor_styles_for_kind` / `wall_styles_for_kind` — opt into style cycling, or
   let the `_ =>` default give it none.

## 5. Rendering — `src/map/hypermap_world.rs`

Chunks are drawn as **batched meshes per material** (no per-tile entities), and
each material exists twice: once for floor 0 (baked onto the chunk root) and
once for the active upper floor (rebuilt on HUD floor change). Budget one mesh
entity per new material × 2 layers.

1. **Materials:** add `Handle<StandardMaterial>` fields to `HypermapRenderAssets`
   and create them in `setup_hypermap_assets`. Emissive + the camera's `Bloom`/`Hdr`
   gives a glow.
2. **Geometry:** write `append_*` helpers (reuse `append_box` / `append_quad`)
   and `build_floor0_*` / `build_upper_*` mesh fns (the upper variant derives
   `y_base = floor * HYPERMAP_FLOOR_HEIGHT`). For the editor preview, a combined
   single-cell `build_*_preview_mesh` is handy.
3. **Partition:** add `Vec` field(s) to `PreparedChunkRender`, collect the cell
   in `partition_chunk_cells_from_vec` (in both the `floor == 0` and
   `floor == active_floor` blocks), and remember a non-void tile still wants its
   normal floor quad unless it fully replaces the floor.
4. **Spawn:** add marker components, spawn the floor-0 entities in
   `spawn_chunk_meshes`, and store the upper entities in `ChunkUpperMeshEntities`.
5. **Floor change:** rebuild the upper entities in
   `refresh_chunk_upper_layers_on_floor_change`.

## 6. Docs

A new `CellType`/token must update: `docs/tilemap.md` (canonical token table),
`docs/map-editor.md` (brush + wheel), `docs/rendering-pipeline.md` (meshes),
`docs/map-generator.md` + the map-generator skill (if generated), and the
map-creator skill if author-facing.

## Verify

```sh
cargo check          # catches the exhaustive matches (editor preview, draft)
cargo test           # parser round-trip, passability, generator placement
cargo run            # fly to a NEW chunk — cached chunks keep old tiles
```

## Gotchas

- **Round-trip or lose data.** A token that parses but has no `cell_to_token`
  arm is silently dropped on Save.
- **`_ =>` hides bugs.** The rendering partition and mesh builders won't error on
  a missing variant; the tile just renders as nothing.
- **Generation order matters.** Place after passes that scan for `Open`/walls so
  you don't shadow doorways or crawler waves.
- **New chunks only.** Procedural changes appear on freshly generated chunks;
  use the editor **Re-gen** button or move to an ungenerated chunk to see them.
- **Box meshes need `cull_mode: None`.** `append_box` winds its ±Z faces inward,
  so a material using default backface culling renders the box inside-out (flipped
  normals). Set `cull_mode: None` like the wall/charger materials do. See
  `docs/rendering-pipeline.md` § Notes.
