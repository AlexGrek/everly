# Bot charge (battery)

Every bot carries a **battery charge** that drains over time. When it reaches
zero the bot is immobilized until something refills it. Implementation lives in
[`src/actor/charge.rs`](../src/actor/charge.rs).

Charge is a plain **ECS component** on the bot entity — it is *not* part of
[`ActorState`](../src/actor/mod.rs). It sits on the same entity as `ActorObject`
and the bot's visual ([`BlackBotVisual`](../src/actor/black_bot.rs) /
[`GlitchBotVisual`](../src/actor/glitch_bot.rs)), so any system can query it
without reaching into the actor trait object.

## The `Charge` component

```rust,ignore
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Charge {
    pub level: f32, // always within [0.0, 1.0]
}
```

| Method | Behavior |
|--------|----------|
| `Charge::new(level)` | Clamps `level` into `[0.0, 1.0]`. The only constructor — `level` is never set raw, so the invariant holds everywhere. |
| `Charge::random(rng)` | Starting charge drawn uniformly from `[SPAWN_CHARGE_MIN, SPAWN_CHARGE_MAX]` = `[0.3, 1.0]`. Takes the same seeded `StdRng` the spawner already threads, so spawns stay deterministic. |
| `Charge::is_depleted()` | `true` iff `level <= 0.0`. The single source of truth for "can't move". |

## Lifecycle

### Spawn

| Path | Starting charge |
|------|-----------------|
| Editor spawn ([`spawn_glitch_bot`](../src/actor/glitch_bot.rs) / [`spawn_black_bot`](../src/actor/black_bot.rs)) | `Charge::random` → `0.3..=1.0` |
| Snapshot load ([`spawn_level_actors`](../src/actor/snapshot.rs)) | Restores the saved `charge`; a missing field defaults to full (`1.0`) |

### Discharge

[`ChargePlugin`](../src/actor/charge.rs) runs `discharge_actors` every frame
while `GameState::InGame` **and** not [`Paused`](../src/actor/mod.rs):

```text
level -= DISCHARGE_PER_S * dt        // DISCHARGE_PER_S = 0.002 → ~500 s full→empty
level  = level.max(0.0)              // clamp at 0.0; never goes negative
```

The drain is gated on `not(is_paused)` so a paused simulation freezes the
battery along with everything else — consistent with the rest of the actor
pipeline. Already-depleted bots are skipped (no work, no underflow).

### Depletion disables movement

A depleted bot (`is_depleted()`) is stopped **in its think system**, not in
[`process_actors`](../src/actor/mod.rs):

- [`glitch_bot_think`](../src/actor/glitch_bot.rs) and
  [`black_bot_think`](../src/actor/black_bot.rs) detect depletion at the top of
  their per-bot loop, zero the [`move_buffer`](../src/actor/mod.rs), and
  `continue` — skipping pathing/wander logic entirely.
- The glitch bot additionally leaves its **accumulator frozen**. This is the key
  reason the gate must live in `think`: its mesh renders from
  `last_accepted_center_subtile + accumulator`, so if `think` kept advancing the
  accumulator while `process_actors` discarded the motion, the bot would slide
  visually without moving on the collision grid. The black bot's mesh follows
  the float `center`, which only `try_move` advances, so zeroing the buffer is
  enough there — but both are handled the same way for uniformity.

Because the buffer is empty, `process_actors`/`try_move` re-stamp the bot's
existing footprint in place: it holds position and keeps its dynamic-occupancy
cell. Recharge is handled by BlackBot brain logic (`GoToChargeStation`) which
seeks accessible chargers from `InteractiveEntityMap`, docks, and applies
`RECHARGE_PER_S` while charging.

## Inspector display

The HUD actor inspector ([`src/hud/actor_inspector.rs`](../src/hud/actor_inspector.rs))
shows a `charge` row for any bot via
[`charge_row`](../src/actor/inspect.rs), rendered as a whole percent:

- `72%` for a charged bot,
- `0% (depleted)` once empty, so the immobilized state is obvious at a glance.

The row is emitted by `collect_inspect_rows(obj, charge, …)`, which the modal
feeds from an `Option<&Charge>` query (entities without the component simply omit
the row).

## Persistence

Charge round-trips through the level save ([`actors.yaml`](level-persistence.md#actors-actorsyaml)):
each `SavedActor` carries a `charge: f32` field with `#[serde(default)]` = full,
so pre-charge save files still load. See
[`level-persistence.md`](level-persistence.md) for the full actor snapshot format.

## Constants

| Constant | Value | Meaning |
|----------|-------|---------|
| `DISCHARGE_PER_S` | `0.002` | Fraction of full charge drained per second (~500 s full→empty) |
| `SPAWN_CHARGE_MIN` | `0.3` | Lower bound of random spawn charge |
| `SPAWN_CHARGE_MAX` | `1.0` | Upper bound of random spawn charge |

All three live in [`src/actor/charge.rs`](../src/actor/charge.rs).

## Not yet wired

- **Uniform drain.** Every bot discharges at the same rate regardless of class,
  speed, or activity. Per-class rates would live as a field on `Charge` or a
  small per-class lookup, not a single global constant.

## Related code

| Concern | Location |
|---------|----------|
| Component + discharge + plugin | [`src/actor/charge.rs`](../src/actor/charge.rs) |
| Movement gate on depletion | [`glitch_bot.rs`](../src/actor/glitch_bot.rs), [`black_bot.rs`](../src/actor/black_bot.rs) |
| Spawn (random charge) | editor spawns in the two bot modules |
| Inspector row | [`inspect.rs`](../src/actor/inspect.rs), [`actor_inspector.rs`](../src/hud/actor_inspector.rs) |
| Persistence | [`snapshot.rs`](../src/actor/snapshot.rs) |

## See also

- [`actor.md`](actor.md) — actor runtime loop, `move_buffer`, and why the gate
  belongs in `think`.
- [`level-persistence.md`](level-persistence.md) — `actors.yaml` format.
- [`interactive-entities.md`](interactive-entities.md) — `Charger` entities (the
  intended future recharge source).
