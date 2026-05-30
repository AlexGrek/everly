---
name: field-interactions
description: >-
  Actor ↔ hypermap field coupling: main-tile tracking after movement, dirt
  deposits, double-buffer flush gating, and overlay dirty chunks. Use when
  editing `src/map/field_interactions.rs`, `src/map/dirt.rs`, dirt overlay,
  or adding new fields (temperature, etc.).
paths:
  - "src/map/field_interactions.rs"
  - "src/map/tile_field.rs"
  - "src/map/dirt.rs"
  - "src/map/dirt_overlay.rs"
  - "src/map/temperature.rs"
  - "src/map/temperature_overlay.rs"
  - "docs/field-interactions.md"
  - "docs/level-persistence.md"
  - "docs/actor.md"
  - "src/actor/mod.rs"
  - "src/actor/black_bot.rs"
---

# Field interactions (Everly)

## Invariants

- Field systems run **after** [`process_actors`](../../src/actor/mod.rs) and
  **before** that field's [`flush_dirt_map`](../../src/map/dirt.rs) (or equivalent).
- Main tile = `(round(center.x), round(center.y))` via [`actor_main_tile`](../../src/actor/mod.rs)
  — shared with [`BlackBot`](../../src/actor/black_bot.rs) (`BlackBotVisual.main_tile`).
  Not `floor` (that is for subtiles only). Full table: `docs/actor.md` § Main tile.
- Track prior tile in [`ActorState::field_main_tile`](../../src/actor/mod.rs);
  update it inside field code only. Not serialized in actor snapshots.
- **Left tile** on transition gets the field effect, not the destination.
- Writers use the field hypermap **write** buffer; readers (overlay, gameplay)
  use **read** after flush.

## Dirt (current)

- Per-actor [`ActorState::dirtiness`](../../src/actor/mod.rs) (`0.0..=1.0`); actors
  spawn clean and it is **not** serialized in snapshots.
- [`dirt_actor_interaction`](../../src/map/field_interactions.rs): iterate actors;
  per actor, if it crossed main tiles, exchange dirt with the **left** tile. Actors
  that didn't change tile do no field math, and only the cleaner-floor branch writes
  the dirt buffer.
- [`exchange_dirt_with_tile`](../../src/map/field_interactions.rs): skip `CellType::Void`;
  apply pure [`dirt_exchange`](../../src/map/field_interactions.rs) (rate
  [`DIRT_TRACK_DEPOSIT`](../../src/map/dirt.rs) = `0.01`): floor cleaner → actor wipes
  1% onto the tile via [`DirtMap::add_tile_dirt`](../../src/map/dirt.rs) (conserved,
  capped at `0.0`); floor dirtier → actor picks up 1% *of the floor*, tile unchanged.
  Writes the **tile** scalar ([`TileFieldMap`](../../src/map/tile_field.rs), not subtile grid).
- [`flush_dirt_map`](../../src/map/dirt.rs): no-op when
  `write_map().loaded_chunk_count() == 0` (no buffer swap).
- Overlay: only repaints `take_dirty_chunks()`.

## Preferred workflow

1. Read `docs/field-interactions.md`.
2. For save/load of `dirt.bin` / `temperature.bin`, read `docs/level-persistence.md`.
3. For movement/tracking changes, read `docs/actor.md` and actor-engineer skill.
4. Add field-specific logic beside shared helpers in `field_interactions.rs`.
5. Keep hot path allocation-free after the transition `Vec` (reuse later if needed).
6. `cargo check` and `cargo test field_interactions`.

## Adding another tile field (e.g. temperature actor coupling)

1. Wrap [`TileFieldMap`](../../src/map/tile_field.rs) like [`TemperatureMap`](../../src/map/temperature.rs).
2. `fn apply_<field>_on_tile(left_tile: IVec2, …)` in `field_interactions.rs`.
3. System after `process_actors` calling `collect_main_tile_transitions` once, then
   each field handler (or one system dispatching all fields).
4. Conditional `flush_if_pending` + tile overlay (`TILE_FIELD_OVERLAY_RES` = 128).
5. Extend `docs/field-interactions.md` and this skill.

[`TemperatureMap`](../../src/map/temperature.rs) already exists (seed + overlay only).

## Pitfalls

- Running field interaction **before** movement — `center` is stale.
- Depositing on the **destination** tile instead of `MainTileTransition::left_tile`.
- Always calling `flush` — forces merge and overlay work when write buffer is empty.
- Forgetting `mark_dirty` / chunk coord when writing dirt samples.
- Using `DoubleBufferedHypermap::flush` (replace) instead of `flush_merge` for
  persistent fields — drops unmodified read chunks.
