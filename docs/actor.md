# Actor Runtime

This document explains the low-level actor subsystem in `src/actor/mod.rs`.

## Overview

The actor subsystem provides:

- A generic `Actor` trait for per-frame behavior.
- `ActorState` as common mutable runtime data.
- A single processing system (`ActorPlugin`) that runs actor logic each in-game frame.
- Movement and occupancy integration through `DynamicPassabilityMap`.

The low-level pipeline is deterministic and synchronous. High-level planning runs separately and feeds intent into `move_buffer`. BlackBot's planner is the OOP **brain** (behaviors → priorities → high/low-level actions, plus charge-station recharge) — see [actor-brain.md](actor-brain.md).

## Level persistence

On `InGame` enter, [`ActorSnapshotPlugin`](../src/actor/snapshot.rs) loads
`levels/level_{name}/actors.yaml` when present and spawns saved black bots
with [`LevelActor`](../src/actor/snapshot.rs). Dynamic passability footprints are
restored immediately after spawn. Positions and state are written only when the
player presses map editor **Save** (same action as geometry). Full format and load
order: [`level-persistence.md`](level-persistence.md).

## Per-frame lifecycle

All movement runs in a **single sequential system**, `process_actor_moves`
(see `src/actor/movement.rs`), preceded by the occupancy flush:

1. `flush_actor_occupancy` — promote passability write→read, clear write.
2. `process_actor_moves` (sequential), in four stages:
   - **Think + propose**: for each actor, clear `last_movement_error`,
     `think_low_level()`, `prepare_movement()`, then `propose_move(static_cache)`
     — a **static-only** validated step recorded in the actor's [`ActorShadow`].
     Off-screen actors `advance_unchecked`; re-entrants are queued.
   - **Resolve**: creature-on-creature conflicts are resolved over a per-frame
     owner grid (entity-sorted, deterministic).
   - **Apply + commit**: each outcome is applied and every final footprint is
     stamped into the passability write buffer.
   - **Re-entry placement**: off-screen re-entrants are teleported to a free cell.
3. Field interactions (e.g. dirt) — after movement; see `docs/field-interactions.md`.

Movement for frame `N` writes occupancy into the passability write buffer, and
that occupancy becomes visible from the read buffer after the flush at the start
of frame `N+1`.

## Actor data model

`ActorState` stores:

