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
  - "docs/pathfind-service.md"
  - "src/map/pathfind_service.rs"
  - "docs/field-interactions.md"
  - "src/lib.rs"
---

# Actor engineer (Everly)

## Scope

Use this skill for:

- `Actor` trait design and default method behavior.
- The 3-step arbitrated movement pipeline: `propose_actor_moves` (parallel),
  `arbitrate_actor_moves` (sequential), and the squeeze/teleport tail.
- Movement/rotation buffers and per-frame error handling.
- `ActorShadow` — per-actor shadow arrays (`current`/`previous` subtile coords)
  swapped each accepted frame; allocated once at spawn.
- `OccupancyArbiter` — per-frame owner grid resource; deterministic conflict
  resolution, bounded backoff (depth ≤ 4), squeeze-pool teleport fallback.
- Footprint-based collision and occupancy updates via `DynamicPassabilityMap`.
- **Per-actor static traversal rules** via `Actor::blocked_flags()` — a bitmask
  of `SubtilePassability` flags the actor cannot enter (ground walkers block on
  `FLAG_BLOCKED | FLAG_VOID`; a flying actor blocks only `FLAG_BLOCKED`, so void
  tiles are traversable).
- The high-level **brain** layer (`src/actor/brain/`): `Behavior`s raise
  `Priorities`, the dominant one selects a `HighLevelAction`, which dictates the
  `LowLevelAction` (`Wait` / `FollowPath`) that fills `move_buffer`. BlackBot's
  wander/patrol + self-recharge run here. **Read `docs/actor-brain.md` first** for
  this layer.
  - **Each `Behavior` is its own module** under `brain/behavior/`
    (`random_walker.rs`, `patroller.rs`, `charge_self_keeper.rs`); the `Behavior`
    trait + re-exports live in `behavior/mod.rs`, and constants shared between
    behaviors go in `behavior/behavior_utils.rs`. Add a new behavior as a new
    module, not inline.
  - A BlackBot's **specialization** (`BotSpecialization` in `black_bot.rs`) is a
    named behavior set + ring color, rolled at spawn (`PATROL` 1/4, else
    `DO_NOTHING`) and persisted in `actors.yaml`. `PATROL` adds the `Patrol`
    component. Per-bot planning state that must outlive a `HighLevelAction`
    belongs on a component, read through `BrainContext`.
  - **Async routing:** high-level actions enqueue on `PathfindQueue` and poll
    `PathfindResults` through `BrainContext::pathfind` (`PathfindAccess`). While
    awaiting, the low-level action is `PendingPath::with_velocity(...)` so inertia
    is preserved. Never call `astar_*` inline from `HighLevelAction::update`.
    Read **`docs/pathfind-service.md`** before touching queue or await logic.

For generic Bevy API usage, still read `.claude/SKILLS/bevy-engineer/SKILL.md` first.

## Invariants

- `propose_actor_moves` clears `last_movement_error` every frame before thinking.
- Actor movement intent is written into `move_buffer`, not applied directly.
- `move_buffer` must have **both** `tile_delta` and `subtile_shift` set each frame; `apply_outcome` applies the float delta to `center`.
- `Actor::propose_move` writes `shadow.proposed_center` / `shadow.origin` (compact `(center, radius)` footprints) — **never** updates `center` directly.
- `arbitrate_actor_moves` (sequential) is the single writer of `center`, `last_accepted_center_subtile`, and the dynamic passability write buffer.
- Collision logic belongs in `DynamicPassabilityMap` and `first_static_block` helpers, not duplicated in actors.
- **Static traversal rules are per-actor** and live in `Actor::blocked_flags()`. Different actor classes can and must override this — flying actors (future) must return `FLAG_BLOCKED` only (not `FLAG_VOID`), so void tiles are passable for them.
- Footprints are **never** stored as cell lists. `ActorShadow` holds only centers (`proposed_center`, `origin`); the cells are derived from the `&'static` baked `CircleShadow` for `radius_subtiles` wherever they are needed.
- Occupancy writes go to passability **write** buffer; visibility for future checks is after `flush()`.
- `center` is **never** derived from integer subtile math; it is always advanced by `tile_delta` for smooth rendering.
- **Grid position comes from `last_accepted_center_subtile`, not from `center_subtile_i32()`.** The float center drifts between subtile boundaries; recomputing from float can round into a wall cell.
- The hot path (`propose_actor_moves → arbitrate_actor_moves`) must remain allocation-free at steady state (`OccupancyArbiter` reuses all scratch vecs).

## Coordinates and units

- `ActorState.center` is tile-space float (`Vec2`), advanced by `tile_delta` every frame — never quantized to the subtile grid.
- `SUBTILE_COUNT = 5`; `1 tile = 5 subtiles`.
- `ActorMoveBuffer` has **two displacement channels**:
  - `tile_delta: Vec2` — exact float displacement in tile-space, applied to `center` every frame for perfectly smooth rendering.
  - `subtile_shift: IVec2` — integer subtile steps for the passability collision grid; usually `(0,0)`, non-zero only when accumulated motion crosses a subtile boundary.
