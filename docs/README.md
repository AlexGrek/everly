# Docs Index

Current behavior docs:

- `docs/tilemap.md` — tilemap file format, **world units** (1 m cells, wall thickness/height, storey spacing), wall bitmask → **world XZ** placement, and center-chunk overlay behavior.
- `docs/hypermap.md` — Hypermap chunk model, multi-floor data, generation, visibility, and water rules.
- `docs/rendering-pipeline.md` — runtime planning, async prep, 30 FPS render stream, floor vs wall meshes.
- `docs/map-editor.md` — in-game hypermap edit mode (HUD, preview, placement, variants, remesh).
- `docs/actor-spawner.md` — in-game actor spawner (Bot/Black palette, own HUD toggle, click-to-spawn).
- `docs/level-persistence.md` — **save/load**: level folder layout, Save button, binaries, actors, camera, load order.
- `docs/map-generator.md` — procedural chunk geometry (`src/map/map_generator/`).
- `docs/add_new_tile.md` — end-to-end checklist for adding a new `CellType` (encoding, passability, generation, editor, rendering).
- `docs/interactive-entities.md` — sparse per-tile store of stateful reference-type objects (chargers), the `InteractiveEntity` trait/enum, and the `InteractiveEntityMap` resource.
- `docs/corners.md` — inner `c*` corner pillars (concave union elbows, detection algorithm).
- `docs/actor.md` — actor trait runtime loop, movement buffer, footprint collision flow, **main tile** (`round(center)`), and usage examples.
- `docs/charge.md` — bot **battery charge**: `Charge` component, discharge system, depletion → movement disabled, inspector display, persistence.
- `docs/chunk-overlay.md` — per-chunk RGBA overlays: temperature heatmap (F5), dirt, generic layer, occupancy debug (F4).
- `docs/tile-fields.md` — tile-resolution scalar fields (dirt, temperature) and shared `TileFieldMap`.
- `docs/field-interactions.md` — actor main-tile tracking and dirt deposits on tiles actors leave.
- `docs/test-world.md` — **shared `TestWorld` fixture** (6×6-chunk generated world) that every game-logic unit test should load.

Agent skills (repo): `.claude/SKILLS/map-creator/SKILL.md` (tilemaps + scale), `.claude/SKILLS/map-generator/SKILL.md` (procedural chunks + room walls), `.claude/SKILLS/bevy-engineer/SKILL.md` (Bevy 0.18 + Everly map constants).
