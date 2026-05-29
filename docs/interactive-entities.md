# Interactive Entities

A sparse, per-tile store of **reference-type** gameplay objects (chargers
today) that actors interact with. Implementation: `src/map/interactive_entity.rs`.

Interactive entities are **not** tiles. A [`CellType`](tilemap.md) is a dense
value baked into every hypermap cell; an interactive entity is *sparse* (a few
per chunk) and *stateful* (charge level, occupancy, an `is_used` flag mutate at
runtime). They live in their own "submap" so the dense tile grid stays a plain
value array.

## Layers

| Type | Role |
|------|------|
| `InteractiveEntity` | Serializable enum of concrete kinds (`Charger(ChargerEntity)`). |
| `InteractiveEntityBehavior` | Trait shared by every kind; implemented on the enum so callers need no `match`. |
| `InteractiveEntityEntry` | The "special type": `(EntityType, EntityCoordinates, InteractiveEntity)`. The first two are redundant tags for cheap filtering. |
| `HypertileList<T>` | Generic ordered list of items sharing one hypertile. `InteractiveEntityHypertileList = HypertileList<InteractiveEntityEntry>`. |
| `InteractiveEntityMap` | The `Resource`: sparse `HashMap<EntityCoordinates, …list>`. One tile can hold **multiple** entities. |

`EntityCoordinates` is `(x, y, floor)` — the full hypermap address — and doubles
as the map key.

## Trait surface (`InteractiveEntityBehavior`)

- `entity_type() -> EntityType`
- `coordinates() -> EntityCoordinates`
- `props() -> HashMap<String, String>` — **empty** when no custom props set
- `is_used() / set_used(bool)` — the special "in use" flag
- `change_prop(key, value)` / `get_prop(key) -> Option<String>`

`ChargerEntity` adds typed fields: `facing` (`ChargerFacing`), `charge_level`
(clamped to `capacity`), and a runtime `occupant: Option<Entity>`. Docking an
occupant sets `is_used`.

## Duplication rule

An entity's `(type, coordinates)` is stored three times: in the entity, in its
`InteractiveEntityEntry`, and (coordinates) as the map key. **Entities never
move**, so this never drifts. The only discipline: add to every index on insert,
drop from every index on removal. Use `InteractiveEntityMap::insert` /
`remove_all_at` / `remove_of_type_at` — never hand-edit the inner map. Querying a
tile is `entities_at(coords) -> &[InteractiveEntityEntry]`.

## Locators

Three ways to ask "which entities are near here", on `InteractiveEntityMap`. Each
takes an optional `kind` filter (`None` = any) and returns borrowed entries. The
store is sparse, so all three iterate every entity and filter — there is no
per-tile spatial index to keep in sync.

| Method | "Near" means | Floor |
|--------|--------------|-------|
| `find_within_radius(center, radius, kind)` | Euclidean distance ≤ `radius` tiles (compared squared; `radius = 0` → just that tile). | same floor as `center` |
| `find_in_rendered_chunks(center, kind)` | On the chunks the renderer would keep meshed around `center` — reuses `hypermap_world::rendered_chunks_around` (camera chunk + the prefetch neighbor on each axis), so this query and the visible footprint never diverge. | all floors (chunk selection is XY-only) |
| `find_accessible_within(passability, start, floor, max_steps, kind)` | Reachable from `start` in ≤ `max_steps` 4-neighbor moves over the static-passability hypermap (bounded BFS). An entity matches if its tile **or any 4-neighbor** is reachable — chargers back onto a wall, so the actor stands adjacent. | `floor` |

`find_accessible_within` takes the single-floor passability map for the level
being searched; it distance-bounds a BFS rather than reusing
`explore_walkable_tiles_limited` (which bounds by expansion count, not step count).

**Locking.** `find_within_radius` and `find_in_rendered_chunks` touch only the
in-memory sparse `HashMap` — no hypermap locks. The accessible BFS reads the
chunked passability map through a `ChunkReadCache`, which holds the current
chunk's `Arc` handle between cell reads: the map-wide `chunks` lock is taken only
when the scan crosses a chunk boundary, and each per-chunk read lock is held just
long enough to copy one cell. (A bare `Hypermap::get` per cell would lock both the
map and the chunk on every step.)

## Serialization

`InteractiveEntityMap` serializes as a **flat `Vec<InteractiveEntityEntry>`**
(each entry carries its own coordinates). This keeps JSON compact and sidesteps
`serde_json`'s "map keys must be strings" limit on the struct key. The runtime
`occupant` is `#[serde(skip)]` — Bevy `Entity` ids are not stable across sessions
— so it loads back as `None`.

## Not yet wired

The store exists and round-trips, but nothing populates it from the map yet:
generation/editor placement of `CellType::Charger` does not auto-register a
`ChargerEntity`, and there is no save/load to a level file. Those are the natural
next steps (see `docs/level-persistence.md` for where a `interactive_entities`
file would slot in).
