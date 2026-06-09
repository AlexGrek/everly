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
- **Vertical spacing between floor planes** is **`HYPERMAP_FLOOR_HEIGHT`** in `src/map/floor_level.rs` — currently **wall height + 0.03 m** so the next storey’s floor mesh sits slightly above wall tops and avoids z-fighting.

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

Each wall cell stores **which slab directions** to draw from the cell center, as a
4-bit value 1–15. Names (`N`/`S`/… and tokens `wn`/`ws`/…) are **historic labels**; **placement**
follows world axes in `for_each_wall_segment` in `src/map/world_map.rs`.

| Bit | Value | ASCII alias | Slab in world **XZ** (cell center = tile midpoint; +Y is up) |
|-----|-------|-------------|------------------------------------------------------------------|
| N   | 1     | `wn`, `w1`  | Full width along **X**, offset toward **−Z** (smaller `z`).      |
| S   | 2     | `ws`, `w2`  | Full width along **X**, offset toward **+Z**.                  |
| E   | 4     | `we`, `w4`  | Full depth along **Z**, offset toward **+X**.                  |
| W   | 8     | `ww`, `w8`  | Full depth along **Z**, offset toward **−X**.                  |

Corners and T-junctions combine bits (e.g. N+W = `1 + 8 = 9`). Mesher draws **one thin slab per set bit**
(thickness `WALL_THICKNESS` in `world_map.rs`).

**Doors** are not a separate token: omit the bit for the side that should stay open. Example:
omit `MASK_SOUTH` to leave the cell’s **+Z** side open toward the neighboring tile (often `__`
if that was the only bit). Facing `__` reads as a doorway onto road or another room.
Procedurally generated doors are **2 tiles wide** (two adjacent cells with the shared slab removed),
with a 1-wide fallback for degenerate geometry. Both door cells are stored in `HouseEntrypoint`
(`wall_x`/`wall_z` and `wall2`) in the chunk metadata.

### Text tokens

- **`w` + one hex digit** — explicit mask (`w1` … `w9`, `wA` … `wF`).
  The digit is the value 1–15 in hex (`w0` is invalid). **Hex letters
  must be uppercase**: lowercase `wa` … `wf` are rejected (`InvalidToken`),
  so the parser never has to disambiguate them from the single-edge
  aliases below.
- **Shortcuts** (single-edge aliases, always lowercase):
  - `wn` → north only (`w1`)
  - `ws` → south only (`w2`)
  - `we` → east only (`w4`)
  - `ww` → west only (`w8`)

The case rule is the structural distinction: **uppercase letter = explicit
hex bitmask, lowercase letter = single-edge alias.** So `we` is **never**
interpreted as hex `e` (14) — write `wE` for mask 14.

### Corner pillar (`c*`)

Thin wall slabs meet with a square void at some tile corners. A **corner pillar**
cell draws a single vertical column with footprint **`WALL_THICKNESS`²**
(0.2 m × 0.2 m, one fifth of the cell each way) and full wall height, centered
in the chosen **corner** of the 1 m cell (same inset as slab centers in
`for_each_wall_segment`).

| Token | Corner in **XZ** (−X/+X vs −Z/+Z from cell center; +Y up) |
|-------|-------------------------------------------------------------|
| `c7`  | NW (`C7` allowed)                                           |
| `c9`  | NE                                                          |
| `c1`  | SW                                                          |
| `c3`  | SE                                                          |

Other `c*` combinations are invalid. Parsed as `CellType::Corner` in
`src/map/world_map.rs`; passability matches walls (blocked).

Procedural placement at concave union elbows: **`docs/corners.md`**.

### Charging station (`cn` / `cs` / `ce` / `cw`)

A **charging station** is a **walkable** cell (passable like `__` road) that renders
extra shapes on top of the normal floor quad: an **elevated metal pad** (inset,
raised ~0.08 m), a **glowing-blue cube**, and a bulky **matte-black transformer box**
(larger than the cube) that bridges the 4-subtile (0.8 m) gap to the backing wall's
slab — which sits on the *outer* edge of the neighboring wall cell — so the unit reads
as bolted to the wall, with the glowing cube mounted on its front. The second letter is
the **facing** — which wall edge the charger backs onto; the cube faces into the room.

