---
name: field-interactions
description: >-
  Actor ↔ hypermap field coupling: main-tile tracking after movement, dirt
  deposits, double-buffer flush gating, and overlay dirty chunks. Use when
  editing `src/map/field_interactions.rs`, `src/map/dirt.rs`, dirt overlay,
  or adding new fields (temperature, etc.).
paths:
  - "src/map/field_interactions.rs"
  - "src/map/dirt.rs"
  - "src/map/dirt_overlay.rs"
  - "docs/field-interactions.md"
  - "src/actor/mod.rs"
---

# Field interactions (Everly)

## Invariants

- Field systems run **after** [`process_actors`](../../src/actor/mod.rs) and
  **before** that field's [`flush_dirt_map`](../../src/map/dirt.rs) (or equivalent).
- Main tile = `(round(center.x), round(center.y))` in tile-space via
  [`actor_main_tile`](../../src/actor/mod.rs) — not `floor` (that is for subtiles).
- Track prior tile in [`ActorState::field_main_tile`](../../src/actor/mod.rs);
  update it inside field code only. Not serialized in actor snapshots.
- **Left tile** on transition gets the field effect, not the destination.
- Writers use the field hypermap **write** buffer; readers (overlay, gameplay)
  use **read** after flush.

## Dirt (current)

- [`dirt_actor_interaction`](../../src/map/field_interactions.rs): if zero
  [`MainTileTransition`]s, return immediately (no deposits, no dirty chunks from actors).
- [`deposit_dirt_on_tile`](../../src/map/field_interactions.rs): skip `CellType::Void`;
  call [`DirtMap::add_tile_dirt`](../../src/map/dirt.rs) with
  [`DIRT_TRACK_DEPOSIT`](../../src/map/dirt.rs) (`0.01`).
- [`flush_dirt_map`](../../src/map/dirt.rs): no-op when
  `write_map().loaded_chunk_count() == 0` (no buffer swap).
- Overlay: only repaints `take_dirty_chunks()`.

## Preferred workflow

1. Read `docs/field-interactions.md`.
2. For movement/tracking changes, read `docs/actor.md` and actor-engineer skill.
3. Add field-specific logic beside shared helpers in `field_interactions.rs`.
4. Keep hot path allocation-free after the transition `Vec` (reuse later if needed).
5. `cargo check` and `cargo test field_interactions`.

## Adding another field (e.g. temperature)

1. New `Hypermap` / `DoubleBufferedHypermap` resource + plugin.
2. `fn apply_<field>_on_tile(left_tile: IVec2, …)` in `field_interactions.rs`.
3. System after `process_actors` calling `collect_main_tile_transitions` once, then
   each field handler (or one system dispatching all fields).
4. Conditional flush + dirty overlay pattern matching dirt.
5. Extend `docs/field-interactions.md` and this skill.

## Pitfalls

- Running field interaction **before** movement — `center` is stale.
- Depositing on the **destination** tile instead of `MainTileTransition::left_tile`.
- Always calling `flush` — forces merge and overlay work when write buffer is empty.
- Forgetting `mark_dirty` / chunk coord when writing dirt samples.
- Using `DoubleBufferedHypermap::flush` (replace) instead of `flush_merge` for
  persistent fields — drops unmodified read chunks.
