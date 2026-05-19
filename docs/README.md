# Docs Index

Current behavior docs:

- `docs/tilemap.md` — tilemap file format, **world units** (1 m cells, wall thickness/height, storey spacing), wall bitmask → **world XZ** placement, and center-chunk overlay behavior.
- `docs/hypermap.md` — Hypermap chunk model, multi-floor data, generation, visibility, and water rules.
- `docs/rendering-pipeline.md` — runtime planning, async prep, 30 FPS render stream, floor vs wall meshes.
- `docs/map-editor.md` — in-game hypermap edit mode (HUD, preview, placement, variants, remesh).
- `docs/actor.md` — actor trait runtime loop, movement buffer, footprint collision flow, **main tile** (`round(center)`), and usage examples.
- `docs/chunk-overlay.md` — per-chunk RGBA overlay textures: dirt stains, generic writable layer, and occupancy debug layer (F4 toggle).
- `docs/tile-fields.md` — tile-resolution scalar fields (dirt, temperature) and shared `TileFieldMap`.
- `docs/field-interactions.md` — actor main-tile tracking and dirt deposits on tiles actors leave.

Agent skills (repo): `.claude/SKILLS/map-creator/SKILL.md` (tilemaps + scale), `.claude/SKILLS/bevy-engineer/SKILL.md` (Bevy 0.18 + Everly map constants).