| Token | Backing wall (cube edge) in **XZ** |
|-------|-------------------------------------|
| `cn`  | North (−Z) — cube on the −Z edge   |
| `cs`  | South (+Z)                         |
| `ce`  | East (+X)                          |
| `cw`  | West (−X)                          |

Uppercase (`CN` … `CW`) is also accepted. The `c` + **letter** form never collides
with the `c` + **digit** corner pillars. Parsed as `CellType::Charger(ChargerFacing)`
in `src/map/world_map.rs`; meshing lives in `src/map/hypermap_world.rs`
(`build_*_charger_metal_mesh` / `build_*_charger_glow_mesh`), see `docs/rendering-pipeline.md`.
Procedural placement (one per house, against an interior wall, not in a corner) is in
`src/map/map_generator/step_charging_stations.rs` — see `docs/map-generator.md`.

## Wall Types At Runtime

Parsed cells use `CellType::Wall(WallMask)` or `CellType::Corner(WallCorner)`;
see `src/map/world_map.rs` for `WallMask`, `WallCorner`, `MASK_NORTH`, `MASK_SOUTH`,
`MASK_EAST`, and `MASK_WEST`.

## Placement Rules

- The parsed map is centered into a `128x128` center chunk (the authored file can
  be up to that full size).
- Cells that fall outside chunk bounds are skipped.
- Procedural neighborhood terrain is generated first, then the center chunk is
  overwritten where the authored map has cells.

## Style Layer

Each chunk has **two independent style files** stored alongside its geometry:

| File | Controls |
|------|---------|
| `levels/level_{name}/style_floor/{x}_{y}.txt` | Floor quad material for every cell |
| `levels/level_{name}/style_wall/{x}_{y}.txt` | Wall slab material for Wall / Corner cells |

Both files use the same `# floor N` section headers and space-separated 2-char tokens per row.
Floors that are entirely default are omitted; if a file is absent the chunk uses default materials.
The editor **Save** button writes both files.

### Floor Style Tokens (`style_floor`)

Applies to the horizontal floor quad rendered under **every** cell type (road and wall).

| Token | Meaning |
|-------|---------|
| `..`  | Default (same as `fr`) |
| `fr`  | Floor **Road** — default dark asphalt |
| `fg`  | Floor **Glass** — reflective dark glass |
| `fp`  | Floor **Pavement** — grey |
| `fm`  | Floor **Marble** — white, reflective |

### Wall Style Tokens (`style_wall`)

Applies only to the vertical slab geometry of **Wall** and **Corner** cells.

| Token | Meaning |
|-------|---------|
| `..`  | Default (same as `wr`) |
| `wr`  | Wall **Regular** — default opaque material |
| `wg`  | Wall **Glass** — semi-transparent blue-tinted glass |

### In-Game Editing

The palette bar shows both active styles. Two independent Tab bindings cycle them:

| Key | Cycles |
|-----|--------|
| **Tab** | Floor style: `..` → `fg` → `fp` → `fm` |
| **Shift+Tab** | Wall style: `wr` → `wg` (only for Wall / Room / Corner brushes) |

Applies per brush:

| Brush | Floor Tab | Wall Shift+Tab |
|-------|-----------|----------------|
| Wall  | yes | yes |
| WallG | yes | fixed `wg` |
| Room  | yes | yes |
| Corner | yes | yes |
| Fill  | yes | — |
| Charger | — | — |
| Road  | yes | — |
| Others | — | — |

Scroll-wheel on the Wall / WallG / Corner brush cycles the wall mask (1–15 bits);
on the **Charger** brush it cycles the facing (N → E → S → W).
**Fill**, **Void**, **Road**, and **Room** have no variants — the wheel zooms normally with those brushes active.

## Parse Errors

- Row has odd character count (cannot split into 2-char tokens).
- Rows are not rectangular.
- Unknown token, or wall token with mask `0` (e.g. `w0`).
- Lowercase hex letter in a `w` token (e.g. `wa`, `wf`); use uppercase `wA` … `wF`.
- Duplicate `>A` or duplicate `>B` when using the marker-aware parser.
