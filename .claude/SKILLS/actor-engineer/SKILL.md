---
name: actor-engineer
description: >-
  Implements and refactors Everly actor runtime code: `Actor` trait flows,
  per-frame low-level systems, movement buffers, subtile footprints, and
  collision integration with `DynamicPassabilityMap`. Use when editing
  `src/actor/`, actor docs, or passability APIs consumed by actor movement.
paths:
  - "src/actor/**/*.rs"
  - "src/map/passability.rs"
  - "docs/actor.md"
  - "src/lib.rs"
---

# Actor engineer (Everly)

## Scope

Use this skill for:

- `Actor` trait design and default method behavior.
- low-level actor processing systems (`think_low_level`, `prepare_movement`, `try_move`).
- movement/rotation buffers and per-frame error handling.
- footprint-based collision and occupancy updates via `DynamicPassabilityMap`.

For generic Bevy API usage, still read `.claude/SKILLS/bevy-engineer/SKILL.md` first.

## Invariants

- Low-level actor step clears `last_movement_error` every frame before thinking.
- Actor movement intent is written into `move_buffer`, not applied directly.
- `try_move` is the gate: collision check + occupancy update + transform update.
- Collision logic belongs in `DynamicPassabilityMap` (`try_update_footprint`), not duplicated in actors.
- Actor `footprint` is persisted and passed back to passability to ignore self-collision overlap.
- Occupancy writes go to passability **write** buffer; visibility for future checks is after `flush()`.

## Coordinates and units

- `ActorState.center` is tile-space float (`Vec2`).
- Movement and occupancy checks use integer subtiles (`IVec2`).
- `SUBTILE_COUNT = 5`; convert tile delta from subtile shift as `shift / 5.0`.
- Radius is integer subtiles; occupied shape is a baked integer circle.

## Preferred workflow

1. Read `docs/actor.md` and `src/actor/mod.rs`.
2. If movement/collision changes, read `src/map/passability.rs` too.
3. Keep actor-side code thin; push shared occupancy rules into passability methods.
4. Preserve deterministic per-frame order:
   - clear error
   - think
   - prepare movement
   - try move
   - flush passability
5. Add/update unit tests in touched modules.
6. Run `cargo check` and targeted tests:
   - `cargo test -p everly -- actor`
   - `cargo test -p everly -- passability`

## Common pitfalls

- Forgetting to persist old footprint causes false self-collision.
- Reading from write buffer semantics by accident; collision should use read side.
- Applying movement before accepted footprint update can desync actor state vs occupancy.
- Not resetting `move_buffer` on failed movement.

## Documentation updates

When actor behavior changes, update:

- `docs/actor.md` (primary reference).
- Follow `docs/actor.md#new-actor-checklist` for onboarding new actor classes.
- `docs/README.md` if scope/discoverability changed.
