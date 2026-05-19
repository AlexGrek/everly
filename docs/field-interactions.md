# Field interactions

Hypermap **fields** (dirt today; temperature and others later) are updated when actors
cross **main tile** boundaries after movement. Implementation lives in
`src/map/field_interactions.rs`; data lives in per-field hypermaps (e.g. [`DirtMap`](../src/map/dirt.rs)).

## Main tile

Defined in `docs/actor.md` § [Main tile](actor.md#main-tile). Summary:

```text
main_tile = (round(center.x), round(center.y))   // via actor_main_tile(center)
```

All gameplay that asks “which tile is the actor in?” shares [`actor_main_tile`](../src/actor/mod.rs) — including [`BlackBot`](../src/actor/black_bot.rs) path think (`BlackBotVisual.main_tile`) and field deposits (`ActorState::field_main_tile`). Collision subtiles still use `floor(center × SUBTILE_COUNT)`; do not mix the two.

[`ActorState::field_main_tile`](../src/actor/mod.rs) is updated only in field interaction systems **after** [`process_actors`](../src/actor/mod.rs) so `center` reflects the completed movement step (including off-screen [`advance_unchecked`] travel).

When `field_main_tile` was `Some(prev)` and `prev != current`, the actor **left**
`prev` — field rules apply to **`prev`**, not the destination tile.

## Frame pipeline

```text
flush_actor_occupancy → process_actors → dirt_actor_interaction → seed_dirt → flush_dirt_map → dirt overlay
```

| Step | What happens |
|------|----------------|
| `process_actors` | Think, prepare, try_move / advance_unchecked |
| `dirt_actor_interaction` | Collect main-tile transitions; deposit dirt on left tiles |
| `seed_dirt_for_visible_chunks` | One-time procedural dirt (write buffer) |
| `flush_dirt_map` | **`flush_merge` only if write buffer has chunks** |
| `update_dirt_overlay_textures` | Repaint only chunks in `take_dirty_chunks()` |

### Skip work when nothing moved

- **`dirt_actor_interaction`** returns immediately when no actor changed main tile
  (no field math, no write-buffer dirt updates).
- **`flush_dirt_map`** skips buffer merge when the write buffer is empty (no actor
  deposits and no seeding this frame).
- **Overlay** already skips GPU upload when `take_dirty_chunks()` is empty.

## Dirt rule

On each main-tile transition, add [`DIRT_TRACK_DEPOSIT`](../src/map/dirt.rs) (`0.01`) to
the **left** tile's scalar dirt value (clamped to `1.0`). Void tiles are skipped.

Dirt is stored in a **tile-only** [`DoubleBufferedHypermap<f32>`](../src/map/tile_field.rs)
(one value per world tile), not per subtile.

## Adding a new field

1. Add a hypermap resource (prefer `DoubleBufferedHypermap` for read/write parallelism).
2. Add helpers in `field_interactions.rs` (or a sibling module) that take
   `&[MainTileTransition]` or reuse `collect_main_tile_transitions`.
3. Register a system **after** `process_actors`, **before** that field's flush.
4. Gate flush and overlay on non-empty writes / dirty chunks.
5. Document the rule here and in `.claude/SKILLS/field-interactions/SKILL.md`.

## Related docs

- `docs/chunk-overlay.md` — dirt overlay rendering
- `docs/actor.md` — movement pipeline
- `docs/hypermap.md` — chunked storage
