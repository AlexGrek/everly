# In-game actor spawner

Runtime tool for dropping actors (**GlitchBot**, **BlackBot**) onto the active floor
at the clicked tile. Implemented as `ActorSpawnPlugin` in `src/edit/actor_spawn.rs`,
wired from `GamePlugin` in `src/lib.rs`. It is **independent** from the tile
[map editor](map-editor.md) (`MapEditPlugin`): its own HUD toggle and palette strip.

## Enabling the palette

1. Press **Actors** in the bottom HUD (next to **Edit**). The label toggles between
   `Actors` and `Actors *`.
2. A **palette strip** appears above the map-edit palette (window bottom `92 px`,
   `40 px` tall): **Bot** (GlitchBot) and **Black** (BlackBot).
3. Press **Actors** again to close the palette. Closing also clears the active brush.

HUD wiring for the toggle lives in `src/hud/game_hud.rs` (`ActorSpawnToggleButton` /
`ActorSpawnToggleLabel`); the palette root is spawned by `spawn_actor_spawn_palette`
(scheduled after `spawn_bottom_hud` in `GamePlugin`).

## Spawn workflow

1. With the palette open, click **Bot** or **Black**. `ActorSpawnState.placement` is set.
2. Move the mouse over the world. A **semi-transparent lime** preview plane (shared
   `MapEditPreviewMaterial`) marks the target cell center on the **active floor**
   (`ActiveFloorLevel`, plane height `floor * HYPERMAP_FLOOR_HEIGHT`).
3. **Left-click** (on release) spawns the actor at the hovered tile center
   (`tile + 0.5`). The spawn goes through `glitch_bot::spawn_glitch_bot` /
   `black_bot::spawn_black_bot` and consumes the actor's seeded RNG resource
   (`GlitchBotRng` / `BlackBotRng`), so placements stay reproducible per seed.
4. **Right-click** to leave placement mode (brush cleared) without closing the palette.

There is no drag — each click drops one actor. Clicks in the **bottom `140 px`** of the
window (HUD bar + both palettes) are ignored (`ACTOR_DEAD_ZONE_PX`) so palette/HUD
buttons never spawn actors underneath.

## Mutual exclusion with the tile brush

The actor brush and the map-editor tile brush are mutually exclusive: picking an actor
clears `MapEditState.placement_tile`, and picking a tile clears
`ActorSpawnState.placement`. This guarantees a single click never both paints a tile and
spawns an actor. The two palettes can be open at the same time, stacked vertically.

## Persistence

The spawner does **not** save. Actors are persisted by the map editor's **Save** button
(the single level-save path), which writes `actors.json` via `save_level_actors`. See
[`level-persistence.md`](level-persistence.md) and [`map-editor.md`](map-editor.md) § Save.
Re-gen in the map editor despawns actors on the regenerated chunk.

## Related code

| Piece | Role |
|-------|------|
| `ActorSpawnState` | `panel_open`, `placement: Option<ActorKind>` |
| `ActorKind` | `GlitchBot` / `BlackBot` |
| `actor_spawn_pointer_click` | Spawns the chosen actor on left-mouse-up |
| `actor_spawn_plane_cell` | Cursor → active-floor tile (reuses `ray_intersect_horizontal_plane`) |

For the actor runtime itself (trait, movement, collision), see [`actor.md`](actor.md).
