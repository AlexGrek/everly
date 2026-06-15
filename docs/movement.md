# Bot movement

How an actor's movement intent becomes a position change, end to end. This is
the deep reference for the **single-pass movement pipeline** in
`src/actor/movement.rs`; the surrounding actor runtime (trait, state, spawning)
is documented in `docs/actor.md`, and the planning layer that decides *where* a
bot wants to go is in `docs/actor-brain.md` / `docs/pathfind-service.md`.

## Design summary

All movement for one frame happens in a **single sequential system**,
`process_actor_moves`. Per frame, in four stages:

1. **Think + propose** — every on-screen actor computes one candidate step,
   validated against **static** geometry only, and records it compactly in its
   `ActorShadow`. Off-screen actors `advance_unchecked` (no collision);
   re-entrants are queued.
2. **Resolve** (deterministic, entity-sorted) — actor-vs-actor occupancy is
   resolved within the frame over a reused **owner grid**.
3. **Apply + commit** — outcomes are written back to the actors and every final
   footprint is stamped into the dynamic passability write buffer.
4. **Re-entry placement** — actors returning from off-screen travel are placed
   on a free cell.

This replaces the older "propose then arbitrate" two-system split. Because the
propose step was already sequential (see below), there is no benefit to keeping
collision detection in a separate pass — merging it lets the resolve stage
commit each actor's footprint as it goes, which removes the back-off cascade and
the squeeze/teleport jam-recovery logic entirely (see *Why no teleport*).

The old parallel design (each actor checking last frame's occupancy snapshot and
OR-stamping its footprint in parallel) let two actors claim the same free cell
and overlap for a frame, and its contended parallel writes were the biggest
per-frame hot spot (see `OPTIMIZATION.md`).

> **Why the pass is sequential, not `par_iter_mut`.** The per-bot work is a
> single static slide probe — single-digit microseconds for the whole crowd.
> Running it on Bevy's global `ComputeTaskPool` (via `par_iter_mut`) was a net
> loss: that pool's `scope` ticks the *global* executor while waiting for its
> own batches, so a propose tick absorbed unrelated queued compute work (render
> batching, etc.) and, at 60 Hz fixed catch-up ticks, compounded into
> frame-rate-coupled stalls. Sequential execution on the main thread removes the
> coupling for no measurable compute cost. A private thread pool would isolate
> the work too, but only matters if per-bot work grows heavy (hundreds of bots);
> at this scale it adds `unsafe` query chunking and core contention for nothing.

## Tick lifecycle — fixed 60 Hz

The whole pipeline runs on Bevy's **`FixedUpdate` schedule at 60 Hz**
(`Time::<Fixed>::from_hz(60.0)` in `GamePlugin`), decoupled from the render
frame rate: a slow render frame runs extra fixed ticks to catch up, so bot
pace never depends on fps. Inside `FixedUpdate`, `Res<Time>` yields the fixed
`dt` automatically.

```
FixedUpdate (×N per render frame, 60 Hz real-time)
  pathfind_collect             drain finished async route results
    ↓
  flush_actor_occupancy        promote occupancy write buffer → read; reset write
    ↓
  black_bot_brain              planning: fills move_buffer (sequential, owns RNG)
    ↓
  pathfind_dispatch            spawn queued searches (≤10 in flight)
    ↓
  process_actor_moves          think + propose + resolve + apply + re-entry
    ↓
  dirt_actor_interaction, …    field deposits read final positions

Update (once per render frame, after all fixed ticks)
  sync_black_bot_transforms    actor state → render transforms
  flush_dirt_map / …           field double-buffer flushes, overlays, HUD
```

`FixedUpdate` always completes before `Update` within a frame, so
render-facing systems read the state the fixed ticks just produced without any
cross-schedule ordering. Input edge detection (`toggle_pause`) stays in
`Update` — `just_pressed` edges can be missed by fixed ticks at high frame
rates. All systems are gated on `GameState::InGame` and not-paused.

## Movement intent: `move_buffer`

A brain (or any controller) never moves an actor directly. It writes an
`ActorMoveBuffer` on `ActorState`:

- `tile_delta: Vec2` — the float displacement for smooth rendering;
- `subtile_shift: IVec2` — the integer grid step the float motion implies;
- `rotation_shift: f32`.

