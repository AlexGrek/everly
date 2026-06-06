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

- **[`Behavior`](../src/actor/brain/behavior.rs)** — a rule that runs every tick
  and raises the bot's *wishes*. It receives a [`BrainContext`] (every bot
  property it could need) and mutates the shared [`Priorities`] list. Behaviors
  may hold their own state (e.g. a hysteresis latch).
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
  `FollowPath`** (mass/inertia, wall-momentum bleed, stuck-repath, and
  elastic bot-on-bot bounce — tuned by [`FollowTuning`]).
  When `FollowPath` abandons an unfinished route due to no progress, the brain
  exposes a `stuck` status (`Brain::is_stuck`) and the bot mesh turns red until
  a new low-level action takes over.

### Bot-on-bot collision response

`FollowPath`'s tile path is planned on **static** geometry only, so it does not
route around other (moving) bots. When a step is rejected with
`BlockedByOccupancy` (another bot's footprint), `FollowPath` first bounces its
velocity elastically off the contact normal, then rolls **one** response,
weighted by [`FollowTuning`]:

| Roll band | Response |
|-----------|----------|
| `bot_reroute_chance` | Insert a single back/strafe **tile** waypoint to step away. |
| `bot_wait_chance` | Pause in place for `bot_wait_secs` to let the clump clear. |
| `bot_subtile_detour_chance` | Plan a **subtile-level detour** around the blocker (see below). |
| remainder | Just keep the elastic bounce. |

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
if a fresh bump invalidates it or it runs longer than `stuck_repath_secs`.

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

## BlackBot wiring

[`src/actor/black_bot.rs`](../src/actor/black_bot.rs):

- Spawns carry a [`Brain`] with behaviors `[RandomWalker, ChargeSelfKeeper]`, the
  default `make_high_level` factory, and a seeded `StdRng`.
- `black_bot_brain` (runs `.after(flush_actor_occupancy).before(process_actors)`,
  sequential) ticks each brain, gates depleted/broken bots (`brain.halt`), ticks
  wear/break, and applies effects via `apply_brain_effects`. It runs after the
  occupancy flush so the bot-on-bot subtile detour reads the same dynamic
  passability snapshot `process_actors` will use this frame.

### Behaviors

- **`RandomWalker`** — always wishes `RandomWalking` at value **15** (routine).
- **`ChargeSelfKeeper`** — latches once charge ≤ **25%**, releasing only at full.
  While latched it wishes `RechargeYourself` at `missing-charge%` (≥75 at the
  trigger, rising as charge falls), floored at **50** so a near-full top-up still
  outranks wandering — no early-undock thrash.

### High-level actions

- **`GoToRandomPoints`** (serves `RandomWalking`) — whenever the path finishes,
  pick a new random reachable target and follow it. Perpetual. If the low-level
  route reports `stuck`, this handler immediately retargets to a different
  random point.
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

Only the brain's `rng_seed` is saved (the behavior set is fixed by actor type); a
loaded bot rebuilds its brain and re-plans from scratch. See
[`snapshot.rs`](../src/actor/snapshot.rs) (`BlackBotBrainSnap`) and
[`level-persistence.md`](level-persistence.md).

## Adding a behavior or action

1. Add a `PriorityKind` variant (`priority.rs`).
2. Implement a `Behavior` that raises it (`behavior.rs`).
3. Implement a `HighLevelAction` that serves it (`high_level.rs`) and map the kind
   in `make_high_level`.
4. Reuse `FollowPath` / `Wait`, or add a new `LowLevelAction` (`low_level.rs`).
5. Add unit tests in the touched module (small hand-built `Hypermap<f32>` /
   `InteractiveEntityMap` — see existing tests).

[`Brain`]: ../src/actor/brain/mod.rs
[`BrainContext`]: ../src/actor/brain/mod.rs
[`BrainEffects`]: ../src/actor/brain/mod.rs
[`Priorities`]: ../src/actor/brain/priority.rs
[`FollowTuning`]: ../src/actor/brain/low_level.rs
[`ChargerEntity`]: ../src/map/interactive_entity.rs
