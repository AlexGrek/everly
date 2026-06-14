# Bot movement

How an actor's movement intent becomes a position change, end to end. This is
the deep reference for the **arbitrated movement pipeline** in
`src/actor/movement.rs`; the surrounding actor runtime (trait, state, spawning)
is documented in `docs/actor.md`, and the planning layer that decides *where* a
bot wants to go is in `docs/actor-brain.md` / `docs/pathfind-service.md`.

## Design summary

Movement is split into three phases per frame:

1. **Propose** (sequential) — every on-screen actor computes one candidate
   step, validated against **static** geometry only.
2. **Arbitrate** (sequential, deterministic) — a single authority resolves all
   actor-vs-actor occupancy conflicts within the frame.
3. **Apply + squeeze** (inside the arbitrate system) — outcomes are written
   back to the actors; hopelessly wedged bots are teleported out.

The split exists because propose is read-only against the static geometry
(each actor touches only its own state) while arbitrate is the single authority
over shared occupancy — two bots must never hold the same subtile in the same
frame, so it is resolved by one thread in a deterministic order. The previous
design (each actor checking last frame's occupancy snapshot and OR-stamping its
footprint in parallel) let two actors claim the same free cell and overlap for
a frame, and its contended parallel writes were the biggest per-frame hot spot
(see `OPTIMIZATION.md`).

> **Why propose is sequential, not `par_iter_mut`.** The per-bot work is a
> single static slide probe — single-digit microseconds for the whole crowd.
> Running it on Bevy's global `ComputeTaskPool` (via `par_iter_mut`) was a net
> loss: that pool's `scope` ticks the *global* executor while waiting for its
> own batches, so a propose tick absorbed unrelated queued compute work (render
> batching, etc.) and, at 60 Hz fixed catch-up ticks, compounded into
> frame-rate-coupled stalls. Sequential execution on the main thread removes the
> coupling for no measurable compute cost. A private thread pool would isolate
> propose too, but only matters if per-bot work grows heavy (hundreds of bots);
> at this scale it adds `unsafe` query chunking and core contention for nothing.

## Tick lifecycle — fixed 60 Hz

The whole pipeline runs on Bevy's **`FixedUpdate` schedule at 60 Hz**
(`Time::<Fixed>::from_hz(60.0)` in `GamePlugin`), decoupled from the render
frame rate: a slow render frame runs extra fixed ticks to catch up, so bot
pace never depends on fps. Inside `FixedUpdate`, `Res<Time>` yields the fixed`
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
  propose_actor_moves          PHASE 1 (sequential)
    ↓
  arbitrate_actor_moves        PHASES 2 + 3 (sequential)
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

## Phase 1 — propose (sequential)

`propose_actor_moves` iterates all actors on the main thread. Per actor:

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
  the pipeline itself never invents avoidance. A genuine head-on wedge that
  cannot be resolved locally escalates, in the BlackBot brain, to a relocate
  **and a full path recalculation against the dynamic passability map** (so the
  new route avoids the tiles other bots currently occupy); this fires both on
  collision-pressure saturation and on a sustained no-progress stall that loops
  inside recovery maneuvers — see `docs/actor-brain.md`.
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

- Phase 1 is sequential and each actor touches only its own state.
- Phase 2/3 process in sorted-entity order, so results are reproducible.
- Bot RNG lives in the sequential brain system and is seeded (`StdRng`).
- The only non-determinism in the wider movement stack is the **arrival
  frame** of async pathfind results (`docs/pathfind-service.md`).

## Performance instrumentation

The HUD (`src/hud/perf_timings.rs`) shows per-stage timings under the FPS
counter, `last ^peak` (peaks hold for 1 s):

| Row | Meaning |
|---|---|
| `propose` | wall-clock of the whole propose system |
| `prop_par` | wall-clock of the per-actor loop (≡ `prop_body` now propose is sequential) |
| `prop_body` | total CPU of the per-actor loop body |
| `prop_think` | `think_low_level` + `prepare_movement` |
| `prop_slide` | `propose_move` static slides |
| `prop_adv` | off-screen `advance_unchecked` |
| `arb_conflict` | collect + sort + snapshot + owner-grid resolution |
| `arb_apply` | outcome application + dynamic-buffer stamping |
| `arb_squeeze` | squeeze/re-entry teleports |
| `chunk_vis` | `update_visible_hypermap_chunks` (visibility / load queueing) |
| `chunk_render` | `render_chunks_30fps` (chunk mesh build + spawn/despawn) |
| `chunk_floors` | floor-switch remesh |
| `brain` | `black_bot_brain` sequential planning tick |
| `pf_dispatch` / `pf_collect` | async pathfind queue spawn / drain |
| `f_dirt` / `f_heat` | per-bot field deposits |
| `dirt_flush` / `temp_flush` | field double-buffer flushes |
| `t_diffuse` | GPU temperature diffusion tick |
| `ovl_*` | occupancy / generic overlay rebuilds |

Plus two gauge lines: `pf q=… fly=… coast=…/…` (pathfind queue depth /
in-flight / coasting bots) and `collide=… squeeze=…` (arbiter back-offs /
teleports this tick).

`prop_par` and `prop_body` should now track each other. A divergence means
something re-introduced pool coupling into propose (e.g. switching it back to
`par_iter_mut`): the loop would again tick the global `ComputeTaskPool`
executor and bill unrelated queued compute (chunk meshing, render prep) to the
propose pass — historically the cause of the frame-rate-coupled propose spike.

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