The float `center` and the integer grid position are deliberately decoupled:
`center` drifts continuously for rendering, while collision and occupancy work
on `last_accepted_center_subtile` (1 tile = 5 subtiles). Movement always
computes the candidate grid cell as `last_accepted_center_subtile +
subtile_shift` — never by re-quantizing the float center, which can round into
a wall.

## Stage 1 — think + propose

`process_actor_moves` iterates all actors on the main thread. Per actor:

1. Clear `last_movement_error` and the per-frame shadow flags.
2. `think_low_level()` + `prepare_movement()` — light per-frame logic that
   fills `move_buffer` (heavy planning happened earlier in the brain system).
3. Branch on visibility:
   - **On-screen** → `Actor::propose_move(static_cache)`, then queued as a
     resolution participant.
   - **Off-screen** → `advance_unchecked()` (move freely, no collision, no
     occupancy footprint) and tag with `OffScreenActor`.
   - **Re-entering** (was off-screen, now on a rendered chunk) → queued for
     placement in stage 4; no proposal this frame.

This stage touches only each actor's own state plus the read-only static cache,
so it is order-independent (the order-sensitive work is stage 2).

`propose_move` validates the candidate step against the **static subtile
cache only** (`first_static_block`) — walls and void, filtered through the
actor's `blocked_flags()` (ground walkers block on `FLAG_BLOCKED | FLAG_VOID`,
flyers on `FLAG_BLOCKED` only). It never touches the dynamic occupancy map.

The default `propose_move` tests the combined `(dx, dy)` footprint and cancels
the whole step if blocked. `BlackBot` overrides it with an axis-decomposed
probe (X-only, then Y-only) so bots **slide along walls** instead of stopping:
a blocked axis is zeroed and the float delta for that axis snaps flush to the
wall; the first blocked axis is reported as `BlockedByStatic`.

The result is recorded compactly in the actor's `ActorShadow`:

- `proposed_center: IVec2` — candidate footprint center (post-slide);
- `origin: IVec2` — the last accepted center, i.e. the hold-in-place target;
- `proposed_delta / proposed_rotation` — float motion to apply on success;
- `static_block` — first statically blocked cell, if the slide clipped one;
- `participates = true`.

Footprints are **never** stored as cell lists. A footprint is always the baked
circle of `radius_subtiles` around a center (`baked_circle_shadow`, `&'static`
offsets), so `(center, radius)` is the entire representation — see
`OPTIMIZATION.md` rule 4.

## Stage 2 — resolve (sequential, deterministic)

The participating actors are sorted by `Entity` (so the outcome is independent
of any iteration order) and snapshotted into plain-`Copy`
`MoveRecord { current, previous, radius, … }`. All scratch (`records`,
`entities`, owner grid, re-entry list) lives in the reused `OccupancyArbiter`
resource — steady state allocates nothing.

Conflicts are resolved over the **owner grid**: a flat foldhash
`HashMap<IVec2, u32>` mapping each claimed world-subtile to the dense record
index that owns it (sequential pass → no locks needed). The pure resolver
(`arbitrate`) works in two sweeps:

1. **Pre-stamp** every actor's `previous` (currently-occupied) footprint into
   the grid. The `previous` footprints are last frame's accepted positions, so
   they are pairwise disjoint.
2. **Resolve** each record in entity order:
   - Release the actor's own `previous` footprint (so a small,
     footprint-overlapping step never blocks on itself).
   - **No foreign cell in its proposed footprint** → stamp `current`; the actor
     advances.
   - **Conflict** → stamp `previous` back; the actor is marked `collided` with
     the conflicting cell and **holds in place**.

### Why no teleport

Because every actor's currently-occupied footprint is pre-stamped:

- A mover can never claim a cell another actor still occupies — the **occupant
  always keeps its spot**, regardless of entity order. (A mover that wants an
  occupant's cell holds for that frame; if the occupant later vacates, the mover
  advances next frame.)
- Every actor always has its **own origin** to fall back to (it owns it, and
  only releases it momentarily before re-claiming on a conflict), so there is
  never a wedged actor with nowhere to go.

That removes the entire jam-recovery machinery the old two-system design
needed — no recursive back-off cascade, no depth cap, and **no squeeze/teleport
pool**. The only remaining teleport is off-screen re-entry (stage 4), which is
unrelated to jams.

