# Actor Brain

The **brain** is the OOP high-level decision layer for smart actors, in
[`src/actor/brain/`](../src/actor/brain/). It sits *above* the deterministic
low-level movement pipeline (`Actor::try_move`, `process_actors`) described in
[`actor.md`](actor.md). BlackBot is its first consumer.

## Concepts

```
Behaviors  ──raise──▶  Priorities (sorted wishes)
                              │ top()
                              ▼
                     High-level action  (exactly one, exclusive)
                              │ dictates
                              ▼
                     Low-level action   (Wait / FollowPath)
                              │ execute()
                              ▼
                     ActorState.move_buffer  ──▶ process_actors → try_move
```

- **[`Behavior`](../src/actor/brain/behavior/mod.rs)** — a rule that runs every
  tick and raises the bot's *wishes*. It receives a [`BrainContext`] (every bot
  property it could need) and mutates the shared [`Priorities`] list. Behaviors
  may hold their own state (e.g. a hysteresis latch). The `Behavior` trait lives
  in [`behavior/mod.rs`](../src/actor/brain/behavior/mod.rs); **each behavior is
  its own module** under `behavior/` (`random_walker.rs`, `patroller.rs`,
  `charge_self_keeper.rs`), with constants shared between them in
  [`behavior_utils.rs`](../src/actor/brain/behavior/behavior_utils.rs).
- **[`Priority`](../src/actor/brain/priority.rs)** — a `kind` + a `value`
  (uncapped `f32`). [`Priorities`] is the reused, sorted "wishes array";
  `top()` returns the dominant wish. Value bands:

  | Range | Meaning |
  |-------|---------|
  | 0–30  | basic routine |
  | 30–50 | high-priority routine |
  | 50–70 | reaction to interruptions |
  | 70–90 | emergency |

