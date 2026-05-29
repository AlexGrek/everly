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
