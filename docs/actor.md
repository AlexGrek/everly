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

This means movement for frame `N` writes occupancy into passability write buffer, and that occupancy becomes visible from read buffer after flush at end of frame.

## Actor data model

`ActorState` stores:

- `center: Vec2` — actor center in tile-space floats.
- `radius_subtiles: i32` — circular occupancy radius in subtiles.
- `rotation: f32` — actor orientation.
- `move_buffer: ActorMoveBuffer` — relative motion for this frame:
  - `subtile_shift: IVec2`
  - `rotation_shift: f32`
- `last_movement_error: Option<ActorMovementError>` — cleared every low-level step.
- `footprint: ActorFootprint` — world-subtile cells occupied in previous frame; used to ignore self-collision overlap.

### Coordinate units

- `1 tile = 5 subtiles` (`SUBTILE_COUNT`).
- Motion and occupancy tests are performed in **integer subtiles**.
- `center` remains float tile coordinates for gameplay and rendering convenience.

## Movement and collision

`Actor::try_move()` delegates occupancy/collision logic to:

- `DynamicPassabilityMap::try_update_footprint(next_center_subtile, radius_subtiles, previous_footprint)`

That method:

- builds the actor's circular footprint,
- checks collisions against passability read buffer,
- ignores any collision cells that are part of `previous_footprint`,
- writes accepted footprint to write buffer and returns it,
- or returns an error and re-stamps old footprint.

On success, `move_actor()` updates center/rotation and stores the returned footprint.

## Example: minimal actor type

```rust,ignore
use bevy::prelude::*;
use everly::actor::{Actor, ActorMoveBuffer, ActorState};
use everly::map::passability::ActorFootprint;

#[derive(Debug)]
struct Walker {
    state: ActorState,
    desired_direction: IVec2,
}

impl Walker {
    fn new(center: Vec2, radius_subtiles: i32) -> Self {
        Self {
            state: ActorState {
                center,
                radius_subtiles,
                rotation: 0.0,
                move_buffer: ActorMoveBuffer::default(),
                last_movement_error: None,
                footprint: ActorFootprint::new(),
            },
            desired_direction: IVec2::new(1, 0),
        }
    }
}

impl Actor for Walker {
    fn state(&self) -> &ActorState {
        &self.state
    }

    fn state_mut(&mut self) -> &mut ActorState {
        &mut self.state
    }

    fn think_low_level(&mut self) {
        // Example: swap direction after a collision from previous frame.
        if self.state.last_movement_error.is_some() {
            self.desired_direction = -self.desired_direction;
        }
    }

    fn prepare_movement(&mut self) {
        // Move by one subtile per frame.
        self.state.move_buffer.subtile_shift = self.desired_direction;
        self.state.move_buffer.rotation_shift = 0.0;
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

## Notes and extension points

- Keep `think_low_level` cheap and deterministic; heavy planning belongs in a separate async layer.
- If different actor classes need custom low-level movement validation, they can override `try_move`.
- `radius_subtiles` controls occupancy shape size. Larger actors naturally claim multiple tiles.
- `last_movement_error` is frame-local: inspect it during `think_low_level` and treat it as ephemeral state.
