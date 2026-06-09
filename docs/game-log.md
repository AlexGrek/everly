# In-game event log

A top-left overlay that surfaces gameplay events (bot reroutes, charging) as
short-lived colored lines. Toggled from the bottom HUD ("Log" button), **off by
default**. Source: [`src/hud/game_log.rs`](../src/hud/game_log.rs).

## Behavior

- **Toggle.** The "Log" HUD button flips `GameLog.enabled`. Default off: events
  are still recorded, just not displayed.
- **Levels & colors.** `err` (red), `warn` (yellow), `info` (white),
  `success` (green), `unexpected` (light blue). See `LogLevel::color`.
- **Lifetime.** Each line lives `LOG_LIFETIME_SECS` (4 s) then disappears. Ages
  advance only while unpaused (`age_logs` is `run_if(not(is_paused))`), so a
  paused game freezes every timer — paused logs never expire.
- **Display.** Newest line on top, top-left of the screen.

## Architecture: hypertile-local queues

Logs are **grouped by hypertile** (one queue per [`ChunkCoord`]) rather than a
single global list. A queue is created lazily the first time an event fires in a
chunk that has none.

`GameLog` is accessed through a shared `Res<GameLog>` and mutates through
interior locks/atomics, so any system — including parallel actor systems — can
log without exclusive access:

- `enabled`: `AtomicBool` (lock-free read/toggle).
- `chunks`: `RwLock<HashMap<ChunkCoord, Mutex<ChunkLog>>>`.
  - **Warm push** (queue exists): take the **read** lock to find the chunk's
    `Mutex`, then lock that `Mutex` only for the single push.
  - **Cold push** (first event in a hypertile): take the **write** lock only to
    insert the new queue, then push under the chunk lock.
  - Every lock is released as soon as its small operation finishes.

## Storage vs. rendering (the optimization)

- Events are pushed as plain structs (`LogEntry`) holding **copied** values — no
  string formatting at push time, so the push path stays allocation-free
  regardless of whether the panel is shown.
- A line is rendered to a `String` (`LogEntry::render`) only when displayed, and
  the result is **cached** on the entry so it is never re-rendered.
- Only the queue for **the hypertile the camera is currently on** is ever
  rendered (`render_logs` maps `StrategyCamera.focus` → `ChunkCoord`). Every
  other chunk's events stay as structs and age out unrendered.
- While disabled, nothing is rendered at all.
- The UI is rebuilt only when the displayed chunk's queue changed (`dirty`) or
  the camera moved onto a different hypertile (`force`).

## Events

| Event | Level | Source |
|---|---|---|
| `<name> rerouting after collision` | `unexpected` | `log_black_bot_reroutes` (rising edge of a head-on bot-on-bot collision, the same condition that triggers the `FollowPath` bounce/detour) |
| `<name> started charging` | `info` | `black_bot_brain` when `BrainEffects::dock` fires (entering the `Charging` phase) |
| `<name> finished charging` | `success` | `black_bot_brain` when `BrainEffects::undock` fires (charge reached full) |

To add an event: add a `LogEntry` variant with its `level()` and `render()`, then
`game_log.push_world(world_x, world_y, ...)` from the relevant system.
