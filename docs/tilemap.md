# Tilemap Format

`world_map.txt` is an authored 2D map that is parsed and applied to the
Hypermap center chunk (`0,0`) when that chunk is first generated. The repo
includes **`scripts/generate_world_map.py`** to regenerate the default
`world_map.txt` from the same layout rules as the parser.

## World units

Everly uses **1 world unit = 1 meter** for map-related geometry:

- Each tile is **1 m × 1 m** in the XZ plane (integer grid).
- **Wall slab thickness** (the thin axis perpendicular to the cell edge) is **one-fifth of a cell** (**0.2 m**); see `world_map::WALL_THICKNESS` and `for_each_wall_segment`.
- **Wall height** is **`HYPERMAP_WALL_HEIGHT` (3.0 m)** per storey.
- **Vertical spacing between floor planes** is **`HYPERMAP_FLOOR_HEIGHT`** in `src/floor_level.rs` — currently **wall height + 0.03 m** so the next storey’s floor mesh sits slightly above wall tops and avoids z-fighting.

## Encoding

- Each cell is exactly 2 characters.
- Each non-empty row must have the same number of cells.
- Whitespace is allowed and ignored, so both compact and space-separated
  layouts are valid.

Non-wall tokens:

- `..` -> `VOID`
- `__` -> `ROAD`

### Path test markers (optional)

Two-character cells used **only for tests and pathfinding tooling** (see
`WorldMapFloor::from_ascii_with_path_markers` and `everly::hypermap_pathfind` in
Rust). They are **not** required in `world_map.txt` for the game.

| Token | Stored cell | Metadata |
|-------|-------------|----------|
| `>A` | `ROAD` (same as `__`) | Start tile `(x, y)` for scripted paths |
| `>B` | `ROAD` (same as `__`) | Goal tile `(x, y)` for scripted paths |

Rules:

- At most **one** `>A` and **one** `>B` per map; a duplicate is a parse error.
- In normal startup parsing (`WorldMapFloor::from_ascii` / `world_map.txt`),
  these tokens still mean **road**; coordinates are not recorded.

### Wall bitmask

Each wall cell stores **which edges** of the cell carry wall geometry, as a
4-bit value 1–15:

| Bit | Value | Edge  |
|-----|-------|-------|
| N   | 1     | North |
| S   | 2     | South |
| E   | 4     | East  |
| W   | 8     | West  |

Corners and T-junctions are the same type: combine bits (e.g. north + west =
`1 + 8 = 9`). Rendering draws one thin slab per set bit.

**Doors** are not a separate token: omit the wall bit on the edge that should
open (e.g. south façade → use mask without `MASK_SOUTH`, often `__` if that was
the only bit). Facing `__` reads as a doorway onto road or another room.

### Text tokens

- **`w` + one hex digit** — explicit mask (`w1` … `w9`, `wa` … `wf`, or
  uppercase `wA` … `wF`). The digit is the value 1–15 in hex (`w0` is invalid).
- **Shortcuts** (same as single-bit hex, kept for readability):
  - `wn` → north only (`w1`)
  - `ws` → south only (`w2`)
  - `we` → east only (`w4`)
  - `ww` → west only (`w8`)

`we` is **never** interpreted as hex `e` (14); use `wE` for mask 14.

### Corner pillar (`c*`)

Thin wall slabs meet with a square void at some tile corners. A **corner pillar**
cell draws a single vertical column with footprint **`WALL_THICKNESS`²**
(0.2 m × 0.2 m, one fifth of the cell each way) and full wall height, centered
in the chosen **corner** of the 1 m cell (same inset as slab centers in
`for_each_wall_segment`).

| Token | Corner (numpad on a north-up map) |
|-------|-----------------------------------|
| `c7`  | NW (`C7` allowed)                 |
| `c9`  | NE                                |
| `c1`  | SW                                |
| `c3`  | SE                                |

Other `c*` combinations are invalid. Parsed as `CellType::Corner` in
`src/world_map.rs`; passability matches walls (blocked).

## Wall Types At Runtime

Parsed cells use `CellType::Wall(WallMask)` or `CellType::Corner(WallCorner)`;
see `src/world_map.rs` for `WallMask`, `WallCorner`, `MASK_NORTH`, `MASK_SOUTH`,
`MASK_EAST`, and `MASK_WEST`.

## Placement Rules

- The parsed map is centered into a `64x64` center chunk (the authored file can
  be up to that full size).
- Cells that fall outside chunk bounds are skipped.
- Procedural neighborhood terrain is generated first, then the center chunk is
  overwritten where the authored map has cells.

## Parse Errors

- Row has odd character count (cannot split into 2-char tokens).
- Rows are not rectangular.
- Unknown token, or wall token with mask `0` (e.g. `w0`).
- Duplicate `>A` or duplicate `>B` when using the marker-aware parser.
