# Bot movement

How an actor's movement intent becomes a position change, end to end. This is
the deep reference for the **arbitrated movement pipeline** in
`src/actor/movement.rs`; the surrounding actor runtime (trait, state, spawning)
is documented in `docs/actor.md`, and the planning layer that decides *where* a
bot wants to go is in `docs/actor-brain.md` / `docs/pathfind-service.md`.

## Design summary

Movement is split into three phases per frame:

1. **Propose** (parallel) — every on-screen actor computes one candidate step,
   validated against **static** geometry only.
2. **Arbitrate** (sequential, deterministic) — a single authority resolves all
   actor-vs-actor occupancy conflicts within the frame.
3. **Apply + squeeze** (inside the arbitrate system) — outcomes are written
   back to the actors; hopelessly wedged bots are teleported out.

The split exists because the two halves want opposite execution models. Static
collision is read-only and embarrassingly parallel, so it runs on
`par_iter_mut` with zero shared writes. Occupancy is a shared resource that
must be allocated authoritatively — two bots must never hold the same subtile
in the same frame — so it is resolved by one thread in a deterministic order.
The previous design (each actor checking last frame's occupancy snapshot and
OR-stamping its footprint in parallel) allowed two actors to claim the same
free cell and overlap for a frame, and its contended parallel writes were the
biggest per-frame hot spot (see `OPTIMIZATION.md`).

## Frame lifecycle

```
flush_actor_occupancy        promote occupancy write buffer → read; reset write
  ↓
black_bot_brain              planning: fills move_buffer (sequential, owns RNG)
  ↓
propose_actor_moves          PHASE 1 (parallel)
  ↓
arbitrate_actor_moves        PHASES 2 + 3 (sequential)
  ↓
dirt_actor_interaction, …    field interactions read final positions
```

All systems are gated on `GameState::InGame` and not-paused.

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

## Phase 1 — propose (parallel)

`propose_actor_moves` runs `par_iter_mut` over all actors. Per actor:

1. Clear `last_movement_error` and the per-frame shadow flags.
2. `think_low_level()` + `prepare_movement()` — light per-frame logic that
   fills `move_buffer` (heavy planning happened earlier in the brain system).
3. Branch on visibility:
   - **On-screen** → `Actor::propose_move(static_cache)`.
   - **Off-screen** → `advance_unchecked()` (move freely, no collision, no
     occupancy footprint) and tag with `OffScreenActor`.
   - **Re-entering** (was off-screen, now on a rendered chunk) → queued for
     sequential placement in phase 3; no proposal this frame.

`propose_move` validates the candidate step against the **static subtile
cache only** (`first_static_block`) — walls and void, filtered through the
actor's `blocked_flags()` (ground walkers block on `FLAG_BLOCKED | FLAG_VOID`,
flyers on `FLAG_BLOCKED` only). It never touches the dynamic occupancy map, so
the whole phase takes no contended locks: static chunks are reached through
the lock-free `ArcSwap` snapshot and read under uncontended per-chunk locks.

The default `propose_move` tests the combined `(dx, dy)` footprint and cancels
the whole step if blocked. `BlackBot` overrides it with an axis-decomposed
probe (X-only, then Y-only) so bots **slide along walls** instead of stopping:
a blocked axis is zeroed and the float delta for that axis snaps flush to the
wall; the first blocked axis is reported as `BlockedByStatic`.

The result is recorded compactly in the actor's `ActorShadow`:

- `proposed_center: IVec2` — candidate footprint center (post-slide);
- `origin: IVec2` — the last accepted center, i.e. the back-off target;
- `proposed_delta / proposed_rotation` — float motion to apply on success;
- `static_block` — first statically blocked cell, if the slide clipped one;
- `participates = true`.

Footprints are **never** stored as cell lists. A footprint is always the baked
circle of `radius_subtiles` around a center (`baked_circle_shadow`, `&'static`
offsets), so `(center, radius)` is the entire representation — see
`OPTIMIZATION.md` rule 4.

## Phase 2 — arbitrate (sequential, deterministic)

`arbitrate_actor_moves` collects every participating actor, sorts by `Entity`
(so the outcome is independent of phase-1 thread scheduling), and snapshots
each one into a plain-`Copy` `MoveRecord { current, previous, radius, … }`.
All scratch (`records`, `entities`, squeeze pool, owner grid) lives in the
reused `OccupancyArbiter` resource — steady state allocates nothing.

Conflicts are resolved over the **owner grid**: a flat foldhash
`HashMap<IVec2, u32>` mapping each claimed world-subtile to the dense record
index that owns it (sequential pass → no locks needed). For each record in
order:

