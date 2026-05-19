# Actor Runtime

This document explains the low-level actor subsystem in `src/actor/mod.rs`.

## Overview

The actor subsystem provides:

- A generic `Actor` trait for per-frame behavior.
- `ActorState` as common mutable runtime data.
- A single processing system (`ActorPlugin`) that runs actor logic each in-game frame.
- Movement and occupancy integration through `DynamicPassabilityMap`.

The low-level pipeline is deterministic and synchronous. High-level planning is expected to run separately (for example, in async systems), then feed intent into `move_buffer`.

## Per-frame lifecycle

For each `ActorObject`, the actor system runs:

1. Clear `last_movement_error`.
2. `think_low_level()`
3. `prepare_movement()`
4. `try_move(passability)`
5. After all actors, flush passability write buffer into read buffer.
6. Field interactions (e.g. dirt) — after movement; see `docs/field-interactions.md`.

This means movement for frame `N` writes occupancy into passability write buffer, and that occupancy becomes visible from read buffer after flush at end of frame.

## Actor data model

`ActorState` stores:

- `center: Vec2` — actor center in tile-space floats. Updated by the exact float `tile_delta` every frame — never quantized.
- `radius_subtiles: i32` — circular occupancy radius in subtiles.
- `rotation: f32` — actor orientation.
- `move_buffer: ActorMoveBuffer` — relative motion for this frame (two displacement channels):
  - `tile_delta: Vec2` — exact float displacement in tile-space, applied to `center` every frame for smooth rendering.
  - `subtile_shift: IVec2` — integer subtile steps for the passability collision grid; typically `(0, 0)` on most frames, non-zero only when accumulated float motion crosses a subtile boundary.
  - `rotation_shift: f32`
