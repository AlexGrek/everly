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
  - "src/map/field_interactions.rs"
  - "docs/actor.md"
  - "docs/actor-brain.md"
  - "docs/field-interactions.md"
  - "src/lib.rs"
---

# Actor engineer (Everly)

## Scope

Use this skill for:

- `Actor` trait design and default method behavior.
- low-level actor processing systems (`think_low_level`, `prepare_movement`, `try_move`).
- movement/rotation buffers and per-frame error handling.
- footprint-based collision and occupancy updates via `DynamicPassabilityMap`.
- **per-actor static traversal rules** via `Actor::is_static_subtile_passable` (flying, swimming, ground-walking, phasing, …).
- the high-level **brain** layer (`src/actor/brain/`): `Behavior`s raise
  `Priorities`, the dominant one selects a `HighLevelAction`, which dictates the
  `LowLevelAction` (`Wait` / `FollowPath`) that fills `move_buffer`. BlackBot's
  wander + self-recharge run here. **Read `docs/actor-brain.md` first** for this layer.

For generic Bevy API usage, still read `.claude/SKILLS/bevy-engineer/SKILL.md` first.

## Invariants

- Low-level actor step clears `last_movement_error` every frame before thinking.
- Actor movement intent is written into `move_buffer`, not applied directly.
- `move_buffer` must have **both** `tile_delta` and `subtile_shift` set each frame; `move_actor` applies the float delta, `try_move` uses the integer shift for collision.
- `try_move(&dynamic, &static_world)` is the gate: dynamic collision + static collision + occupancy update + transform update.
- Collision logic belongs in `DynamicPassabilityMap::try_update_footprint_with_static`, not duplicated in actors.
- **Static traversal rules are per-actor** and live in `Actor::is_static_subtile_passable`. Different actor classes can and must override this — flying actors are not ground walkers.
- Previous occupancy is stored compactly as `(last_accepted_center_subtile, last_accepted_radius_subtiles)` and reconstructed via the baked `CircleShadow` — **never** materialize a `Vec<IVec2>` per frame for self-overlap testing.
- Self-overlap bypasses both static and dynamic checks; tested in `O(1)` via `CircleShadow::contains_offset(target - previous_center)`.
- Occupancy writes go to passability **write** buffer; visibility for future checks is after `flush()`.
- `center` is **never** derived from integer subtile math; it is always advanced by `tile_delta` for smooth rendering.
- **Grid position comes from `last_accepted_center_subtile`, not from `center_subtile_i32()`.** The float center drifts between subtile boundaries via `tile_delta`; recomputing the grid position from the float can round into a wall cell and permanently deadlock the actor. Only the initial frame (when `last_accepted_center_subtile` is `None`) falls back to float-derived rounding.
- The hot path (`process_actors → try_move → try_update_footprint_with_static`) must remain allocation-free at steady state.

## Coordinates and units

- `ActorState.center` is tile-space float (`Vec2`), advanced by `tile_delta` every frame — never quantized to the subtile grid.
- `SUBTILE_COUNT = 5`; `1 tile = 5 subtiles`.
- `ActorMoveBuffer` has **two displacement channels**:
  - `tile_delta: Vec2` — exact float displacement in tile-space, applied to `center` every frame for perfectly smooth rendering.
  - `subtile_shift: IVec2` — integer subtile steps for the passability collision grid; usually `(0,0)`, non-zero only when accumulated motion crosses a subtile boundary.
- Continuous-movement actors accumulate `direction * speed * dt` in a float accumulator; the integer part becomes `subtile_shift`, the fractional part carries forward.
- Radius is integer subtiles; occupied shape is a baked integer circle.
- The **world-subtile** coordinate (`IVec2`) passed into `is_static_subtile_passable` is absolute, not local; convert to a tile with `world_subtile.div_euclid(SUBTILE_COUNT as i32)`.
- **Main tile** (which world tile the actor is in): always [`actor_main_tile`](../../src/actor/mod.rs) = `round(center)` in tile space. Used by field interactions and `BlackBot` think. Never `floor(center)` for this — see `docs/actor.md` § Main tile.
- **Subtile grid** (collision): `floor(center * SUBTILE_COUNT)` via `center_subtile_i32` / `last_accepted_center_subtile` — separate from main tile.

