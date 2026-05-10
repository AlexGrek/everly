# In-game map editor

Runtime tool for painting **hypermap** tiles under the cursor: void, road, bitmask walls, and corner pillars. Implemented as `MapEditPlugin` in `src/map_edit.rs`, wired from `GamePlugin` in `src/lib.rs` after `HypermapWorldPlugin` in `src/hypermap_world.rs`.

This editor updates the **live hypermap** chunk data (see `hypermap.md`) and triggers a **chunk mesh rebuild**. It does **not** write `world_map.txt` or `world_map_floor1.txt`; those files are still the authored startup overlay for the center chunk only.

## Enabling the palette

1. Press **Edit** in the bottom HUD (next to **Map**). The label toggles between `Edit` and `Edit ✓`.
2. A **palette strip** appears just above the 52 px bottom bar: buttons **Void**, **Road**, **Wall**, **Corner**.
3. Press **Edit** again to close the palette. Closing also clears any active placement brush.

HUD wiring for the toggle lives in `src/game_hud.rs`; the palette root is spawned by `spawn_map_edit_palette` in `src/map_edit.rs` (scheduled after `spawn_bottom_hud` in `GamePlugin`).

## Placement workflow

1. With the palette open, click a tile type (e.g. **Wall**). You enter **placement mode**: `MapEditState.placement_tile` is set and the wall **variant** resets to index `0`.
2. Move the mouse over the world. A **semi-transparent lime, emissive** preview mesh shows the geometry that would be placed at the hovered **integer grid cell** `(x, z)` on the **active floor** (see `ActiveFloorLevel` in `src/floor_level.rs` and plane height `floor * HYPERMAP_FLOOR_HEIGHT`).
3. **Left-click** to commit: the cell is written with `Hypermap::set_floor` in `src/hypermap.rs`, the owning chunk is queued on `HypermapChunkRemeshQueue` in `src/hypermap_world.rs`, and `render_chunks_30fps` there re-bakes that chunk’s meshes when the queue is drained.
4. **Right-click** to leave placement mode (brush cleared) so you can pick another palette entry without turning Edit off.

The preview is a separate entity; it does not modify picking or existing meshes until you left-click.

## Mouse wheel variants

| Palette type | Wheel behavior |
|--------------|----------------|
| **Wall** | Cycles bitmask **1 … 15** (same numeric masks as hex `w1`…`wF` in [`tilemap.md`](tilemap.md)). Order is variant index modulo 15, mapped to `bits = (index % 15) + 1`. |
| **Corner** | Cycles pillar corners **NW → NE → SW → SE** (same semantics as `c7` / `c9` / `c1` / `c3`). |
| **Void**, **Road** | No variants. The wheel does **not** reserve placement input for these types. |

While placing **Wall** or **Corner**, the **strategy camera zoom** is disabled so the wheel only changes the variant (`zoom_camera` in `src/camera.rs` checks `MapEditState`). With **Void** or **Road** selected, zoom behaves normally.

## UI chrome guard

Hover and **placement clicks** are ignored when the cursor is in the **bottom ~120 px** of the window (bottom HUD + palette). That avoids accidental paints when using **Map**, **Edit**, floor **+/−**, or palette buttons.

## Coordinate system

Hover uses a ray from the **strategy camera** through the cursor, intersected with a horizontal plane at the active floor’s `y`. Floor indices match the hypermap: world `x` / `z` integer floors are the tile column / row used by `world_to_chunk_local` in `src/hypermap.rs` and chunk rendering (see `hypermap.md`).

## Persistence and scope

- **In memory only** for edited cells: data lives in the `HypermapRuntime` resource (`Arc<Hypermap<CellType>>` in `src/hypermap_world.rs`) after chunks are generated or touched.
- **Remesh** applies to whichever **chunk** contains `(x, z)`; the visible set is still driven by the strategy camera and chunk visibility rules in `hypermap_world`.
- To ship a layout as a file, continue to author or export **`world_map.txt`** (and paired **`world_map_floor1.txt`**) per [`tilemap.md`](tilemap.md); the in-game editor is a convenience for iterating on the loaded world, not a map file exporter.

## Related code

| Piece | Role |
|-------|------|
| `MapEditState` | `panel_open`, `placement_tile` |
| `HypermapChunkRemeshQueue` | Chunks to re-bake after edits |
| `queue_hypermap_chunk_remesh` | Enqueue by world tile |
| `ensure_chunk_generated` | Ensures chunk exists (procedural + file overlay) before `set_floor` |
| `build_floor0_*` / `build_upper_*` | Shared mesh builders for preview and chunk bake |

For encoding of wall bits and corner tokens in ASCII maps, see [`tilemap.md`](tilemap.md). For how chunks are meshed and updated over time, see [`rendering-pipeline.md`](rendering-pipeline.md).