**Train ripple.** A line of bots all stepping the same direction flows
smoothly, but a follower processed *before* its leader (lower `Entity`) sees the
leader's not-yet-vacated cell as occupied and holds for a single frame; it
advances the next frame once the leader has moved. This one-frame ripple is
imperceptible at 60 Hz and is the price of the simpler, overlap-free model — it
never produces an overlap.

Invariants: at most one owner per subtile at every step; the resolution is a
pure function over `records` (unit-tested directly in `movement.rs`).

## Stage 3 — apply + commit

In entity order, each outcome is written back to the actor:

- **Advanced**: `center += proposed_delta`,
  `last_accepted_center_subtile = proposed_center`, rotation applied. If the
  slide clipped a wall, `last_movement_error = BlockedByStatic`.
- **Collided**: position holds; `last_movement_error =
  BlockedByOccupancy { conflict_cell }`. Reaction is owned by the existing brain
  machinery next frame (re-route, collision pressure, status flash) — the
  pipeline itself never invents avoidance. The brain's `FollowPath` follows a
  **single unified path of cell + subcell nodes** (`PathNode`); a bump first
  splices a local subtile detour inline, and a stall first tries a local
  splice-repair, before escalating. A genuine head-on wedge that cannot be
  resolved locally escalates, in the BlackBot brain, to a relocate **and a full
  path recalculation against the dynamic passability map** (so the new route
  avoids the tiles other bots currently occupy); this fires both on
  collision-pressure saturation and on a sustained no-progress stall that loops
  inside recovery maneuvers — see `docs/actor-brain.md`.

Every final footprint (the `current` cell on advance, the `previous` cell on a
hold) is stamped into the dynamic passability **write** buffer via
`commit_footprint` (`FLAG_BLOCKED | FLAG_CREATURE`), so after the next `flush`
the brain's avoidance views and the async pathfinder see exactly the occupancy
the movement pass decided.

## Stage 4 — re-entry placement

Off-screen re-entrants (was off-screen, now on a rendered chunk) are placed
sequentially (sorted by entity) by `resolve_offscreen_collision` — an expanding
ring search for the nearest statically-and-dynamically free cell. The write
buffer already holds this frame's footprints, so a re-entrant never lands on a
placed actor. This teleport is the only non-local move in the system; it exists
solely because off-screen actors travel **without collision detection against
dynamic objects** and may re-enter sitting inside static geometry. Each placed
re-entrant sets `shadow.teleported`, which the BlackBot brain uses to drop its
stale plan and re-route from the new position.

## Occupancy storage

The dynamic occupancy map (`DynamicPassabilityMap`) is a double-buffered,
single-floor subtile hypermap. Each frame `flush_actor_occupancy` promotes the
write buffer to the read side; the write side starts clean and receives only
this frame's accepted footprints. Reads during planning therefore see a
consistent snapshot of *last* frame's occupancy, while `process_actor_moves` is
the only within-frame authority. Flushed chunks are recycled through a pool with
dirty-cell spot-resetting, so the per-frame buffer cycle allocates nothing at
steady state (see `OPTIMIZATION.md`, "Dynamic passability — single-floor
chunks + recycled flush").

## Determinism

- Stage 1 each actor touches only its own state.
- Stage 2/3 process in sorted-entity order, so results are reproducible.
- Bot RNG lives in the sequential brain system and is seeded (`StdRng`).
- The only non-determinism in the wider movement stack is the **arrival
  frame** of async pathfind results (`docs/pathfind-service.md`).

## Where things live

| Concern | File |
|---|---|
| Movement system, owner grid, resolution core | `src/actor/movement.rs` |
| `Actor` trait, `ActorState`, default `propose_move`, off-screen advance | `src/actor/mod.rs` |
| BlackBot slide override, brain integration | `src/actor/black_bot.rs` |
| Static probe, dynamic buffer, footprint stamping, baked circles | `src/map/passability.rs` |
| Chunked store, double buffer, chunk recycling | `src/map/hypermap.rs` |

When editing any of this, read `.claude/SKILLS/actor-engineer/SKILL.md` first
(per `CLAUDE.md`), and treat any perf-sensitive change as an
`OPTIMIZATION.md` read-and-update task.