- `last_movement_error: Option<ActorMovementError>` — cleared every low-level step.
- `last_accepted_center_subtile: Option<IVec2>` — integer subtile center of the last accepted occupancy update; `None` for a brand-new actor.
- `last_accepted_radius_subtiles: i32` — radius (in subtiles) of that last accepted footprint; tracked separately from `radius_subtiles` so an actor that resizes still re-stamps the correct old circle on rejection.
- `field_main_tile: Option<IVec2>` — last observed **main tile** for hypermap field coupling (dirt, etc.); see [Main tile](#main-tile) and `docs/field-interactions.md`.

> **Previous-footprint encoding.** The previous frame's occupied cells are described compactly by the `(last_accepted_center_subtile, last_accepted_radius_subtiles)` pair and the baked `CircleShadow` for that radius — *not* by storing a `Vec<IVec2>`. This keeps the per-actor hot path allocation-free; self-overlap is an `O(1)` bitmap test against the previous shadow.

### Dual-channel movement

Movement uses two parallel channels to separate smooth rendering from discrete collision:

| Channel | Type | Updated | Purpose |
|---|---|---|---|
| `tile_delta` | `Vec2` | every frame | exact float center displacement — makes the rendered position perfectly smooth |
| `subtile_shift` | `IVec2` | sparse | integer subtile steps for the passability grid — only non-zero when a subtile boundary is crossed |

A typical continuous-movement actor (like `GlitchBot`) computes velocity in subtiles/s, converts to tile-space for `tile_delta`, and accumulates fractional subtile displacement across frames. When the accumulator's integer part becomes non-zero, that integer is emitted as `subtile_shift` and the remainder is carried forward.

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
| `actor_main_tile(center)` | **round** | Field interactions (`field_main_tile`), [`BlackBot`](../src/actor/black_bot.rs) path think (`BlackBotVisual.main_tile`) |
| `center_subtile_i32()` | **floor**(`center × 5`) | Passability grid, footprints, collision (first frame only) |
| `center_tile_i32()` | **floor**(`center`) | Legacy helper; prefer `actor_main_tile` for tile identity |

Do **not** use `floor(center)` for main-tile or field logic — an actor spawned at tile center `(0.5, 0.5)` would be assigned the wrong tile. Subtile collision intentionally keeps `floor` so the footprint stays inside the subtile that contains the float position.

After [`process_actors`](../src/actor/mod.rs), [`dirt_actor_interaction`](../src/map/field_interactions.rs) updates `field_main_tile` and applies field rules to the tile the actor **left** when main tile changes. See `docs/field-interactions.md`.

## Movement and collision

`Actor::try_move()` is the gate that validates a frame's intended movement against **both** dynamic and static passability. Its signature is:

```rust,ignore
fn try_move(
    &mut self,
    dynamic_passability: &DynamicPassabilityMap,
    static_world: &StaticWorld,
);
```

`StaticWorld` bundles read-only views of the world's static layers an actor may need to consult:

```rust,ignore
pub struct StaticWorld<'a> {
    pub passability: &'a Hypermap<f32>,    // > 0.0 = walkable (ground)
    pub cell_types: &'a Hypermap<CellType>, // raw geometry (Void / Road / Wall / Corner)
}
```

Both maps are exposed because `passability` collapses `Void` and `Wall(_)` to the same `0.0` value — fine for ground walkers but insufficient for classes that need to distinguish the two (a flier wants to traverse `Void` but not `Wall`). Use `StaticWorld::cell_at_subtile(world_subtile)` to read the raw `CellType` and `StaticWorld::passability_at_subtile(world_subtile)` for the scalar walkability.

Internally it delegates to:

```rust,ignore
DynamicPassabilityMap::try_update_footprint_with_static(
    next_center_subtile: IVec2,
    radius_subtiles: i32,
    previous: Option<(IVec2, i32)>,  // (previous_center_subtile, previous_radius_subtiles)
    is_static_passable: impl Fn(IVec2) -> bool,
) -> Result<(), TryUpdateFootprintError>
```

That method:

- builds the actor's circular footprint at `next_center_subtile` from the cached `CircleShadow`,
- for each candidate subtile, asks the **per-actor** static predicate `is_static_passable(world_subtile)` — rejects with `BlockedByStatic` if it returns `false`,
- then checks the dynamic **read** buffer — rejects with `BlockedByOccupancy` if blocked,
- bypasses both checks for any cell inside the previous circle (`O(1)` bitmap test via `previous_shadow.contains_offset(target - previous_center)`),
- on success, stamps the accepted circle into the **write** buffer,
- on failure, re-stamps the previous circle so occupancy persists next frame.

On success, `try_move` advances `last_accepted_center_subtile` / `last_accepted_radius_subtiles` and then `move_actor()` applies `tile_delta` to `center` (exact float, never quantized) and adds `rotation_shift` to `rotation`.

### Grid position vs float center

`center` (float) and `last_accepted_center_subtile` (integer) are **two separate coordinate tracks** that must not be mixed:

- `center` drifts smoothly via `tile_delta` every frame — it exists purely for rendering.
- `last_accepted_center_subtile` advances only by accepted `subtile_shift` steps — it is the source of truth for the collision grid.

`try_move` derives the candidate grid position from `last_accepted_center_subtile + subtile_shift`, **never** from `center_subtile_i32()` (except on the very first frame, when there is no accepted position yet). Without this invariant, the float center can drift past a subtile boundary and `.round()` into a wall cell, causing every subsequent move (including zero-shift) to fail permanently.

### Allocation-free hot path

The actor pipeline performs **zero heap allocations per actor per frame** on the steady-state path:

- previous occupancy is `(IVec2, i32)`, copied by value — no `Vec` clone;
- self-overlap test is an `O(1)` bitmap lookup against the baked `CircleShadow` — no `HashSet` construction;
- the new footprint is stamped directly into the write buffer as the candidate loop iterates — no `Vec<IVec2>` materialized then re-iterated.

`CircleShadow` instances are baked once per radius (lazy, behind a `Mutex<HashMap>`) and leaked to `&'static`, so every subsequent frame is a pure read.

### Per-actor static passability

The closure passed into `try_update_footprint_with_static` is **built per actor** from a trait method:

```rust,ignore
trait Actor {
    /// Default: ground-walker. A subtile is passable iff its containing
    /// world tile has static passability `> 0.0`.
    fn is_static_subtile_passable(
        &self,
        world_subtile: IVec2,
        world: &StaticWorld,
    ) -> bool {
        world.passability_at_subtile(world_subtile) > 0.0
    }
}
```

This is **critical**: different actor classes interpret the same world geometry differently and must override this method to declare their traversal rules.

| Actor class | `is_static_subtile_passable` override |
|---|---|
| Ground walker (default) | tile passability `> 0.0` |
| Flier (e.g. `GlitchBot`) | **subtile-aware**: crosses `Void`; blocks only wall edge strips and corner-pillar subtiles |
| Swimmer | tile passability `> 0.0` **or** the cell is water |
| Phasing creature | always `true` — ignore everything |
| Wall-only mover | `cell == Wall(..)` |

The `IVec2` argument is a **world-subtile** coordinate (not a tile); use `StaticWorld::subtile_to_tile`, `cell_at_subtile`, or `passability_at_subtile` to access the world data.

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
                move_buffer: ActorMoveBuffer::default(),
                last_movement_error: None,
                last_accepted_center_subtile: None,
                last_accepted_radius_subtiles: radius_subtiles,
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

    // Ground walker — inherits the default `is_static_subtile_passable`
    // (passable iff tile static passability > 0.0). No override needed.
}

