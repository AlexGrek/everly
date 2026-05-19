# Level save and load

Canonical reference for how Everly persists a playable level on disk. All paths are
relative to the **process working directory** (run from the repo root so `levels/`
resolves correctly).

**There is no autosave.** Runtime changes (geometry edits, dirt deposits, actor
movement, temperature drift) live in memory until the player presses **Save** in
the map editor palette (`docs/map-editor.md`). Quitting without Save drops unsaved
chunks and field edits.

## Level folder layout

Active level name comes from the [`LevelName`](../src/map/level.rs) resource, set
by the main menu when the player picks or creates a level (`docs/main-menu.md`).

```text
levels/level_{name}/
├── geometry/
│   └── {chunk_x}_{chunk_y}.txt    # cell geometry (ASCII, see tilemap.md)
├── style_floor/
│   └── {chunk_x}_{chunk_y}.txt    # per-tile floor material (optional floors)
├── style_wall/
│   └── {chunk_x}_{chunk_y}.txt    # per-tile wall material (optional floors)
├── dirt.bin                       # all saved dirt chunks (binary)
├── temperature.bin                # all saved temperature chunks (binary)
├── metadata/
│   └── {chunk_x}_{chunk_y}.json   # procedural room layout reference (optional)
├── actors.json                    # all actors in the level (JSON)
└── camera.json                    # strategy camera (JSON)
```

Files may be absent (e.g. a level with only `geometry/0_0.txt` until the player
explores and saves). Missing files use the defaults described under **Load**.

## What Save writes

Triggered only by the map editor **Save** button (`map_edit_save_button` in
`src/edit/map_edit.rs`).

| Step | Function / module | What |
|------|-------------------|------|
| 1 | `DirtMap` / `TemperatureMap` | `flush_if_pending()` merges field write buffers into read buffers |
| 2 | [`save_full_generated_level`](../src/map/level.rs) | All **loaded** chunks across geometry + styles + fields (see below) |
| 3 | [`save_level_actors`](../src/actor/snapshot.rs) | Every glitch/black bot in the world |
| 4 | [`save_level_camera`](../src/scene/camera_snapshot.rs) | Current `StrategyCamera` state |

### Which chunks are included

[`loaded_chunk_coord_union`](../src/map/level.rs) builds the save set as the
**union** of chunk coordinates that exist in any of:

- `HypermapRuntime.map` (geometry / `CellType`)
- `style_floor_map`, `style_wall_map`
- dirt read hypermap
- temperature read hypermap

So Save persists **every chunk ever generated in memory this session**, not only
the three chunks in the camera render window (`HypermapRuntime.desired_chunks`).

Chunks that were never visited (no hypermap allocation) are not written.

### Per-artifact save behavior

| Artifact | On disk | Notes |
|----------|---------|--------|
| Geometry | One file per chunk | [`encode_chunk_geometry`](../src/map/level.rs); floors that are all void omitted |
| Floor style | One file per chunk | Omitted if every cell is default style |
| Wall style | One file per chunk | Omitted if every cell is default style |
| Dirt | **Single** `dirt.bin` | All in-memory dirt chunks in one file ([`save_dirt_bin`](../src/map/tile_field_level.rs)) |
| Temperature | **Single** `temperature.bin` | Same format as dirt |
| Actors | `actors.json` | Position, movement state, visuals, RNG seeds — see `src/actor/snapshot.rs` |
| Camera | `camera.json` | Focus, distance, yaw, pitch, view mode |
| Procedural metadata | `metadata/{x}_{y}.json` | Per-chunk houses + entries (see [`map-generator.md`](map-generator.md)) |

## What is not saved separately

| Data | Behavior |
|------|----------|
| `static_passability_map` | Rebuilt from geometry whenever a chunk is generated or edited |
| `static_subtile_cache` | Rebuilt from geometry |
| Dynamic actor footprints | Re-stamped on load from saved actor positions (`restore_loaded_actor_footprints` in `src/actor/snapshot.rs`) |
| Overlay GPU textures | Recomputed from dirt/temperature hypermaps |
| `world_map.txt` / `world_map_floor1.txt` | Repo-root startup overlays only; not part of level Save |

## Load timeline

### Entering gameplay (`OnEnter(GameState::InGame)`)

Order matters for plugins that chain after hypermap setup:

| When | Source | Condition |
|------|--------|-----------|
| Camera spawn | Default `StrategyCamera` | Always |
| Camera override | `levels/level_{name}/camera.json` | If file exists (`CameraSnapshotPlugin`) |
| Hypermap runtime | Empty hypermaps + defaults | `setup_hypermap_runtime` |
| Actors | `levels/level_{name}/actors.json` | If file exists (`ActorSnapshotPlugin`); then footprint restore into dynamic passability |

No geometry or field files are loaded at menu transition — only when chunks are
needed.

### First time a chunk is needed