## Per-actor static passability

`Actor::is_static_subtile_passable(&self, world_subtile: IVec2, world: &StaticWorld) -> bool` is the per-class traversal rule.

`StaticWorld` bundles **two** read-only views:

- `passability: &Hypermap<f32>` — scalar, `> 0.0` = walkable (ground-walker view).
- `cell_types: &Hypermap<CellType>` — raw geometry, needed to distinguish `Void` from `Wall(_)` because the scalar map collapses both to `0.0`.

Use `StaticWorld::cell_at_subtile(sub)` / `passability_at_subtile(sub)` / `subtile_to_tile(sub)` helpers — never reach into the raw maps and recompute the subtile-to-tile division yourself.

- Default impl = ground walker (`world.passability_at_subtile(sub) > 0.0`).
- Override for flying (`GlitchBot`): **subtile-aware** wall policy — cross void tiles, but block wall edge strips and corner-pillar subtiles.
- Override for swimming: ground passable OR cell is water.
- Override for phasing: always `true`.
- Self-overlap (subtile is in `previous_footprint`) bypasses this check inside `try_update_footprint_with_static`.

The callback is built fresh inside `try_move` from `self`, so it always reflects the current actor's class — never assume one global rule.

## Preferred workflow

0. Always read documentation before reading any code, for whatever system you need!
1. Read `docs/actor.md` and `src/actor/mod.rs`.
2. If movement/collision changes, read `src/map/passability.rs` too.
3. Keep actor-side code thin; push shared occupancy rules into passability methods.
4. Preserve deterministic per-frame order:
   - clear error
   - think
   - prepare movement
   - try move
   - flush passability
   - field interactions (e.g. dirt) — see `.claude/SKILLS/field-interactions/SKILL.md`
5. Add/update unit tests in touched modules.
6. Run `cargo check` and targeted tests:
   - `cargo test -p everly -- actor`
   - `cargo test -p everly -- passability`

## Common pitfalls

- Forgetting to persist old footprint causes false self-collision.
- Reading from write buffer semantics by accident; collision should use read side.
- Applying movement before accepted footprint update can desync actor state vs occupancy.
- Not resetting `move_buffer` on failed movement.
- Deriving `center` displacement from integer `subtile_shift` instead of float `tile_delta` — this quantizes the rendered position and makes movement look choppy.
- Forgetting to reset the float accumulator on collision — the actor will "teleport" when it resumes movement.
- **Letting a flying / phasing actor inherit the ground-walker static check.** If a new actor class can traverse otherwise-blocked geometry, you MUST override `is_static_subtile_passable` — the default is *always* ground-walker.
- **Confusing "static passability `0.0`" with "wall".** `Void` and `Wall(_)` both have passability `0.0`. A flier that returns `true` for everything will literally fly through walls — branch on `StaticWorld::cell_at_subtile(...)` to tell them apart.
- **Re-introducing per-frame `Vec<IVec2>` clones for the previous footprint.** Don't. The compact `(last_accepted_center_subtile, last_accepted_radius_subtiles)` representation is the source of truth — reconstruct the circle via `baked_circle_shadow(radius)` if you really need the cell list.
- **Forgetting to set `last_accepted_radius_subtiles` when initializing `ActorState`.** It must match `radius_subtiles` at construction; mismatching values would re-stamp the wrong-sized circle on the first failed move (though, since `last_accepted_center_subtile` starts as `None`, no re-stamp happens until after the first successful frame).
- Confusing `world_subtile` (absolute, `SUBTILE_COUNT = 5` per tile) with a local-to-tile subtile index in static-passability callbacks.
- Treating `BlockedByStatic` and `BlockedByOccupancy` as interchangeable — they're separate variants so actor behavior (e.g. re-pathing vs waiting) can differ per cause.

## Documentation updates

When actor behavior changes, update:

- `docs/actor.md` (primary reference).
- Follow `docs/actor.md#new-actor-checklist` for onboarding new actor classes.
- `docs/README.md` if scope/discoverability changed.
