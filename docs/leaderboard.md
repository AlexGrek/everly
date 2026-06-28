# Bot leaderboard

A HUD modal listing every bot in the world, ranked, plus world-wide aggregate
stats. Source: `src/hud/leaderboard.rs` (`LeaderboardPlugin`).

## Opening / closing

- **"Bots" button** in the bottom HUD bar, or the **`L`** key, toggles it.
- Close with the **X** button, **Escape**, or clicking the scrim.
- Clicking a row **selects that bot** (opening the right-docked actor inspector,
  via `SelectedActor`) and closes the leaderboard so the inspector is
  unobstructed.

The panel is a centered, scrim-backed modal styled to match
`hud::overlays`. Content is rebuilt lazily — immediately on open and then twice a
second (`REFRESH_INTERVAL_S`) while open — so charge and health stay current
without per-frame churn. Nothing rebuilds while it is closed.

## Per-bot rows

Bots are queried by `With<ActorInspectable>` and **ranked by charge, fullest
first** (a depleted bot sinks to the bottom; a bot with no `Charge` sorts as
full). Each row shows four columns:

| Column | Source | Notes |
|---|---|---|
| Name | `Name` (via `display_actor_name`) | grows to fill |
| Role | `BotSpecialization` | `DO_NOTHING` / `PATROL` / `FIXER`, color-coded |
| Charge | `Charge::level` | `0–100%`, green > 50% · yellow > 20% · red otherwise |
| Health | `Breakable` | `OK` / `N hit` / `OFFLINE (N)` |

**Health** reflects only the breakable sub-systems (movement engine, control
plane, sensory system). A broken movement engine *or* control plane immobilizes
the bot, so any of those reads `OFFLINE`; other breakages read `N hit`
(`HEALTH_DAMAGED`). Charge depletion is reported in the Charge column, not here.

## Aggregate stats

A block above the list reports world-wide counts. Bots are bucketed
**mutually-exclusively by priority** so the three buckets partition the total:

1. **Discharged** — `Charge::is_depleted()` (charge ≤ 0).
2. **Broken** — otherwise, movement engine or control plane broken
   (immobilized by damage).
3. **Alive** — otherwise (operational), further split by specialization
   (`DO_NOTHING` / `PATROL` / `FIXER`).

Each line shows the count and its percentage of the total (`pct`). A bot that is
both discharged and broken counts only as discharged (highest priority).

## Related

- `src/hud/actor_inspector.rs` — the per-bot detail panel a row click opens.
- `docs/charge.md` — `Charge` semantics.
- `src/actor/black_bot.rs` — `BotSpecialization`, `Breakable`.