// A flying variant: crosses voids freely but still respects wall geometry
// at subtile precision (wall edge strips + single corner-pillar subtile).
struct Flier { state: ActorState }
impl Actor for Flier {
    fn state(&self) -> &ActorState { &self.state }
    fn state_mut(&mut self) -> &mut ActorState { &mut self.state }

    fn is_static_subtile_passable(
        &self,
        world_subtile: IVec2,
        world: &StaticWorld,
    ) -> bool {
        // Example policy sketch:
        // 1) Read containing tile CellType.
        // 2) Convert world_subtile -> local (x,y) in 0..5.
        // 3) For Wall(mask), block only the matching edge strip(s).
        // 4) For Corner(c), block only the corresponding corner subtile.
        // 5) Void/Road pass.
        true // placeholder for example brevity
    }
}

// In a Bevy system (runs before `process_actors`):
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
  - [ ] `move_buffer: ActorMoveBuffer::default()`
  - [ ] `last_movement_error: None`
  - [ ] `last_accepted_center_subtile: None`
  - [ ] `last_accepted_radius_subtiles: <same as radius_subtiles>`
- [ ] Implement `Actor`:
  - [ ] `state()` and `state_mut()`
  - [ ] `think_low_level()` if needed
  - [ ] `prepare_movement()` to fill `move_buffer`
  - [ ] **Decide static traversal rules** and override `is_static_subtile_passable` if the actor is not a plain ground walker (fliers, swimmers, phasers, wall-runners…).
- [ ] Do not mutate `center` directly in gameplay systems; let `try_move` / `move_actor` apply accepted motion.
- [ ] Write both channels every frame:
  - [ ] `tile_delta` — exact float displacement in tile-space (`direction * speed * dt / SUBTILE_COUNT`)
  - [ ] `subtile_shift` — integer steps from an accumulator (only non-zero on subtile boundary crossings)
- [ ] Remember `1 tile = 5 subtiles`.
- [ ] Spawn the actor as `ActorObject::new(Box::new(...))` and add a `Name`.
- [ ] Add/adjust unit tests for:
  - [ ] successful movement
  - [ ] dynamic-occupancy blocked path (`BlockedByOccupancy`)
  - [ ] static-geometry blocked path (`BlockedByStatic`) — at least one test confirming the actor's traversal rules
  - [ ] footprint persistence expectations (self-overlap should be ignored by passability updates)
- [ ] Run:
  - [ ] `cargo check`
  - [ ] `cargo test -p everly -- actor`
  - [ ] `cargo test -p everly -- passability`

## Notes and extension points

- Keep `think_low_level` cheap and deterministic; heavy planning belongs in a separate async layer.
- If different actor classes need custom low-level movement validation, they can override `try_move`.
- `radius_subtiles` controls occupancy shape size. Larger actors naturally claim multiple tiles.
- `last_movement_error` is frame-local: inspect it during `think_low_level` and treat it as ephemeral state.
