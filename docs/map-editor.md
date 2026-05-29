# In-game map editor

Runtime tool for painting **hypermap** tiles under the cursor: void, road, bitmask walls, closed wall outlines (**Room**), and corner pillars. Implemented as `MapEditPlugin` in `src/edit/map_edit.rs`, wired from `GamePlugin` in `src/lib.rs` after `HypermapWorldPlugin` in `src/map/hypermap_world.rs`.

This editor updates the **live hypermap** chunk data (see `hypermap.md`) and triggers a **chunk mesh rebuild**. It does **not** write `world_map.txt` or `world_map_floor1.txt`; those files are still the authored startup overlay for the center chunk only.

## Enabling the palette

1. Press **Edit** in the bottom HUD (next to **Map**). The label toggles between `Edit` and `Edit ✓`.
2. A **palette strip** appears just above the 52 px bottom bar: tile brushes, **Save**, and **Re-gen**.
3. Press **Edit** again to close the palette. Closing also clears any active placement brush.

HUD wiring for the toggle lives in `src/hud/game_hud.rs`; the palette root is spawned by `spawn_map_edit_palette` in `src/edit/map_edit.rs` (scheduled after `spawn_bottom_hud` in `GamePlugin`).

## Placement workflow

1. With the palette open, click a tile type (e.g. **Wall**). You enter **placement mode**: `MapEditState.placement_tile` is set and the wall **variant** resets to index `0`.
2. Move the mouse over the world. A **semi-transparent lime unlit** preview mesh shows what would be painted on the **active floor** (see `ActiveFloorLevel` in `src/map/floor_level.rs` and plane height `floor * HYPERMAP_FLOOR_HEIGHT`).
3. **Left mouse down** stores the start tile; **left mouse up** commits the stroke (see below). Writes go through `write_world_cell` in `src/map/hypermap_world.rs` (world map + static passability); affected chunks are queued on `HypermapChunkRemeshQueue` and `render_chunks_30fps` re-bakes meshes when the queue is drained.
4. **Right-click** to leave placement mode (brush cleared) so you can pick another palette entry without turning Edit off.

Stroke rules (world grid `(x, z)`):

- **Wall:** strictly orthogonal segment from start to end. Compare `|Δx|` and `|Δz|` from start; the axis with the **larger** span gets the line (constant `z` from start when `|Δx| > |Δz|`, constant `x` from start when `|Δz| > |Δx|`). If `|Δx| == |Δz|` (including diagonal endpoints), the stroke is the **horizontal** segment at start `z`. One tile if start equals end.
- **Void / Road:** axis-aligned **rectangle** (filled) between start and end on mouse up. New “floor” palette kinds should extend `MapTileKind`, `stroke_world_cells`, and `map_edit_update_preview` in `map_edit.rs` the same way.
- **Room:** same drag as void/road, but only the **rectangle border** is written. Each border cell gets a [`WallMask`](tilemap.md) on the **outer** sides of the selection (`perimeter_wall_mask` in `map_edit.rs`), consistent with the world-space rules in `tilemap.md` § Wall bitmask. The interior is left unchanged.
- **Corner:** single pillar at the **mouse-up** cell (variant still from the wheel).

Releasing over the HUD dead zone or with no valid ray cancels the stroke (nothing written). The preview entity does not modify picking or existing meshes until mouse up.

## Mouse wheel variants

| Palette type | Wheel behavior |
|--------------|----------------|
| **Wall** | Cycles bitmask **1 … 15** (same numeric masks as hex `w1`…`wF` in [`tilemap.md`](tilemap.md)). Order is variant index modulo 15, mapped to `bits = (index % 15) + 1`. |
| **Corner** | Cycles pillar corners **NW → NE → SW → SE** (same semantics as `c7` / `c9` / `c1` / `c3`). |
| **Void**, **Road**, **Room** | No variants. The wheel does **not** reserve placement input for these types. |

While placing **Wall** or **Corner**, the **strategy camera zoom** is disabled so the wheel only changes the variant (`zoom_camera` in `src/scene/camera.rs` checks `MapEditState`). With **Void**, **Road**, or **Room** selected, zoom behaves normally.

## UI chrome guard

Hover and **stroke start / end** (mouse down / up over the map) are ignored when the cursor is in the **bottom ~120 px** of the window (bottom HUD + palette). That avoids accidental paints when using **Map**, **Edit**, floor **+/−**, or palette buttons.

## Coordinate system

Hover uses a ray from the **strategy camera** through the cursor, intersected with a horizontal plane at the active floor’s `y`. Floor indices match the hypermap: world `x` / `z` integer floors are the tile column / row used by `world_to_chunk_local` in `src/map/hypermap.rs` and chunk rendering (see `hypermap.md`).

## Re-generate

The palette **Re-gen** button targets the **camera’s current chunk** (same chunk index as
`StrategyCamera` focus → `world_to_chunk_local`):

1. Despawns every actor whose main tile lies on that chunk (clears dynamic footprints first).
2. Overwrites the chunk with a **fresh procedural** layout (`fill_procedural_chunk` via
   [`regenerate_procedural_chunk`](../src/map/hypermap_world.rs)) — no reload from
   `levels/.../geometry/`, no `world_map.txt` overlay.
3. Resets floor/wall styles, dirt, and temperature for that chunk, then re-seeds dirt/temperature.
4. Queues a chunk remesh.

Does **not** write to disk; use **Save** to persist.

## Save

The palette **Save** button is the **only** persistence path (no autosave). It flushes
dirt/temperature write buffers, calls `save_full_generated_level` in `src/map/level.rs`,
then writes `actors.json` and `camera.json`.

Full layout, load order, binary `EVTF` format, chunk union rules, and what is *not*
saved are documented in [`level-persistence.md`](level-persistence.md).

## Related code

| Piece | Role |
|-------|------|
| `MapEditState` | `panel_open`, `placement_tile` |
| `MapEditDragAnchor` | Start tile for in-progress wall line / floor rectangle / room outline |
| `room_outline_cells` / `perimeter_wall_mask` | Border tiles and outward-facing `WallMask` for **Room** |
| `HypermapChunkRemeshQueue` | Chunks to re-bake after edits |
| `queue_hypermap_chunk_remesh` | Enqueue by world tile |
| `ensure_chunk_generated` | Ensures chunk exists (level geometry file, else procedural + center overlay) before edits |
| `build_floor0_*` / `build_upper_*` | Shared mesh builders for preview and chunk bake |

For encoding of wall bits and corner tokens in ASCII maps, see [`tilemap.md`](tilemap.md). For how chunks are meshed and updated over time, see [`rendering-pipeline.md`](rendering-pipeline.md). Spawning actors (GlitchBot/BlackBot) is a separate tool — see [`actor-spawner.md`](actor-spawner.md) (its **Actors** HUD toggle sits next to **Edit**).
