# Bot charge (battery)

Every bot carries a **battery charge** that drains over time. When it reaches
zero the bot is immobilized until something refills it. Implementation lives in
[`src/actor/charge.rs`](../src/actor/charge.rs).

Charge is a plain **ECS component** on the bot entity — it is *not* part of
[`ActorState`](../src/actor/mod.rs). It sits on the same entity as `ActorObject`
and the bot's visual ([`BlackBotVisual`](../src/actor/black_bot.rs)),
so any system can query it without reaching into the actor trait object.

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
| Editor spawn ([`spawn_black_bot`](../src/actor/black_bot.rs)) | `Charge::random` → `0.3..=1.0` |
| Snapshot load ([`spawn_level_actors`](../src/actor/snapshot.rs)) | Restores the saved `charge`; a missing field defaults to full (`1.0`) |

### Discharge

[`ChargePlugin`](../src/actor/charge.rs) runs `discharge_actors` every frame
while `GameState::InGame` **and** not [`Paused`](../src/actor/mod.rs):

```text
level -= DISCHARGE_PER_S * dt * mult  // DISCHARGE_PER_S = 0.002 → ~500 s full→empty at mult = 1
level  = level.max(0.0)               // clamp at 0.0; never goes negative
```

`mult` is the bot's **genetic battery-drain multiplier**,
`BEST_BATTERY_DRAIN_MULT / battery_quality`, read from its
[`Genome`](../src/actor/genetics.rs)
([`GeneticTraits::discharge_multiplier`]); a bot with no genome drains at the
baseline (`mult = 1`). Battery quality is half-normal and capped at 100% (the most
common variant), so a top-quality battery drains at `mult = 0.5` (~1000 s
full→empty, the longest-lasting) and a tail of lower-quality batteries scale up
toward and past the baseline. See [actor.md § Genetics](actor.md#genetics).

The drain is gated on `not(is_paused)` so a paused simulation freezes the
battery along with everything else — consistent with the rest of the actor
pipeline. Already-depleted bots are skipped (no work, no underflow).

### Depletion disables movement

A depleted bot (`is_depleted()`) is stopped **in its think system**, not in
[`process_actor_moves`](../src/actor/movement.rs):

- [`black_bot_brain`](../src/actor/black_bot.rs) detects depletion at the top of
  its per-bot loop, zeros the [`move_buffer`](../src/actor/mod.rs), and
  `continue` — skipping pathing logic entirely.

Because the buffer is empty, `process_actor_moves` records the held footprint
(`proposed_center == origin`) in place (no delta) and re-stamps the bot's existing
footprint: it holds position and keeps its dynamic-occupancy cell. Recharge is
handled by BlackBot brain logic (`GoToChargeStation`) which seeks accessible
chargers from `InteractiveEntityMap`, docks, and applies `RECHARGE_PER_S`
while charging.

### Rescue of a fully-discharged bot

A bot that hits 0% can no longer reach a charger on its own. Like a *broken*
bot, it **asks for help**: `maintain_dispatch_queue` posts a `Battery`
[`RepairRequest`](dispatch.md) for it. A **fixer** bot fetches a battery from the
parts depot and delivers it, recharging the discharged bot to a random **50–70%**
(`recharge_target` effect). That partial top-up lets the bot move again and seek a
charger for the rest. See [`dispatch.md`](dispatch.md).

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
| `DISCHARGE_PER_S` | `0.002` | Baseline fraction of full charge drained per second, for a perfect (quality `1.0`) battery (~500 s full→empty) |
| `SPAWN_CHARGE_MIN` | `0.3` | Lower bound of random spawn charge |
| `SPAWN_CHARGE_MAX` | `1.0` | Upper bound of random spawn charge |

All three live in [`src/actor/charge.rs`](../src/actor/charge.rs).

## Not yet wired

- **Activity-independent drain.** A bot's drain rate is scaled by its genetic
  battery quality (see above) but not by what it is *doing* — moving, idling, and
  charging-queue waiting all drain at the same genetic rate. Activity-weighted
  drain would multiply by a per-frame activity factor in `discharge_actors`.

## Related code

| Concern | Location |
|---------|----------|
| Component + discharge + plugin | [`src/actor/charge.rs`](../src/actor/charge.rs) |
| Movement gate on depletion | [`black_bot.rs`](../src/actor/black_bot.rs) |
| Spawn (random charge) | [`spawn_black_bot`](../src/actor/black_bot.rs) |
| Inspector row | [`inspect.rs`](../src/actor/inspect.rs), [`actor_inspector.rs`](../src/hud/actor_inspector.rs) |
| Persistence | [`snapshot.rs`](../src/actor/snapshot.rs) |

## See also

- [`actor.md`](actor.md) — actor runtime loop, `move_buffer`, and why the gate
  belongs in `think`.
- [`level-persistence.md`](level-persistence.md) — `actors.yaml` format.
- [`interactive-entities.md`](interactive-entities.md) — `Charger` entities (the
  intended future recharge source).
