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
  `FollowPath`** (mass/inertia, wall-momentum bleed, stuck-repath, bot-on-bot
  reroute/wait — tuned by [`FollowTuning`]).

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
- `black_bot_brain` (runs `.before(process_actors)`, sequential) ticks each
  brain, gates depleted/broken bots (`brain.halt`), ticks wear/break, and applies
  effects via `apply_brain_effects`.

### Behaviors

- **`RandomWalker`** — always wishes `RandomWalking` at value **15** (routine).
- **`ChargeSelfKeeper`** — latches once charge ≤ **25%**, releasing only at full.
  While latched it wishes `RechargeYourself` at `missing-charge%` (≥75 at the
  trigger, rising as charge falls), floored at **50** so a near-full top-up still
  outranks wandering — no early-undock thrash.

### High-level actions

- **`GoToRandomPoints`** (serves `RandomWalking`) — whenever the path finishes,
  pick a new random reachable target and follow it. Perpetual.
- **`GoToChargeStation`** (serves `RechargeYourself`) — `Seeking` → `Traveling` →
  `Charging`:
  - find the nearest *accessible, unoccupied* charger
    ([`InteractiveEntityMap::find_accessible_within`](../src/map/interactive_entity.rs))
    and follow a path to its (passable) tile;
  - on arrival, request `dock` and `Wait`;
  - while charging, request `recharge` (`RECHARGE_PER_S`, an **infinite station** —
    the charger's stored energy is intentionally ignored) until full, then request
    `undock` and report `Done`.

### Effects

`apply_brain_effects` (black_bot.rs) is the only place that mutates the world from
a brain decision: `dock`/`undock` set the [`ChargerEntity`] occupant; `recharge`
raises the bot's [`Charge`](../src/actor/charge.rs) toward `1.0`. A depleted bot is
immobilized **before** the tick, so a bot must trigger recharge (25%) with enough
runway to reach a charger.

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