- **[`HighLevelAction`](../src/actor/brain/high_level.rs)** — the single,
  exclusive task the bot is pursuing. The dominant priority's `kind` selects it
  (via the brain's factory); a different dominant kind **pre-empts** it. It
  `update`s the low-level action and may request [`BrainEffects`].
- **[`LowLevelAction`](../src/actor/brain/low_level.rs)** — what the bot is
  physically doing this frame: `Idle`, `Wait(time)`, or `FollowPath(path)`.
  `execute` writes `move_buffer`. **All of BlackBot's movement feel lives in
  `FollowPath`** (mass/inertia, wall-momentum bleed, stuck-repath, and the
  head-on bot-on-bot response — elastic bounce then either a detour or a
  step-aside-and-pause; rear bumps ignored — tuned by [`FollowTuning`]).
  When `FollowPath` abandons an unfinished route due to no progress, the brain
  exposes a `stuck` status (`Brain::is_stuck`) and the bot mesh flashes yellow,
  then eases back to black over a few seconds. The stall trigger fires when the
  bot has not moved more than [`FollowTuning::stuck_progress_eps`] from a
  reference position for [`FollowTuning::stuck_repath_secs`], regardless of
  distance to the active waypoint (near goals are no longer exempt).
  Charger-queue membership pauses the timer. [`Wait::retry`] uses the same stall
  rule so patrol/wander bots that cannot plan a route still recover instead of
  idling until depletion.

  **Relocate before rescheduling.** A stalled bot does *not* immediately
  abandon (which would replan expensive A\* every cycle while it keeps wedging a
  chokepoint and dragging every queued bot's velocity toward zero). Instead it
  enters an *escape*: `find_escape_cell` scans the `ESCAPE_SEARCH_TILES`-radius
  square for the **nearest cell whose whole footprint is clear of other
  creatures and static geometry** (its own current footprint is bypassed, via
  `DynamicPassabilityMap::probe_footprint`), then drives to that cell's center
  with the normal braking profile. Only on arrival does it mark the route
  abandoned, so `Brain::is_stuck` (yellow flash + `BotStuck` log) and the
  high-level reschedule happen **from a free, tile-centered position** the bot
  has just vacated into — not from inside the jam. A secondary stall timer
  during the escape, plus the no-avoidance-data fallback (headless tests),
  abandons in place so the maneuver can never loop forever.

  High-level replan (`GoToPatrol`, `GoToRandomPoints`, recharge stuck handler)
  runs only on the **rising edge** of `is_stuck` / `is_finished`, not every
  frame while `Wait::retry` stays stalled — so one trapped bot cannot serialize
  the sequential brain loop with a per-frame A\* storm.

### BlackBot status colors

`sync_black_bot_status_visual` (in `black_bot.rs`, runs `.after(process_actors)`)
recolors the sphere by priority: **white** when the control plane breaks, a
**yellow stuck flash** when `Brain::is_stuck` (relit to full yellow, then
fading back over `STUCK_FLASH_FADE_SECS`), otherwise a **collision flash** — a
blocked movement step relights `BlackBotVisual::collision_flash` to `1.0`, which
then fades linearly back to black over `COLLISION_FLASH_FADE_SECS` (a quick red
blink). A wall graze (`BlockedByStatic`) always counts, but a bot-on-bot bump
(`BlockedByOccupancy`) only flashes when it is **head-on** — a rear bump is
ignored, exactly mirroring the movement response below (both call
[`is_front_collision`](../src/actor/mod.rs)). The material is only rewritten when
the displayed color changes, so a settled bot costs no per-frame asset writes.

### Bot-on-bot collision response

`FollowPath`'s tile path is planned on **static** geometry only, so it does not
route around other (moving) bots. When a step is rejected with
`BlockedByOccupancy` (another bot's footprint):

1. **Front/back gate.** Classify the contact with `is_front_collision` against
   the bot's heading (its velocity, or `direction` when stopped). A **rear bump**
   (blocker behind the heading) is **ignored entirely** — no bounce, step, or
   detour. Only a **head-on or side** contact provokes a response. (Ambiguous
   cases — degenerate normal or a stationary bot — count as front.)
2. **Bounce** the velocity elastically off the contact normal (recoil; feel only).
3. **Roll the response (`FollowTuning::bot_detour_chance`, default `0.5`).**
   - *Detour* → plan a **subtile-level detour** around the blocker (see below)
     toward the next path node.
   - *Step aside + pause* → step to an adjacent cell and hold there for a random
     1–3 s (`STEP_BACK_WAIT_*_SECS`). The step is usually **straight back** to the
     previously occupied cell (`track_tiles` records `prev_tile`), but
     `FollowTuning::bot_strafe_chance` (default `0.3`) of the time it **strafes
     left/right** relative to the heading instead (falling back to straight-back
     if the chosen side is blocked). The pause arms only once the bot *reaches*
     that cell (`pending_wait` → `contact_wait_s`); it then brakes to a stop with
     the same deceleration profile as normal travel before the hold timer runs.

   A detour is **forced** (regardless of the roll) when no valid step cell is
   known, and the step is the fallback when a rolled detour can't be planned (no
   avoidance data / no clear route).

This applies to bot-on-bot bumps only; a wall graze (`BlockedByStatic`) is left
to the normal wall-slide / stuck-repath path.

The subtile detour is a *second, finer* pathfinding pass for short distances:
[`astar_subtile_detour`](../src/map/hypermap_pathfind.rs) runs a bounded
4-neighbour A\* on the subtile grid (`1 tile = SUBTILE_COUNT subtiles`) from the
bot's current subtile to the **next already-calculated path node**. Each
candidate subtile is accepted only when the bot's whole circular footprint —
i.e. its **size** (`radius_subtiles`) — is clear of both static geometry and
other creatures, tested via
[`DynamicPassabilityMap::probe_footprint`](../src/map/passability.rs). The
search is kept local: it is skipped past `DETOUR_MAX_SPAN_SUBTILES`, confined to
the start/goal bounding box grown by `DETOUR_PAD_SUBTILES`, and capped at
`DETOUR_MAX_EXPANDED` expansions. The resulting subtile staircase is collapsed
to its corners and followed (in tile-space float coordinates) until the bot
reaches that next node, then the normal tile path resumes. A detour is dropped
if a **fresh** head-on bump invalidates it (a new blocker subtile, or contact
after a frame with no occupancy error) or it runs longer than
`stuck_repath_secs`. While two bots stay pressed together the movement error
persists every frame, but the response runs only on the **rising edge** — the
same rising-edge latch pattern as the game-log stuck event — so an in-flight
detour is not wiped and replanned each tick. Choosing a detour also removes any unreached
step-aside waypoint that was inserted by an earlier bump on the same path.

This needs occupancy data the rest of the brain doesn't: `BrainContext` carries
an optional [`AvoidanceViews`](../src/actor/brain/mod.rs) (the dynamic map, the
static subtile cache, and the actor's `blocked_flags`). It is `Some` only in the
live `black_bot_brain` system (which runs after `flush_actor_occupancy` so it
reads the current occupancy snapshot) and `None` everywhere else, which disables
the detour.

## Tick (`Brain::tick`)

Each frame, the owning ECS system builds a `BrainContext` and calls
`Brain::tick`:

1. `priorities.clear()`, then every behavior raises its wish.
2. `priorities.top()` → if its `kind` differs from the current action's kind,
   replace the current action (and reset the low-level action to `Idle` so the
   new plan starts fresh) — this is pre-emption.
3. the current action `update`s: sets/replaces the low-level action, returns
   [`BrainEffects`]. If it reports `Done`, the brain drops it (re-plans next tick).
4. the low-level action `execute`s, writing this frame's movement intent.

`tick` returns the [`BrainEffects`]; it never touches ECS resources itself. The
owning system applies them. Steady-state ticks allocate nothing (`Priorities`
reuses its buffer; effects are a fixed-size struct; a path `Vec` is allocated
only on a re-path).

## Specializations

Every BlackBot has a **specialization** (`BotSpecialization` in
[`black_bot.rs`](../src/actor/black_bot.rs)) — rolled randomly at spawn
(`BotSpecialization::roll`: `PATROL` with probability **1/4**, else
`DO_NOTHING`). A specialization is just a *named behavior set* plus a **ring
color** rendered around the sphere:

| Specialization | Behaviors | Routine | Ring |
|----------------|-----------|---------|------|
| `DO_NOTHING` | `[RandomWalker, ChargeSelfKeeper]` | wander to random cells | black |
| `PATROL` | `[Patroller, ChargeSelfKeeper]` | stick to a fixed loop of cells | blue |

`BotSpecialization::build_brain` constructs the matching [`Brain`]; both share
`ChargeSelfKeeper`, so any specialization still leaves its routine to recharge.
The ring is a flat torus child of the actor root (`spawn_bot_ring`), positioned
each frame by `sync_black_bot_transforms`; it carries no pick mesh, so the status
recolor leaves it alone and it keeps its specialization color for life.

The specialization is **persisted** (see [Persistence](#persistence)); the patrol
*loop* is not — it is regenerated on load.

## BlackBot wiring

[`src/actor/black_bot.rs`](../src/actor/black_bot.rs):

- Spawns carry a [`Brain`] whose behavior set is chosen by the bot's
  [specialization](#specializations) (`BotSpecialization::build_brain`), the
  default `make_high_level` factory, and a seeded `StdRng`.
- `black_bot_brain` (runs `.after(flush_actor_occupancy).before(process_actors)`,
  sequential) ticks each brain, gates depleted/broken bots (`brain.reset` — wipes
  the plan and clears movement intent), ticks wear/break, and applies effects via
  `apply_brain_effects`. It runs after the occupancy flush so the bot-on-bot
  subtile detour reads the same dynamic passability snapshot `process_actors` will
  use this frame.
  - **Offline eviction:** when a bot first becomes non-operational
    (depleted or broken), the gate calls
    `InteractiveEntityMap::evict_actor_everywhere`, dropping it from every
    charger's wanting/waiting queue and releasing any charger it occupied, so a
    dead bot never blocks a station or queue slot. This map-wide sweep runs once
    on the transition (latched by `BlackBotVisual::offline_released`), not every
    frame; the latch clears when the bot is operational again.

### Behaviors

Each behavior is a module under [`behavior/`](../src/actor/brain/behavior/); the
shared routine wish value lives in `behavior_utils.rs`.

- **`RandomWalker`** (`DO_NOTHING`) — always wishes `RandomWalking` at the routine
  value **15** (`ROUTINE_WISH_VALUE`).
- **`Patroller`** (`PATROL`) — always wishes `Patrolling` at the same routine
  value **15**, so a recharge need still pre-empts it.
- **`ChargeSelfKeeper`** (all specializations) — latches once charge ≤ **25%**, releasing only at full.
  While latched it wishes `RechargeYourself` at `missing-charge%` (≥75 at the
  trigger, rising as charge falls), floored at **50** so a near-full top-up still
  outranks wandering — no early-undock thrash.

### High-level actions

- **`GoToRandomPoints`** (serves `RandomWalking`) — whenever the path finishes,
  pick a new random reachable target and follow it. Perpetual. If the low-level
  route reports `stuck`, this handler immediately retargets to a different
  random point.
- **`GoToPatrol`** (serves `Patrolling`) — walks a *fixed* loop of cells forever.
  The loop lives on the bot's `Patrol` component (generated once, lazily, by
  `black_bot_brain` via `generate_patrol_loop` from the spawn tile, then never
  changed) and is surfaced to the action through `BrainContext::patrol_loop`. The
  action itself is transient — the brain rebuilds it whenever `Patrolling`
  becomes dominant again (e.g. after a recharge pre-empts it) — so on (re)creation
  it snaps its cursor to the loop waypoint **nearest the bot**, resuming "where it
  stopped". On `stuck`, it skips the unreachable waypoint and tries the next.
  Perpetual.
- **`GoToChargeStation`** (serves `RechargeYourself`) — `Seeking` → `Traveling` →
  `WaitingQueue` → `Charging`:
  - scan chargers in the bot's 4 nearest hypertiles (current chunk + nearest X/Y
    neighbors + diagonal), rank by reachable path length, then apply queue policy:
    prefer stations with `< 2` waiting bots; if all candidates are busier, pick
    `2nd`/`3rd`/... nearest based on the nearest station's waiting depth;
  - queue-selection and "enter waiting zone" transitions are evaluated on main-tile
    changes (the actor-brain integration's usual coarse cadence);
  - on selection, join that station's **wanting** queue;
  - when Manhattan distance to the station drops below 5 tiles, move from wanting
    into the **waiting** queue and stop near the station;
  - while waiting, re-check availability after short random waits; only approach
    and dock when the station is free and this bot is first in waiting queue;
  - while charging, request `recharge` (`RECHARGE_PER_S`, an **infinite station** —
    the charger's stored energy is intentionally ignored) until full, then request
    `undock` and report `Done`.
  - if movement reports `stuck` while not already charging, clear queue state and
    re-run charger search immediately.

### Effects

`apply_brain_effects` (black_bot.rs) is the only place that mutates the world from
a brain decision: queue add/remove requests update station wanting/waiting queues,
`dock`/`undock` set the [`ChargerEntity`] occupant, and `recharge` raises the bot's
[`Charge`](../src/actor/charge.rs) toward `1.0`. Waiting-queue membership is removed
when docking succeeds. A depleted bot is immobilized **before** the tick, so a bot
must trigger recharge (25%) with enough runway to reach a charger.

## Persistence

A saved BlackBot stores its brain's `rng_seed` **and its `specialization`** (so a
loaded bot keeps its role and ring). The behavior set is then fixed by the
specialization, so nothing else about the brain is serialized: a loaded bot
rebuilds its brain and re-plans from scratch, and a `PATROL` bot **regenerates its
patrol loop** on first tick (the loop is not persisted). A `specialization`
missing from older `actors.yaml` loads as `DO_NOTHING`. See
[`snapshot.rs`](../src/actor/snapshot.rs) (`BlackBotBrainSnap`, `SavedActor::BlackBot`)
and [`level-persistence.md`](level-persistence.md).

## Adding a behavior or action

1. Add a `PriorityKind` variant (`priority.rs`).
2. Implement a `Behavior` that raises it in **its own module** under
   `behavior/` (declare it in `behavior/mod.rs` and re-export it); put any value
   shared with another behavior in `behavior_utils.rs`.
3. Implement a `HighLevelAction` that serves it (`high_level.rs`) and map the kind
   in `make_high_level`.
4. Reuse `FollowPath` / `Wait`, or add a new `LowLevelAction` (`low_level.rs`).
5. Add unit tests in the touched module (small hand-built `Hypermap<f32>` /
   `InteractiveEntityMap` — see existing tests).

To add a new **specialization** instead, extend `BotSpecialization` in
`black_bot.rs` (a behavior set + ring color), roll it in `BotSpecialization::roll`,
and add a `#[serde(default)]`-friendly variant — persistence is automatic.

[`Brain`]: ../src/actor/brain/mod.rs
[`BrainContext`]: ../src/actor/brain/mod.rs
[`BrainEffects`]: ../src/actor/brain/mod.rs
[`Priorities`]: ../src/actor/brain/priority.rs
[`FollowTuning`]: ../src/actor/brain/low_level.rs
[`ChargerEntity`]: ../src/map/interactive_entity.rs
