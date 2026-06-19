# In-game event log

A top-left overlay that surfaces gameplay events (stuck bots, breakage, charge,
charging) as short-lived colored lines. Toggled from the **Overlays** panel (opened by the "Overlays" button in the bottom HUD) or its "Log" entry inside, **on by default**. The F-keys and direct buttons for other overlays live in the same panel. Source: [`src/hud/game_log.rs`](../src/hud/game_log.rs).

## Behavior

- **Toggle.** The "Log" entry inside the **Overlays** panel (or the old direct "Log" button when it existed) flips `GameLog.enabled`. Default on: all
  events on the camera's hypertile are shown. When off, events are still
  recorded but only lines with the **FORCE** flag are displayed. The F4/F5/F6
  equivalents for other overlays are also collected in the Overlays panel (opened
  from the bottom HUD "Overlays" button).
- **FORCE.** Every push carries a `force` bit. When an actor's
  [`ActorForceLogs`](../src/actor/actor_pick.rs) is enabled (inspector **Debug**
  tab → "Force logs"), its events are pushed with `force: true` and remain
  visible even while the global log overlay is off.
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
- While disabled, only FORCE-flagged lines on the camera's hypertile are
  rendered.
- The UI is rebuilt only when the displayed chunk's queue changed (`dirty`) or
  the camera moved onto a different hypertile (`force`).

## Events

| Event | Level | Source |
|---|---|---|
| `<name> stuck` | `warn` | `black_bot_brain` when [`Brain::is_stuck`](../src/actor/brain/mod.rs) becomes true (rising edge) |
| `<name> charge depleted` | `err` | `black_bot_brain` when [`Charge::is_depleted`](../src/actor/charge.rs) becomes true |
| `<name> <system> broken` | `err` | `black_bot_brain` when a [`Breakable`](../src/actor/black_bot.rs) part newly breaks (`movement engine`, `control plane`, `sensory system`) |
| `<name> started charging` | `info` | `black_bot_brain` when `BrainEffects::dock` fires (entering the `Charging` phase) |
| `<name> finished charging` | `success` | `black_bot_brain` when `BrainEffects::undock` fires (charge reached full) |
| `pathfind backlog: N queued (> T); …` | `warn` | `pathfind_dispatch` every frame while the pathfind queue depth exceeds `BACKLOG_WARN` (40) |
| `<name> wander timed out (x, y)` | `unexpected` | `black_bot_brain` when [`GoToRandomPoints`](../src/actor/brain/high_level.rs) abandons a leg after its Manhattan×3 s budget |
| `<name> skipped patrol waypoint (x, y)` | `unexpected` | `black_bot_brain` when [`GoToPatrol`](../src/actor/brain/high_level.rs) skips a waypoint after its Manhattan×3 s budget |
| `<name> reset (collision pressure)` | `warn` | `track_black_bot_collision_pressure` when a BlackBot's collision pressure reaches 50 |

To add an event: add a `LogEntry` variant with its `level()` and `render()`, then
`game_log.push_world(world_x, world_y, entry, force)` from the relevant system
(`force` from the producing actor's `ActorForceLogs`).
