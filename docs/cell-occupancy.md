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
    cells: HashMap<IVec2, Vec<Entity>>,        // tile -> entities on it (sparse)
    info: HashMap<Entity, BotKinematics>,      // entity -> its kinematics (incl. tile)
}

#[derive(Clone, Copy)]
pub struct BotKinematics {
    pub tile: IVec2,            // main tile (the `cells` key)
    pub center: Vec2,           // tile-space center
    pub heading: Vec2,          // movement direction (unit; ZERO = none)
    pub radius_subtiles: i32,   // footprint radius
}
```

- **Sparse:** a tile with no actors holds **no** entry. The vacated cell's `Vec`
  is removed once it empties, so the map only ever holds occupied tiles.
- **Reverse index:** `info` is the source of truth for change detection and
  removal (both O(1)) **and** the per-bot motion read API. The `cells` map only
  mutates on a **tile change**; the kinematics value is refreshed in place every
  frame (no allocation).
- Covers **every** actor (on-screen or off) — anything with an `ActorObject`.

| Method | Role |
|--------|------|
| `entities_in(tile) -> &[Entity]` | Everyone on `tile` (empty slice if none). |
| `cell_of(entity) -> Option<IVec2>` | The tile an actor is recorded in. |
| `kinematics_of(entity) -> Option<BotKinematics>` | An actor's position/heading/size. |
| `resolve_blocker(world_subtile, exclude) -> Option<(Entity, BotKinematics)>` | The bot occupying a (collision) subtile. |
| `update(entity, BotKinematics) -> bool` | Record/refresh an actor; `true` on a cell change. |
| `remove(entity)` | Drop an actor (despawn / no longer an actor). |
| `tracked_len()` / `is_empty()` | Count of tracked actors. |

### Resolving a collision to the bot that caused it

`resolve_blocker(world_subtile, exclude)` turns a blocked subtile (e.g. the
coordinate in `ActorMovementError::BlockedByOccupancy`) into the **bot** standing
there. It converts the subtile to its tile, scans that tile **plus its eight
neighbours** (a footprint can spill one tile past the bot's main cell), and returns
the nearest-centered candidate whose body can plausibly reach the subtile
(`dist ≤ (radius + 1) subtiles`), skipping `exclude` (the querying bot). It is the
basis of the [identity-aware bot-on-bot collision response](actor-brain.md#bot-on-bot-collision-response):
the responder reads the resolved bot's `heading` to tell whether it struck that
bot's front (head-on) or back (rear-ended it). Allocation-free — a fixed 3×3 scan
picking the minimum-distance candidate.

## Maintenance

[`track_cell_occupancy`] runs in **`Update`** (which executes after the frame's
`FixedUpdate` movement ticks, so `center` reflects the completed step and `heading`
is the value the brain published this frame):

1. drain [`RemovedComponents<ActorObject>`] → `remove` each despawned actor;
2. for every live actor, build its [`BotKinematics`] (`actor_main_tile(center)` +
   center/heading/radius) and `update`.

`update` moves an entry between `cells` **only on a genuine tile change** (the new
tile differs from the recorded one, or the actor is new); the kinematics value is
refreshed in place every frame. In steady state the system does pure hash lookups
and **allocates nothing**; list growth happens only on an actual insert/move.

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