- **No foreign cell in its proposed footprint** → stamp it; the actor advances.
- **Conflict** → the actor is marked `collided` and backed off to its
  `previous` footprint via `back_off`:
  1. Unstamp anything the actor already placed.
  2. Try to stamp its `previous` footprint.
  3. If some other actor `j` occupies one of those cells, mark `j` collided
     and recursively back `j` off to *its* previous footprint, then rescan.
  4. Recursion is capped at `MAX_BACKOFF_DEPTH` (4). An actor touched at the
     cap — or one that keeps re-landing on the same contested cell (cycle
     guard) — is **unplaced and pushed to the squeeze pool** instead.

Invariants: at most one owner per subtile at every step; a backed-off actor's
old cells are always cleared before re-placement (no ghost ownership); the
whole resolution is a pure function over `records` (unit-tested directly in
`movement.rs`).

## Phase 3 — apply + squeeze

Still inside `arbitrate_actor_moves`, in entity order:

- **Advanced** (placed at `current`): `center += proposed_delta`,
  `last_accepted_center_subtile = proposed_center`, rotation applied. If the
  slide clipped a wall, `last_movement_error = BlockedByStatic`.
- **Collided** (placed at `previous`): position holds; `last_movement_error =
  BlockedByOccupancy { conflict_cell }`. Reaction is owned by the existing
  brain machinery next frame (re-route, collision pressure, status flash) —
  the pipeline itself never invents avoidance.
- **Squeezed** (not placed): handled below.

Every placed footprint is stamped into the dynamic passability **write**
buffer via `commit_footprint` (`FLAG_BLOCKED | FLAG_CREATURE`), so after the
next `flush` the brain's avoidance views and the async pathfinder see exactly
the occupancy the arbiter decided.

**Squeeze + re-entry:** squeeze-pool actors and off-screen re-entrants are
placed sequentially (sorted by entity) by `resolve_offscreen_collision` — an
expanding ring search for the nearest statically-and-dynamically free cell.
This teleport is the only non-local move in the system and is the documented
last resort for unresolvable jams; each squeeze emits a `BotSqueezedOut` game
log entry and sets `shadow.teleported`, which the BlackBot brain uses to
re-plan from the new position.

## Occupancy storage

The dynamic occupancy map (`DynamicPassabilityMap`) is a double-buffered,
single-floor subtile hypermap. Each frame `flush_actor_occupancy` promotes the
write buffer to the read side; the write side starts clean and receives only
this frame's accepted footprints. Reads during planning therefore see a
consistent snapshot of *last* frame's occupancy, while the arbiter is the only
within-frame authority. Flushed chunks are recycled through a pool with
dirty-cell spot-resetting, so the per-frame buffer cycle allocates nothing at
steady state (see `OPTIMIZATION.md`, "Dynamic passability — single-floor
chunks + recycled flush").

## Determinism

- Phase 1 is order-independent (each actor touches only its own state).
- Phase 2/3 process in sorted-entity order, so results are reproducible
  regardless of thread scheduling.
- Bot RNG lives in the sequential brain system and is seeded (`StdRng`).
- The only non-determinism in the wider movement stack is the **arrival
  frame** of async pathfind results (`docs/pathfind-service.md`).

## Performance instrumentation

The HUD (`src/hud/perf_timings.rs`) shows per-stage timings under the FPS
counter, `last ^peak` (peaks hold for 1 s):

| Row | Meaning |
|---|---|
| `propose` | wall-clock of the whole parallel propose pass |
| `prop_think` | aggregate CPU across threads: `think_low_level` + `prepare_movement` |
| `prop_slide` | aggregate CPU: `propose_move` static slides |
| `prop_adv` | aggregate CPU: off-screen `advance_unchecked` |
| `arb_conflict` | collect + sort + snapshot + owner-grid resolution |
| `arb_apply` | outcome application + dynamic-buffer stamping |
| `arb_squeeze` | squeeze/re-entry teleports |

`propose` is wall-clock while the `prop_*` rows are summed CPU time, so
`propose` ≫ `prop_*` indicates parallel-dispatch overhead or an external stall
(task-pool contention, allocator pressure), not actor work.

## Where things live

| Concern | File |
|---|---|
| Pipeline systems, owner grid, arbitration core | `src/actor/movement.rs` |
| `Actor` trait, `ActorState`, default `propose_move`, off-screen advance | `src/actor/mod.rs` |
| BlackBot slide override, brain integration | `src/actor/black_bot.rs` |
| Static probe, dynamic buffer, footprint stamping, baked circles | `src/map/passability.rs` |
| Chunked store, double buffer, chunk recycling | `src/map/hypermap.rs` |

When editing any of this, read `.claude/SKILLS/actor-engineer/SKILL.md` first
(per `CLAUDE.md`), and treat any perf-sensitive change as an
`OPTIMIZATION.md` read-and-update task.
