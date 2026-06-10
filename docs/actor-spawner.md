# In-game actor spawner

Runtime tool for dropping actors (**BlackBot**) onto the active floor at the
clicked tile. Implemented as `ActorSpawnPlugin` in `src/edit/actor_spawn.rs`,
wired from `GamePlugin` in `src/lib.rs`. It is **independent** from the tile
[map editor](map-editor.md) (`MapEditPlugin`): its own HUD toggle and palette strip.

## Enabling the palette

1. Press **Actors** in the bottom HUD (next to **Edit**). The label toggles between
   `Actors` and `Actors *`.
2. A **palette strip** appears above the map-edit palette (window bottom `92 px`,
   `40 px` tall): **Bot** (BlackBot), **Kill**, and **Resurrect all**.
3. Press **Actors** again to close the palette. Closing also clears the active tool.

HUD wiring for the toggle lives in `src/hud/game_hud.rs` (`ActorSpawnToggleButton` /
`ActorSpawnToggleLabel`); the palette root is spawned by `spawn_actor_spawn_palette`
(scheduled after `spawn_bottom_hud` in `GamePlugin`).

## Spawn workflow

1. With the palette open, click **Bot**. `ActorSpawnState.tool` is set to
   `ActorTool::Spawn(ActorKind::BlackBot)`.
2. Move the mouse over the world. A semi-transparent preview plane marks the target
   cell center on the **active floor** (`ActiveFloorLevel`, plane height
   `floor * HYPERMAP_FLOOR_HEIGHT`). **Lime** (`MapEditPreviewMaterial`) means the
   footprint fits at that tile; **red** (`ActorSpawnPreviewInvalidMaterial`) means
   the placement is blocked (walls/void).
3. **Left-click** (on release) spawns the actor at the hovered tile center when the
   preview is lime. Red tiles ignore the click.
   (`tile + 0.5`). The spawn goes through `black_bot::spawn_black_bot` and
   consumes the actor's seeded RNG resource (`BlackBotRng`), so placements stay
   reproducible per seed.
4. **Right-click** to leave placement mode (tool cleared) without closing the palette.

There is no drag — each click drops one actor. Clicks in the **bottom `120 px`** of the
window (HUD bar + both palettes) are ignored (`ACTOR_DEAD_ZONE_PX`) so palette/HUD
buttons never spawn actors underneath.

## Resurrect all

Click **Resurrect all** to recover **every** actor in the world in one shot (no
brush mode, no world click):

- **Charge** on each actor is raised to at least **30%** (never lowered).
- **BlackBot** broken sub-components (`movement engine`, `control plane`,
  `sensory system`) are repaired (`broken` cleared; wear is kept).
- Every **BlackBot** brain is reset so bots replan on the next tick.
- Movement intent is cleared on all actors.

Implemented in [`resurrect_all_actors`](../src/actor/resurrect.rs), triggered by
[`resurrect_all_button`](../src/actor/resurrect.rs) from the palette.

## Kill workflow

1. With the palette open, click **Kill**. `ActorSpawnState.tool` is set to
   `ActorTool::Kill`. No preview plane is shown.
2. **Left-click** any bot entity in the world to despawn it immediately. The dynamic
   occupancy footprint clears automatically on the next flush.
3. **Right-click** to disarm Kill mode without closing the palette.

## Mutual exclusion with the tile brush

The actor tool and the map-editor tile brush are mutually exclusive: picking an actor
tool clears `MapEditState.placement_tile`, and picking a tile clears
`ActorSpawnState.tool`. This guarantees a single click never both paints a tile and
spawns/kills an actor. The two palettes can be open at the same time, stacked vertically.

## Persistence

The spawner does **not** save. Actors are persisted by the map editor's **Save** button
(the single level-save path), which writes `actors.yaml` via `save_level_actors`. See
[`level-persistence.md`](level-persistence.md) and [`map-editor.md`](map-editor.md) § Save.
Re-gen in the map editor despawns actors on the regenerated chunk.

## Related code

| Piece | Role |
|-------|------|
| `ActorSpawnState` | `panel_open`, `tool: Option<ActorTool>` |
| `ActorTool` | `Spawn(ActorKind)` / `Kill` |
| `ActorKind` | `BlackBot` |
| `actor_spawn_pointer_click` | Spawns the chosen actor on left-mouse-up (spawn mode only) |
| `on_actor_pointer_click` (`actor_inspector.rs`) | Despawns the clicked bot when Kill mode is armed |
| `resurrect_all_button` (`resurrect.rs`) | Bulk-repairs all actors when **Resurrect all** is pressed |
| `actor_spawn_plane_cell` | Cursor → active-floor tile (reuses `ray_intersect_horizontal_plane`) |
| `actor_spawn_cell_passable` | Footprint probe via `DynamicPassabilityMap::probe_footprint` (per `ActorKind` blocked flags) |

For the actor runtime itself (trait, movement, collision), see [`actor.md`](actor.md).