- `center: Vec2` — actor center in tile-space floats. Updated by the exact float `tile_delta` every frame — never quantized.
- `radius_subtiles: i32` — circular occupancy radius in subtiles.
- `rotation: f32` — actor orientation.
- `heading: Vec2` — current **movement direction** as a unit vector (`Vec2::ZERO` = unknown); see [Movement direction](#movement-direction-heading). Published each frame by the think system so other bots can read where this one is going. Transient — recomputed from the plan each tick, not serialized.
- `move_buffer: ActorMoveBuffer` — relative motion for this frame (two displacement channels):
  - `tile_delta: Vec2` — exact float displacement in tile-space, applied to `center` every frame for smooth rendering.
  - `subtile_shift: IVec2` — integer subtile steps for the passability collision grid; typically `(0, 0)` on most frames, non-zero only when accumulated float motion crosses a subtile boundary.
  - `rotation_shift: f32`
- `last_movement_error: Option<ActorMovementError>` — cleared every low-level step.
- `last_accepted_center_subtile: Option<IVec2>` — integer subtile center of the last accepted occupancy update; `None` for a brand-new actor.
- `last_accepted_radius_subtiles: i32` — radius (in subtiles) of that last accepted footprint; tracked separately from `radius_subtiles` so an actor that resizes still re-stamps the correct old circle on rejection.
- `next_waypoint_hint: Option<Vec2>` — destination hint set each frame by the actor's think system (e.g. current path waypoint). When an off-screen actor re-enters a rendered chunk and its footprint overlaps static geometry, `resolve_offscreen_collision` tries this tile before falling back to a ring search. May be `None` for actors that don't pathfind.
- `field_main_tile: Option<IVec2>` — last observed **main tile** for hypermap field coupling (dirt, etc.); see [Main tile](#main-tile) and `docs/field-interactions.md`.
- `dirtiness: f32` — actor's own dirt level `0.0..=1.0`; exchanged with the floor `DirtMap` on each main-tile transition (see `docs/field-interactions.md`). Actors spawn clean (`0.0`) and this is **not** serialized in snapshots — a loaded actor starts clean again.

> **Previous-footprint encoding.** The previous frame's occupied cells are described compactly by the `(last_accepted_center_subtile, last_accepted_radius_subtiles)` pair and the baked `CircleShadow` for that radius — *not* by storing a `Vec<IVec2>`. This keeps the per-actor hot path allocation-free; self-overlap is an `O(1)` bitmap test against the previous shadow.

### Dual-channel movement

Movement uses two parallel channels to separate smooth rendering from discrete collision:

| Channel | Type | Updated | Purpose |
|---|---|---|---|
| `tile_delta` | `Vec2` | every frame | exact float center displacement — makes the rendered position perfectly smooth |
| `subtile_shift` | `IVec2` | sparse | integer subtile steps for the passability grid — only non-zero when a subtile boundary is crossed |

A typical continuous-movement actor (like `BlackBot`) computes velocity in subtiles/s, converts to tile-space for `tile_delta`, and accumulates fractional subtile displacement across frames. When the accumulator's integer part becomes non-zero, that integer is emitted as `subtile_shift` and the remainder is carried forward.

### Coordinate units

- `1 tile = 5 subtiles` (`SUBTILE_COUNT`).
- Collision and occupancy tests are performed in **integer subtiles** via `subtile_shift`.
- `center` is always float tile coordinates; it advances by `tile_delta` every frame, never quantized to the subtile grid.

### Main tile

**Main tile** = which world tile an actor is nearest to, derived from float `center`:

```text
main_tile = (round(center.x), round(center.y))   // tile units, 1 unit = 1 m
```

Canonical API: [`actor_main_tile`](../src/actor/mod.rs) and [`ActorState::main_tile_i32`](../src/actor/mod.rs).

| API | Quantization | Used for |
|-----|----------------|----------|
| `actor_main_tile(center)` | **round** | Field interactions (`field_main_tile`), [`BlackBot`](../src/actor/black_bot.rs) inspector (`BlackBotVisual.main_tile`) |
| `center_subtile_i32()` | **floor**(`center × 5`) | Passability grid, footprints, collision (first frame only) |
| `center_tile_i32()` | **floor**(`center`) | Legacy helper; prefer `actor_main_tile` for tile identity |

Do **not** use `floor(center)` for main-tile or field logic — an actor spawned at tile center `(0.5, 0.5)` would be assigned the wrong tile. Subtile collision intentionally keeps `floor` so the footprint stays inside the subtile that contains the float position.

After [`process_actor_moves`](../src/actor/movement.rs), [`dirt_actor_interaction`](../src/map/field_interactions.rs) updates `field_main_tile` and applies field rules to the tile the actor **left** when main tile changes. See `docs/field-interactions.md`.

## Movement direction (heading)

`ActorState.heading` is the actor's current movement direction as a **unit
vector** (`Vec2::ZERO` when it has none — e.g. a bot that has never moved). It is
published every frame by the bot's think system so **other** bots can read where
it is going (combine with [`CellOccupancy`](../src/map/cell_occupancy.rs) to find
who is on a nearby tile, then read their `heading`). It also doubles as the bot's
facing/rotation, which is **not yet rendered** (bots are spheres), so `rotation`
stays unused for now.

Heading is **not** the same as velocity. A bot wedged against another or braking
has ~zero velocity but still *intends* to move toward its next path node — so
velocity alone cannot tell you where a stalled bot is trying to go. The
derivation (in [`FollowPath::heading`](../src/actor/brain/low_level.rs), surfaced
through [`Brain::heading`](../src/actor/brain/mod.rs) and written in
[`black_bot_brain`](../src/actor/black_bot.rs)) is:

1. **moving** → the normalized velocity direction;
2. **stalled / braking** (velocity ≈ 0) → the maintained unit `direction` toward
   the next path node;
3. otherwise → `Vec2::ZERO`.

The write is **sticky**: a frame that yields no direction (idle, waiting, braked
to rest) keeps the last heading, so a paused bot still faces the way it last
moved.

Relative-motion helpers on `ActorState` (read another bot's state and compare):

| Method | Meaning |
|--------|---------|
| `is_moving()` | has a known direction (`heading != 0`) |
| `heading_angle() -> Option<f32>` | facing as `atan2(y, x)` radians (the future yaw) |
| `is_moving_toward(point)` | direction points toward a world point |
| `is_moving_toward_actor(other)` | closing on the other bot's position |
| `heading_alignment(other) -> f32` | `heading · other.heading` in `-1..=1` |
| `is_moving_forward_relative_to(other)` | same general direction (alignment `> 0`) — convoy/following |
| `is_moving_opposite_to(other)` | opposite general direction (alignment `< 0`) |
| `is_head_on_with(other)` | **both** closing on each other — the case needing strongest avoidance |

`is_moving_*` is always `false` for a directionless bot, so callers never need to
guard the zero case.

## Charge

Every bot entity carries a [`Charge`](../src/actor/charge.rs) component — a
battery `level` in `[0.0, 1.0]` that drains over time. A depleted bot is
immobilized **in its think system** (zeroing `move_buffer`), not in
`process_actor_moves` — see [`charge.md`](charge.md) for the full system, including
why the gate must live in `think`, the discharge rate, spawn ranges, inspector
display, and persistence.

## Genetics

Every BlackBot carries a [`Genome`](../src/actor/genetics.rs) component that
fixes its **immutable, heritable movement traits** — `max_speed` and
`acceleration`. The model (`src/actor/genetics.rs`):

- **`Genes`** — 258 random unsigned bytes, the raw genome. Generated **once** at
  creation from a seed (`Genes::from_seed`) via the seeded [`StdRng`] (never
  `thread_rng`, so a reloaded bot reproduces an identical genome) and never
  mutated afterward.
- **`GeneticTraits`** — the decoded phenotype (`max_speed`, `acceleration`,
  `battery_quality`), computed once and cached. Runtime code reads only these;
  the raw genes stay accessible via [`Genome::genes`] but are otherwise dormant
  after creation.

**Genome → trait mapping** is pure and deterministic. Each trait derives a single
**seed byte** from the genome through its own formula, then feeds that byte (mixed
with a per-trait salt so colliding bytes still decorrelate) into `normal_sample`
— a Box–Muller draw from a freshly seeded `StdRng` — to produce a normally
distributed value, clamped to a sane range. The canonical formula (speed):

1. `byte(143) ^ 123`,
2. `X = byte(42) % 128` selects which byte to mix in,
3. XOR step 1 with `byte(X)`.

Acceleration and battery quality use distinct formulas in the same spirit. Add a
new trait by adding a `GeneticTraits` field, a `*_seed` formula, and its
distribution constants.

**Battery quality** (`battery_quality`, in `(0, 1]`) is a special **upper-clamped
half-normal**: a `N(1.0, spread)` draw clamped to `[min, 1.0]`. The whole upper
half (samples ≥ 1.0) collapses onto exactly `1.0`, so a perfect battery is the
single most popular variant — quality *cannot exceed* 100% but can fall below it.
It feeds [`GeneticTraits::discharge_multiplier`](../src/actor/genetics.rs) =
`BEST_BATTERY_DRAIN_MULT / battery_quality`, which
[`discharge_actors`](../src/actor/charge.rs) multiplies the baseline drain by: a
perfect battery drains at `BEST_BATTERY_DRAIN_MULT` (`0.5` — *below* baseline, so
the best batteries last ~twice as long), and a worse one scales up from there,
draining proportionally faster. See [`charge.md`](charge.md).

**Wiring.** `build_brain_with_genome` (in `black_bot.rs`) rolls the genome from
the bot's brain seed and overwrites the default
[`FollowTuning`](../src/actor/brain/low_level.rs) `max_speed` / `accel` with the
genome's traits, so bot movement speed and acceleration are genetic. The genome
is attached as a component at spawn; the inspector **Status** tab shows
`gene_max_speed` / `gene_acceleration` / `gene_battery_quality`. Genes are not
serialized — they are re-derived deterministically from the persisted brain
`rng_seed` on load.

## Selection and inspection

Left-clicking a bot's mesh sets the [`SelectedActor`](../src/hud/actor_inspector.rs)
resource (mesh picking) and opens a **non-blocking, right-docked properties
panel** ([`ActorInspectorPlugin`](../src/hud/actor_inspector.rs)). The panel
live-refreshes its rows **twice a second** while the same bot stays selected; the
rest of the screen stays interactive (the camera keeps panning/zooming, except
mouse-wheel over the panel scrolls the panel). Closing the panel (its **X** or
**Escape**) or clicking a different bot updates the selection; killing/deleting
the selected bot clears it.

While a bot is selected, [`SelectionOverlayPlugin`](../src/actor/selection_overlay.rs)
draws two world-space cues for that bot only:

- a small **glowing green cube** hovering above it (a bobbing, spinning emissive
  marker), and
- its remaining route as **waypoint gizmos** — a polyline from the bot through
  each upcoming simplified-path waypoint tile center, with a node marker at each.
  This is independent of the global path overlay
  ([`paint_black_bot_targets`](../src/actor/black_bot.rs), gated on
  `PathOverlayEnabled`): the selection waypoints are always shown for the
  selection regardless of that toggle.

## Movement and collision

Collision is split into a parallel **proposal** and a sequential **arbitration**.
`docs/movement.md` is the deep, end-to-end reference for this pipeline; the
sections below cover what the actor runtime itself contributes.

### Proposal — `Actor::propose_move`

```rust,ignore
fn propose_move(&mut self, static_cache: &Hypermap<SubtilePassability>);
```

`propose_move` validates this frame's step against **static** geometry only —
never the dynamic occupancy. It reads the persistent `static_cache` (subtile flag
grid, updated only on map edits) through
[`first_static_block`](../src/map/passability.rs), which walks the candidate
footprint and returns the first cell whose flags intersect the actor's
`blocked_flags()` (e.g. `FLAG_BLOCKED | FLAG_VOID` for a ground walker,
`FLAG_BLOCKED` for a flier). Self-overlap with the previous footprint is bypassed.

The result is written into the actor's [`ActorShadow`] (`src/actor/movement.rs`):
`proposed_center`, `proposed_delta`/`proposed_rotation`, `static_block`, and the
absolute footprint cells in `current` (and the previous footprint in `previous`).
The default cancels the whole step if any cell is statically blocked; `BlackBot`
overrides `propose_move` with an **axis-decomposed** static slide so a grazing
wall keeps the free axis and snaps the blocked one to the wall edge.

The proposal touches only the read-only static cache, so it has no shared
writes. It runs **sequentially** on the main thread — the per-bot work is a few
microseconds for the whole crowd, and keeping it off Bevy's global
`ComputeTaskPool` avoids the work-stealing coupling that made the old
`par_iter_mut` version absorb unrelated compute work (see `docs/movement.md`
and `OPTIMIZATION.md`).

### Resolve — owner grid

The resolve stage (entity-sorted for determinism) uses a reused per-frame
**owner grid** (a flat lock-free `HashMap<IVec2, u32>` from world-subtile to
actor slot index — the pass is sequential, so no chunked/locked structure is
needed). Footprints travel as compact `(center, radius)` circles end-to-end. The
pure resolver (`arbitrate`) first **pre-stamps every actor's currently-occupied
footprint**, then for each actor in order:

- it releases its own origin (so a small overlapping step never self-blocks);
- if its proposed cells are all free, it takes them;
- if a cell is owned by another actor, it **holds at its origin** and is marked
  collided (`BlockedByOccupancy`).

Pre-stamping makes the occupant of a cell unbeatable: a mover can never claim a
cell another actor still holds, and every actor always has its own origin to fall
back to. So occupancy is authoritative **within** the frame — two actors can
never overlap — **and no jam ever needs a teleport**: there is no back-off
cascade, no depth cap, and no squeeze pool (cf. the old two-system design). A
follower processed before its leader holds for one frame (the *train ripple* in
`docs/movement.md`), never overlapping.

Accepted footprints are then stamped into the dynamic **write** buffer
(`commit_footprint`) so the brain's avoidance views and the async pathfinder read
identical occupancy after the next flush.

### Apply + re-entry

In the same sequential system: placed actors advance (`center += proposed_delta`,
`last_accepted_center_subtile = proposed_center`, rotation applied); collided
actors hold position and surface `last_movement_error` for the brain to react to
next frame; off-screen re-entrants are teleported to a free cell by
`resolve_offscreen_collision` (the only non-local move) and flagged
`shadow.teleported` so the brain re-routes from the landing spot.

### Grid position vs float center

`center` (float) and `last_accepted_center_subtile` (integer) are **two separate coordinate tracks** that must not be mixed:

- `center` drifts smoothly via `tile_delta` every frame — it exists purely for rendering.
- `last_accepted_center_subtile` advances only by accepted `subtile_shift` steps — it is the source of truth for the collision grid.

`propose_move` derives the candidate grid position from `last_accepted_center_subtile + subtile_shift`, **never** from `center_subtile_i32()` (except on the very first frame, when there is no accepted position yet). Without this invariant, the float center can drift past a subtile boundary and `.round()` into a wall cell, causing every subsequent move (including zero-shift) to fail permanently.

### Allocation-free hot path

The actor pipeline performs **zero heap allocations per actor per frame** on the steady-state path:

- previous occupancy is `(IVec2, i32)`, copied by value — no `Vec` clone;
- self-overlap test is an `O(1)` bitmap lookup against the baked `CircleShadow` — no `HashSet` construction;
- the new footprint is stamped directly into the write buffer as the candidate loop iterates — no `Vec<IVec2>` materialized then re-iterated.

`CircleShadow` instances are baked once per radius into a lock-free `OnceLock`
slot table (indexed by radius) and leaked to `&'static`, so every warm lookup is
a single atomic load with no contention. Only pathological radii (≥ the table
length) fall back to a locked map.

### Lock-free, chunk-local collision

The collision core has **no process-global lock on the hot path**:

- Shadow lookups are the lock-free `OnceLock` table above.
- Footprint reads/writes are *chunk-local*: a compact footprint's subtiles
  almost always share one hypermap chunk, so a per-call cursor
  (`SubtileReadCursor` / `SubtileWriteCursor` in `passability.rs`) resolves that
  chunk — the global chunk-table lock plus `Arc` clone — at most once per
  distinct chunk instead of once per subtile, and reads each per-tile
  `SubtilePassability` by reference (no 200-byte clone). The only locks taken are
  fine-grained per-chunk `RwLock`s, acquired exactly when a cell is touched.

### Sequential proposal, sequential arbitration

The **propose** phase iterates actors on the main thread: each mutates only its
own state and reads only the immutable static cache. It is deliberately *not*
`par_iter_mut` — the per-bot work is a few microseconds for the whole crowd, and
Bevy's global `ComputeTaskPool` would tick the global executor while waiting,
absorbing unrelated compute work into the propose tick (see `OPTIMIZATION.md`).
The **arbitrate** phase is single-threaded by design — it is the authority that
serializes occupancy — and processes actors in **entity-sorted** order. The
sequential pass touches each visible actor's footprint once over the lock-free
owner grid, which is why it replaced the old contended parallel footprint
OR-writes (see `OPTIMIZATION.md`).

### Off-screen culling and re-entry

An actor whose containing chunk has no spawned mesh entity
(`HypermapRuntime::is_world_pos_rendered` is `false` — i.e. it is far from the
camera) is **not** collision-checked. It carries the `OffScreenActor` marker and
moves via `Actor::advance_unchecked`: position advances with no footprint stamp,
so off-screen actors neither collide nor occupy the dynamic map. This keeps the
collision cost proportional to the *visible* actor set, not the whole world.

On the single frame an actor crosses **off-screen → on-screen**, it is placed
back into a free cell by `resolve_offscreen_collision` (current cell →
`next_waypoint_hint` → expanding tile ring r=1..5). This runs as the last stage
of `process_actor_moves`, after every on-screen actor's footprint has been
stamped into the **write** buffer, over only the actors that re-entered this
frame (collected during the think + propose stage, sorted by entity). Each
placement uses `DynamicPassabilityMap::try_claim_reentry_footprint`, which also
probes the **write** buffer and commits its claim there — so a re-entrant avoids
both the on-screen actors' new footprints and earlier re-entrants' just-claimed
cells. Re-entry is the **only** teleport left in the system (the old jam-recovery
squeeze pool is gone — see `docs/movement.md` § *Why no teleport*).

### Per-actor static passability

Each actor declares a bitmask of `SubtilePassability` flags it cannot enter via:

```rust,ignore
trait Actor {
    /// Default: ground-walker — blocks FLAG_BLOCKED | FLAG_VOID.
    fn blocked_flags(&self) -> u64 {
        FLAG_BLOCKED | FLAG_VOID
    }
}
```

`first_static_block(static_cache, center, radius, blocked, previous)` in
[`passability.rs`](../src/map/passability.rs) uses this mask to find the first
candidate subtile that contains any of the actor's `blocked_flags`. Called in
`propose_move` (Step 1), not during arbitration.

| Actor class | `blocked_flags` override | Crosses void? |
|---|---|---|
| Ground walker (default) | `FLAG_BLOCKED \| FLAG_VOID` | No |
| Flier (future) | `FLAG_BLOCKED` only | Yes |
| Phasing creature (future) | `0` | Yes (ignores all) |

### `ActorMovementError`

`last_movement_error` is one of:

- `BlockedByOccupancy { world_subtile_x, world_subtile_y }` — another actor's footprint blocks the way.
- `BlockedByStatic { world_subtile_x, world_subtile_y }` — the actor's own static rule rejected the cell.
- `InvalidRadius(i32)` — programmer error: negative radius.

## Example: minimal actor type

```rust,ignore
use bevy::prelude::*;
use everly::actor::{Actor, ActorMoveBuffer, ActorState};
use everly::map::passability::{ActorFootprint, SUBTILE_COUNT};

#[derive(Debug)]
struct Walker {
    state: ActorState,
    direction: Vec2,           // continuous heading (unit vector)
    accumulator: Vec2,         // fractional subtile displacement carried across frames
}

const SPEED_SUBTILES_PER_S: f32 = 6.0;

impl Walker {
    fn new(center: Vec2, radius_subtiles: i32) -> Self {
        Self {
            state: ActorState {
                center,
                radius_subtiles,
                rotation: 0.0,
                heading: Vec2::ZERO,
                move_buffer: ActorMoveBuffer::default(),
                last_movement_error: None,
                last_accepted_center_subtile: None,
                last_accepted_radius_subtiles: radius_subtiles,
                next_waypoint_hint: None,
                field_main_tile: None,
                dirtiness: 0.0,
                shadow: ActorShadow::default(),
            },
            direction: Vec2::new(1.0, 0.0),
            accumulator: Vec2::ZERO,
        }
    }
}

impl Actor for Walker {
    fn state(&self) -> &ActorState { &self.state }
    fn state_mut(&mut self) -> &mut ActorState { &mut self.state }

    fn think_low_level(&mut self) {
        if self.state.last_movement_error.is_some() {
            self.direction = -self.direction;
            self.accumulator = Vec2::ZERO;
        }
    }

    // Ground walker — inherits the default `blocked_flags` (FLAG_BLOCKED | FLAG_VOID).
    // No override needed.
}

// A flying variant: crosses voids freely, blocks only wall edge strips.
struct Flier { state: ActorState }
impl Actor for Flier {
    fn state(&self) -> &ActorState { &self.state }
    fn state_mut(&mut self) -> &mut ActorState { &mut self.state }

    fn blocked_flags(&self) -> u64 {
        FLAG_BLOCKED // no FLAG_VOID, so void tiles are passable
    }
}

// In a Bevy system (runs before `process_actor_moves`):
fn walker_think(time: Res<Time>, mut q: Query<(&mut ActorObject, &mut WalkerVisual)>) {
    let dt = time.delta_secs();
    let subtile_to_tile = 1.0 / SUBTILE_COUNT as f32;

    for (mut obj, mut vis) in &mut q {
        let delta_subtiles = vis.direction * SPEED_SUBTILES_PER_S * dt;

        let state = obj.inner.state_mut();
        // Exact float displacement — applied to center every frame (smooth).
        state.move_buffer.tile_delta = delta_subtiles * subtile_to_tile;

        // Integer subtile steps — only when a boundary is crossed (collision grid).
        vis.accumulator += delta_subtiles;
        let step = vis.accumulator.trunc();
        vis.accumulator -= step;
        state.move_buffer.subtile_shift = IVec2::new(step.x as i32, step.y as i32);
    }
}
```

## Example: spawning an actor entity

```rust,ignore
use bevy::prelude::*;
use everly::actor::ActorObject;

fn spawn_actor(mut commands: Commands) {
    let walker = Walker::new(Vec2::new(10.0, 10.0), 2);
    commands.spawn((
        Name::new("Walker"),
        ActorObject::new(Box::new(walker)),
    ));
}
```

## New actor checklist

Use this when introducing a new actor class:

- [ ] Define a concrete actor struct with `state: ActorState`.
- [ ] Initialize `ActorState` with:
  - [ ] `center` in tile-space (`Vec2`)
  - [ ] `radius_subtiles`
  - [ ] `rotation`
  - [ ] `heading: Vec2::ZERO` (published each frame by the think system; see [Movement direction](#movement-direction-heading))
  - [ ] `move_buffer: ActorMoveBuffer::default()`
  - [ ] `last_movement_error: None`
  - [ ] `last_accepted_center_subtile: None`
  - [ ] `last_accepted_radius_subtiles: <same as radius_subtiles>`
  - [ ] `next_waypoint_hint: None` (set each frame in think if the actor pathfinds)
  - [ ] `field_main_tile: None`
  - [ ] `dirtiness: 0.0`
  - [ ] `shadow: ActorShadow::default()`
- [ ] Implement `Actor`:
  - [ ] `state()` and `state_mut()`
  - [ ] `think_low_level()` if needed
  - [ ] `prepare_movement()` to fill `move_buffer`
  - [ ] **Decide static traversal rules** and override `blocked_flags()` if the actor is not a plain ground walker (fliers, swimmers, phasers, wall-runners…).
  - [ ] Override `propose_move()` if axis-decomposed wall-sliding or a special footprint shape is needed (default: axis-combined, no slide).
- [ ] Do not mutate `center` directly in gameplay systems; `process_actor_moves` applies accepted motion.
- [ ] Write both channels every frame:
  - [ ] `tile_delta` — exact float displacement in tile-space (`direction * speed * dt / SUBTILE_COUNT`)
  - [ ] `subtile_shift` — integer steps from an accumulator (only non-zero on subtile boundary crossings)
- [ ] Remember `1 tile = 5 subtiles`.
- [ ] Spawn the actor as `ActorObject::new(Box::new(...))` and add a `Name`.
- [ ] If the actor has a brain plugin, wire it `.before(process_actor_moves)` and `.after(process_actor_moves)`.
- [ ] Add/adjust unit tests for:
  - [ ] successful `propose_move` (`shadow.proposed_center` / `shadow.origin` match the expected centers)
  - [ ] dynamic-occupancy blocked path (`BlockedByOccupancy` after arbitration)
  - [ ] static-geometry blocked path (`BlockedByStatic`) — at least one test confirming the actor's `blocked_flags`
- [ ] Run:
  - [ ] `cargo check`
  - [ ] `cargo test -p everly -- actor`
  - [ ] `cargo test -p everly -- passability`

## Notes and extension points

- Keep `think_low_level` cheap and deterministic; heavy planning belongs in a separate async layer.
- If different actor classes need custom wall-sliding or a non-circular footprint, override `propose_move`.
- `radius_subtiles` controls occupancy shape size. Larger actors naturally claim multiple tiles.
- `last_movement_error` is frame-local: inspect it during `think_low_level` and treat it as ephemeral state.
- `OccupancyArbiter` is generic — no per-class changes needed when adding a new actor type.
