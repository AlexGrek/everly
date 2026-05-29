---
name: map-creator
description: >-
  Authors and validates Everly startup tilemaps (`world_map.txt`): 2-character
  cells, void/road tokens, wall edge bitmasks (`w`+hex, `wn`/`ws`/`we`/`ww`
  shortcuts), rectangular layout, and center-chunk overlay rules; includes
  `scripts/` helpers (e.g. `generate_world_map.py`) to regenerate the default
  map. Use when creating or editing maps, `world_map.txt`, `docs/tilemap.md`,
  `scripts/`, wall masks, neighborhoods, or hypermap center content.
paths:
  - "world_map.txt"
  - "world_map_floor1.txt"
  - "docs/tilemap.md"
  - "docs/hypermap.md"
  - "docs/level-persistence.md"
  - "docs/rendering-pipeline.md"
  - "scripts/generate_world_map.py"
  - "src/map/floor_level.rs"
---

# Map creator (Everly tilemap)

## `scripts/` directory

Map tooling lives under **`scripts/`** (repo root). Today:

| Script | Role |
|--------|------|
| **`scripts/generate_world_map.py`** | Regenerates **`world_map.txt`** from code (void ring, road spine, multiple building footprints, bitmask walls). Run after changing layout logic there, or when you want a clean baseline file without hand-editing 4k cells. |

Add new **`scripts/*.py`** helpers here (validators, exporters, other generators) and reference them from this skill so agents know where to look.

## Before editing

1. Read **`docs/tilemap.md`** — canonical encoding (void, road, wall bitmask → world **XZ** via `for_each_wall_segment`, token rules) and **world units**.
2. For multi-floor overlays and chunk visibility, skim **`docs/hypermap.md`** and **`docs/rendering-pipeline.md`**.
3. For **`levels/level_{name}/`** save/load (geometry, `dirt.bin`, actors, no autosave), read **`docs/level-persistence.md`**.
4. Parser lives in **`src/map/world_map.rs`** (`parse_cell_token`, `WallMask`, `MASK_*`). Runtime must stay aligned with docs.
5. Vertical spacing and camera floor height live in **`src/map/floor_level.rs`** (`HYPERMAP_WALL_HEIGHT`, `HYPERMAP_FLOOR_HEIGHT`).
6. If the task is **regenerate or refactor the default map layout**, read **`scripts/generate_world_map.py`** first (or run it and diff `world_map.txt`).

## World scale (authoring mental model)

Treat **one world unit as one meter** in Everly’s map space:

| Quantity | Value | Source of truth |
|----------|-------|-----------------|
| Cell footprint (XZ) | **1 m × 1 m** | Integer grid in `hypermap_world` / `world_map` spawn |
| Wall slab thickness (thin axis) | **0.2 m** (one-fifth of a cell) | `world_map::WALL_THICKNESS`, `for_each_wall_segment` |
| Wall height (vertical) | **3.0 m** per storey | `floor_level::HYPERMAP_WALL_HEIGHT` |
| Storey spacing (floor plane to floor plane) | **3.03 m** (`3.0 + 0.03`) | `floor_level::HYPERMAP_FLOOR_HEIGHT` — slightly above wall height so upper floor meshes do not z-fight with wall tops |

**Center chunk overlays:** `world_map.txt` stamps floor **0** on chunk `(0,0)`; **`world_map_floor1.txt`** (when present) stamps **floor 1** on the same chunk. Keep both maps the same rectangular size when authoring paired floors.

## Cell format

- **Exactly two characters per cell.** Space-separated rows in `world_map.txt` are fine; whitespace is stripped before pairing.
- Every non-empty line must have the **same number of cells** (even total character count after removing whitespace).

## Tokens

The **full token table** (void/road, wall hex masks + `wn`/`ws`/`we`/`ww` aliases,
corner pillars `c7`/`c9`/`c1`/`c3`, charging stations `cn`/`cs`/`ce`/`cw`) is the
canonical encoding section in **[`docs/tilemap.md`](../../../docs/tilemap.md) § Encoding**.
Read it there — do not duplicate the table here.

Key invariants to remember: **`we` is the east-edge alias, never hex 14** (use `wE`);
lowercase `wa`…`wf` are rejected; `w0` (mask zero) is invalid; other `c*` digit
combinations (e.g. `c2`) are invalid.

## Bitmask quick reference

- N=1, S=2, E=4, W=8 — OR bits for corners and T-shapes (e.g. NW corner = `9` → `w9`).
- Full box perimeter on one cell is not typical; use separate cells per edge or combined masks as designed.

## Placement

- Map is **centered** into the **64×64** hypermap chunk `(0,0)`; cells outside that square are skipped.
- Prefer authoring **≤ 64×64** so intent matches what players see at origin.

## Programmatic regen (`scripts/generate_world_map.py`)

From the repo root, overwrite **`world_map.txt`** with the scripted handcrafted
layout (rectangles, L-shape, hollow frame + courtyard shed, NE wing, SE
stepped slabs, road spine, void margin):

```bash
python3 scripts/generate_world_map.py
python3 scripts/generate_world_map.py --output path/to/world_map.txt
```

## Workflow checklist

- [ ] Row lengths match (same token count per line).
- [ ] Only known two-char tokens (no stray single letters).
- [ ] No `w0`; use `wE` not `we` when mask must be 14.
- [ ] After edits, run **`cargo test world_map::`** (parser unit tests) or **`cargo check`** if Rust changed.
- [ ] If **`scripts/generate_world_map.py`** changed, run it and confirm **`world_map.txt`** still parses.

## When Rust or scripts change

If you add tokens or change bitmask rules, update **`docs/tilemap.md`**, **`parse_cell_token`**, and any procedural stampers in **`src/map/hypermap_world.rs`**. If you change **vertical scale** (wall height, storey spacing, clearance), update **`src/map/floor_level.rs`**, **`docs/tilemap.md`**, **`docs/rendering-pipeline.md`**, and this skill’s **World scale** table so they stay aligned. If the **Python generator** encodes walls or tokens, keep **`scripts/generate_world_map.py`** in sync with the same bitmask rules, then re-run it so **`world_map.txt`** matches.

**Procedural chunk rooms** (`src/map/map_generator/`) are a separate path — use **`.claude/SKILLS/map-generator/SKILL.md`**, not this skill’s corner-pillar layout for rectangular room outlines.
