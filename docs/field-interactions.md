# Field interactions

Hypermap **fields** (dirt today; temperature and others later) are updated when actors
cross **main tile** boundaries after movement. Implementation lives in
`src/map/field_interactions.rs`; data lives in per-field hypermaps (e.g. [`DirtMap`](../src/map/dirt.rs)).

## Main tile

Defined in `docs/actor.md` § [Main tile](actor.md#main-tile). Summary:

```text
main_tile = (round(center.x), round(center.y))   // via actor_main_tile(center)
```

All gameplay that asks “which tile is the actor in?” shares [`actor_main_tile`](../src/actor/mod.rs) — including field deposits (`ActorState::field_main_tile`). [`BlackBot`](../src/actor/black_bot.rs) path following uses float `center` distance to each waypoint's tile center, not main-tile equality. Collision subtiles still use `floor(center × SUBTILE_COUNT)`; do not mix the two.

[`ActorState::field_main_tile`](../src/actor/mod.rs) is updated only in field interaction systems **after** [`process_actor_moves`](../src/actor/movement.rs) so `center` reflects the completed movement step (including off-screen [`advance_unchecked`] travel).

When `field_main_tile` was `Some(prev)` and `prev != current`, the actor **left**
`prev` — field rules apply to **`prev`**, not the destination tile.

## Frame pipeline

```text
flush_actor_occupancy → process_actor_moves → dirt_actor_interaction → seed_dirt → flush_dirt_map → dirt overlay
                      → bot_occupancy_heat → seed_temperature → flush_temperature_map → temperature overlay / GPU diffusion
```

| Step | What happens |
|------|----------------|
| `process_actor_moves` | Think, propose (static-only), owner-grid resolution, apply outcomes, re-entry placement (sequential) |
| `dirt_actor_interaction` | Collect main-tile transitions; exchange dirt between each actor and its left tile |
| `bot_occupancy_heat` | Every 1 s, +3 °C on each **current** main tile occupied by a bot (write buffer; deduped per tile) |
| `seed_dirt_for_visible_chunks` | One-time procedural dirt (write buffer) |
| `flush_dirt_map` | **`flush_merge` only if write buffer has chunks** |
| `update_dirt_overlay_textures` | Repaint only chunks in `take_dirty_chunks()` |

### Skip work when nothing moved

- **`dirt_actor_interaction`** only touches the dirt write buffer for actors that
  changed main tile (and only in the cleaner-floor branch); actors that did not
  move tiles do no field math.
- **`flush_dirt_map`** skips buffer merge when the write buffer is empty (no actor
  deposits and no seeding this frame).
- **Overlay** already skips GPU upload when `take_dirty_chunks()` is empty.

## Dirt rule

Every actor carries its own [`ActorState::dirtiness`](../src/actor/mod.rs) in `0.0..=1.0`
and **spawns clean** (`0.0`; not serialized, so a loaded actor starts clean again). On
each main-tile transition the actor **exchanges** dirt with the tile it just **left**
([`dirt_exchange`](../src/map/field_interactions.rs), rate
[`DIRT_TRACK_DEPOSIT`](../src/map/dirt.rs) = `0.05` = 5%):

| Floor vs actor | Effect |
|----------------|--------|
| Floor **cleaner** than actor | Actor wipes `5%` of its dirtiness onto the tile (capped so it never goes below `0.0`); the tile gains exactly what the actor lost (conserved). |
| Floor **dirtier** than actor | Actor picks up `5%` *of the floor's* dirtiness (clamped to `1.0`); the tile is unchanged. |
| Equal, or **Void** tile | No-op. |

Dirt is stored in a **tile-only** [`DoubleBufferedHypermap<f32>`](../src/map/tile_field.rs)
(one value per world tile), not per subtile. The actor's dirtiness lives on the actor.

### Cleaner override

A **cleaner bot in cleaning mode** ([`Cleaner::cleaning`](../src/actor/black_bot.rs),
set by [`GoClean`](actor-brain.md#high-level-actions)) bypasses the exchange entirely:
`dirt_actor_interaction` **zeroes the dirt of the cell it currently occupies every
frame** (not just on a transition) and keeps the bot's own `dirtiness` at `0.0`. So a
cleaning bot fully scrubs every cell it crawls across — including the dirty cell it was
routed to and stops on — instead of merely tracking 5% around. `Void` tiles are skipped.

**Persistence:** deposits and procedural seeds stay in memory until the player uses
map editor **Save**, which writes `levels/level_{name}/dirt.bin` (all loaded dirt chunks).
See [`level-persistence.md`](level-persistence.md).

## Adding a new field

1. Add a hypermap resource (prefer `DoubleBufferedHypermap` for read/write parallelism).
2. Add helpers in `field_interactions.rs` (or a sibling module) that take
   `&[MainTileTransition]` or reuse `collect_main_tile_transitions`.
3. Register a system **after** `process_actor_moves`, **before** that field's flush.
4. Gate flush and overlay on non-empty writes / dirty chunks.
5. Document the rule here and in `.claude/SKILLS/field-interactions/SKILL.md`.

## Temperature

Temperature is a sibling tile field ([`TemperatureMap`](../src/map/temperature.rs), same
[`TileFieldMap`](../src/map/tile_field.rs) backing as dirt). It **diffuses on the GPU** every frame:
heat spreads across tiles (seamlessly over chunk borders), insulated by walls/void, relaxing
toward ambient. The CPU field stays authoritative — results are read back and applied via
[`TileFieldMap::apply_window_to_read`](../src/map/tile_field.rs), which marks chunks dirty so the
overlay repaints. See [`temperature-diffusion.md`](temperature-diffusion.md).

### Bot occupancy heating

[`bot_occupancy_heat`](../src/map/field_interactions.rs) runs after [`process_actor_moves`](../src/actor/movement.rs)
on a **1 s** repeating timer ([`BOT_OCCUPANCY_HEAT_INTERVAL_S`](../src/map/temperature.rs)). Each
tick adds [`BOT_OCCUPANCY_HEAT_DELTA_C`](../src/map/temperature.rs) (**+3 °C**) to every **current**
main tile that holds at least one bot ([`actor_main_tile`](../src/actor/mod.rs)); multiple bots on
the same tile still heat it once. `Void` tiles are skipped. Writes use the temperature **write**
buffer and are merged by [`flush_temperature_map`](../src/map/temperature.rs) before overlays and
GPU diffusion read the field.

## Related docs

- `docs/temperature-diffusion.md` — GPU temperature spread
- `docs/chunk-overlay.md` — dirt overlay rendering
- `docs/actor.md` — movement pipeline
- `docs/hypermap.md` — chunked storage