- Continuous-movement actors accumulate `direction * speed * dt` in a float accumulator; the integer part becomes `subtile_shift`, the fractional part carries forward.
- Radius is integer subtiles; occupied shape is a baked integer circle.
- The **world-subtile** coordinate (`IVec2`) used in passability probes is absolute; convert to a tile with `world_subtile.div_euclid(SUBTILE_COUNT as i32)`.
- **Main tile** (which world tile the actor is in): always [`actor_main_tile`](../../src/actor/mod.rs) = `round(center)` in tile space. Used by field interactions and `BlackBot` think. Never `floor(center)` for this — see `docs/actor.md` § Main tile.
- **Subtile grid** (collision): `floor(center * SUBTILE_COUNT)` via `center_subtile_i32` / `last_accepted_center_subtile` — separate from main tile.

## Per-actor static passability

`Actor::blocked_flags(&self) -> u64` returns the bitmask of `SubtilePassability`
flags the actor cannot enter.

- Default impl = ground walker: `FLAG_BLOCKED | FLAG_VOID`.
- Flying actor (future): return `FLAG_BLOCKED` only — void tiles passable, wall edge strips still block.
- Phasing actor (future): return `0` — always passable.

`first_static_block(static_cache, center, radius, blocked, previous)` (in
`src/map/passability.rs`) returns the first subtile in the proposed footprint that
contains any of `blocked`'s flags. `propose_move` calls this during Step 1;
`arbitrate_actor_moves` never re-checks static geometry (that was already done).

## Pipeline overview

```
flush_actor_occupancy        (clear dynamic write buffer)
  ↓
propose_actor_moves          (parallel par_iter_mut)
    think_low_level
    prepare_movement
    propose_move             ← writes shadow.proposed_center / shadow.origin
                             ← static-only check via first_static_block
                             ← off-screen: advance_unchecked, no shadow update
  ↓
arbitrate_actor_moves        (sequential)
    entity-sorted MoveRecord build
    arbitrate()              ← owner grid, backoff cascade (depth ≤ 4), squeeze pool
    apply_outcome()          ← update center, shadow swap, last_movement_error
    stamp dynamic write buffer
    teleport squeezed / re-entrant actors
  ↓
dirt_actor_interaction / bot_occupancy_heat / ...   (field interactions)
  ↓
flush passability read buffer
```

## Preferred workflow

0. Always read documentation before reading any code, for whatever system you need!
1. Read `docs/actor.md` and `src/actor/mod.rs`.
2. If movement/collision changes, read `src/map/passability.rs` and
   `src/actor/movement.rs` too.
3. Keep actor-side code thin; push shared occupancy rules into passability methods.
4. Preserve deterministic per-frame order (see pipeline above).
5. Add/update unit tests in touched modules. **Brain tests assert pathfind
   requests** (enqueued `PathKind`, `PendingPath`, injected `PathOutcome`), not
   real route geometry. Path quality tests belong in `pathfind_service` or
   `hypermap_pathfind`.
6. Run `cargo check` and targeted tests:
   - `cargo test -p everly -- actor`
   - `cargo test -p everly -- passability`
   - `cargo test -p everly -- pathfind_service`

## Common pitfalls

- Forgetting to set both `shadow.proposed_center` **and** `shadow.origin` in `propose_move` — `arbitrate_actor_moves` uses both.
- Reading the dynamic passability buffer in `propose_move` — Step 1 is static-only; dynamic reads belong in the brain (before propose) or the arbiter (sequential).
- Applying movement before `arbitrate_actor_moves` runs — actors must not write `center` in `propose_move`.
- Not resetting `move_buffer` on collision outcome — the arbiter calls `apply_outcome`, which clears the buffer on a backed-off frame; don't double-clear.
- Deriving `center` displacement from integer `subtile_shift` instead of float `tile_delta` — this quantizes the rendered position and makes movement look choppy.
- Forgetting to reset the float accumulator on collision — the actor will "teleport" when it resumes movement.
- **Letting a flying / phasing actor inherit the ground-walker `blocked_flags`.** Override `blocked_flags()` for any class with non-standard traversal.
- **Re-introducing explicit `Vec<IVec2>` footprints.** Footprints stay compact `(center, radius)` end-to-end (shadow → `MoveRecord` → owner grid → `commit_footprint`); expand through `baked_circle_shadow` only at the point of use.
- Treating `BlockedByStatic` and `BlockedByOccupancy` as interchangeable — they're separate variants so actor behavior (e.g. re-pathing vs waiting) can differ per cause.

## New actor checklist

Follow `docs/actor.md#new-actor-checklist` when adding a new actor class. Key points:

1. Implement the `Actor` trait; override `blocked_flags()` if non-ground-walker.
2. Override `propose_move()` if axis-decomposed slide or special footprint needed (default is axis-combined, no slide).
3. Add `shadow: ActorShadow::default()` to the `ActorState` literal at construction.
4. Wire a brain plugin (if the actor has a brain) `.before(propose_actor_moves)` and `.after(arbitrate_actor_moves)`.
5. No need to touch `OccupancyArbiter` — it's generic over all actors.

## Documentation updates

When actor behavior changes, update:

- `docs/actor.md` (primary reference).
- `docs/actor-brain.md` / `docs/pathfind-service.md` when brain routing or the
  async queue changes.
- Follow `docs/actor.md#new-actor-checklist` for onboarding new actor classes.
- `docs/README.md` if scope/discoverability changed.
