# Cell occupancy (per-tile actor lists)

[`CellOccupancy`](../src/map/cell_occupancy.rs) stores, for **every** hypermap
cell that holds at least one actor, the **full list of entities** whose
[main tile](actor.md#main-tile) is that cell. It is the inverse of "where is this
bot?": given a tile, get everyone standing on it.

This is the lookup that turns a tile-scoped query — e.g. a collision's blocked
subtile → its tile — back into the **entities** responsible. Bots themselves only
ever learn a blocked *coordinate* from movement (`ActorMovementError`, see
[`movement.md`](movement.md)); the dynamic passability grid is flag bits, not
entities. `CellOccupancy` is the side index that recovers identity.

## Data model

```rust,ignore
#[derive(Resource, Default)]
pub struct CellOccupancy {
    cells: HashMap<IVec2, Vec<Entity>>,   // tile -> entities on it (sparse)
    entity_cell: HashMap<Entity, IVec2>,  // entity -> its recorded cell
}
```

- **Sparse:** a tile with no actors holds **no** entry. The vacated cell's `Vec`
  is removed once it empties, so the map only ever holds occupied tiles.
- **Reverse index:** `entity_cell` is the source of truth for change detection
  and removal, so both are O(1) — no scanning cells to find an entity.
- Covers **every** actor (GlitchBot + BlackBot), on-screen or off — anything with
  an `ActorObject`.

| Method | Role |
|--------|------|
| `entities_in(tile) -> &[Entity]` | Everyone on `tile` (empty slice if none). |
| `cell_of(entity) -> Option<IVec2>` | The tile an actor is recorded in. |
| `set_cell(entity, tile) -> bool` | Record/move an actor; `true` on a real change. |
| `remove(entity)` | Drop an actor (despawn / no longer an actor). |
| `tracked_len()` / `is_empty()` | Count of tracked actors. |

## Maintenance

[`track_cell_occupancy`] runs in **`Update`** (which executes after the frame's
`FixedUpdate` movement ticks, so `center` reflects the completed step):

1. drain [`RemovedComponents<ActorObject>`] → `remove` each despawned actor;
2. for every live actor, compute `actor_main_tile(center)` and `set_cell`.

`set_cell` mutates the map **only on a genuine cell change** (the new tile differs
from the recorded one, or the actor is new). In steady state — no bot crossed a
cell boundary — the system does pure hash lookups and **allocates nothing**; list
growth happens only on an actual insert/move.

Gated on `GameState::InGame` only (not on pause): when paused nothing moves, so the
map is stable, but the initial population still happens as soon as actors exist.

## Notes / future

- Within a cell, entity order follows query (archetype) order and is not
  semantically meaningful — treat the list as a set.
- Iteration order over `cells` is not deterministic (hash map); no gameplay
  decision currently iterates all cells, so this is not a determinism concern. If
  one does, sort or key by `IVec2` first.
- The map is **not persisted** — it is rebuilt from actor positions each session.

## Related code

| Concern | Location |
|---------|----------|
| Resource + system + plugin | [`src/map/cell_occupancy.rs`](../src/map/cell_occupancy.rs) |
| Main tile (`round(center)`) | [`actor_main_tile`](../src/actor/mod.rs), [`actor.md`](actor.md#main-tile) |
| Collision error (coordinate only) | [`ActorMovementError`](../src/actor/mod.rs), [`movement.md`](movement.md) |
| Sibling sparse per-tile store (chargers) | [`interactive-entities.md`](interactive-entities.md) |