[`ensure_chunk_generated`](../src/map/hypermap_world.rs) runs when the camera
visibility set includes a chunk (and before map edits on that chunk).

```text
1. If hypermap already has chunk → return
2. Try levels/level_{name}/geometry/{x}_{y}.txt
      → Ok: load geometry only (no procedural fill)
      → missing/invalid: procedural map generator (random seed, in memory)
3. If chunk is (0,0) AND step 2 was procedural:
      overlay world_map.txt (floor 0) and world_map_floor1.txt (floor 1) if present
4. Mirror geometry → static_passability_map + static_subtile_cache
5. Try load style_floor / style_wall files for this chunk (optional)
```

**Important:** If `geometry/0_0.txt` exists (including new levels created with
only a road origin), `world_map.txt` is **not** applied — the geometry file is
authoritative.

### First time dirt / temperature need a chunk

When a visible chunk is seeded (`seed_dirt_for_visible_chunks` /
`seed_temperature_for_visible_chunks`):

1. Once per level per field: read entire `dirt.bin` or `temperature.bin` into the
   field hypermap (if file exists).
2. If that chunk exists in the hypermap after hydration → use saved samples.
3. Else → one-time **random** procedural seed for that chunk (in memory only until Save).

Actor track deposits and other runtime field changes update the write buffer;
they are included in the next Save after `flush_if_pending`.

## File formats

### Geometry (`geometry/{x}_{y}.txt`)

- ASCII, two characters per cell — [`docs/tilemap.md`](tilemap.md).
- Sections `# floor N` for `N` in `0..=9`; `128` lines × `128` tokens per section.
- Floors omitted from the file load as all void.

### Style (`style_floor/`, `style_wall/`)

- Same section layout as geometry; one short token per cell (`TileStyle` in
  `src/map/world_map.rs`).
- Files with only default style are not written on Save.

### Tile fields (`dirt.bin`, `temperature.bin`)

Binary format implemented in [`src/map/tile_field_level.rs`](../src/map/tile_field_level.rs):

| Offset | Size | Content |
|--------|------|---------|
| 0 | 4 | Magic `EVTF` |
| 4 | 4 | Version `1` (u32 LE) |
| 8 | 4 | Chunk count (u32 LE) |
| 12 | 8 + 65536 | Per chunk: `chunk_x` i32 LE, `chunk_y` i32 LE, then `16384` × `f32` LE |

- Samples are **ground floor only**, row-major local chunk coordinates
  (`x` then `y`, `0..127`).
- Dirt values are typically `0.0..=1.0`; temperature values are degrees Celsius
  (clamped at runtime to `−30..=+30` when sampled for display).

### Actors (`actors.json`)

- JSON, pretty-printed; `version` field (currently `1`).
- Tagged union `glitch_bot` / `black_bot` with `state` and `visual` snapshots.
- Loaded actors get [`LevelActor`](../src/actor/snapshot.rs) and replace any
  default spawns for that session.

### Camera (`camera.json`)

- JSON; `version` `1`; nested `camera` with focus, pan velocity, distance, yaw,
  pitch, `view_mode` (`strategy` | `map`).

## New levels

[`create_new_level_with_road_origin`](../src/map/level.rs) (main menu **+ New level**):

- Creates `geometry/0_0.txt` with floor `0` entirely road.
- Does **not** create `dirt.bin`, `temperature.bin`, `actors.json`, or `camera.json`.
- Other chunks appear when the camera reaches them (procedural generator, random
  seed) until the player saves.

## Repo-root overlays (not level Save)

| File | Applies when |
|------|----------------|
| `world_map.txt` | Center chunk `(0,0)` procedurally generated (no geometry file on load) |
| `world_map_floor1.txt` | Same, floor 1 overlay |

These remain separate from `levels/level_{name}/` and are documented in
[`tilemap.md`](tilemap.md) and [`hypermap.md`](hypermap.md).

## Related code index

| Concern | Primary module |
|---------|----------------|
| Save orchestration | `src/edit/map_edit.rs` → `save_full_generated_level` |
| Geometry encode/decode | `src/map/level.rs` |
| Field binaries | `src/map/tile_field_level.rs` |
| Chunk generation | `src/map/hypermap_world.rs` |
| Procedural geometry | `src/map/map_generator/` |
| Actors | `src/actor/snapshot.rs` |
| Camera | `src/scene/camera_snapshot.rs` |
| Dirt / temperature runtime | `src/map/dirt.rs`, `src/map/temperature.rs` |

## See also

- [`map-editor.md`](map-editor.md) — Save button UI and edit workflow
- [`hypermap.md`](hypermap.md) — when chunks are generated and meshed
- [`map-generator.md`](map-generator.md) — procedural fill when geometry file missing
- [`tile-fields.md`](tile-fields.md) — runtime field buffers and overlays
- [`field-interactions.md`](field-interactions.md) — actor dirt deposits (need Save to persist)
- [`main-menu.md`](main-menu.md) — level pick / create and `LevelName`
