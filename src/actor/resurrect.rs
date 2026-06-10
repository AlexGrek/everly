//! Bulk actor recovery triggered from the actor-spawner palette.

use bevy::prelude::*;

use crate::actor::black_bot::{BlackBotVisual, Breakable};
use crate::actor::brain::Brain;
use crate::actor::charge::Charge;
use crate::actor::{ActorMoveBuffer, ActorObject};

/// Minimum charge level applied to every actor by [`resurrect_all_actors`].
pub const RESURRECT_MIN_CHARGE: f32 = 0.3;

/// Repairs every operational actor in the world: broken sub-components are fixed,
/// charge is raised to at least [`RESURRECT_MIN_CHARGE`], BlackBot brains are
/// reset, and movement intent is cleared.
pub fn resurrect_all_actors(
    charges: &mut Query<&mut Charge>,
    breakables: &mut Query<&mut Breakable>,
    brains: &mut Query<&mut Brain>,
    black_vis: &mut Query<&mut BlackBotVisual>,
    actors: &mut Query<&mut ActorObject>,
) {
    for mut charge in charges.iter_mut() {
        charge.level = charge.level.max(RESURRECT_MIN_CHARGE);
    }
    for mut breakable in breakables.iter_mut() {
        breakable.repair_all();
    }
    for mut brain in brains.iter_mut() {
        brain.reset();
    }
    for mut vis in black_vis.iter_mut() {
        vis.on_resurrect();
    }
    for mut obj in actors.iter_mut() {
        let state = obj.inner.state_mut();
        state.move_buffer = ActorMoveBuffer::default();
        state.next_waypoint_hint = None;
    }
}

/// Marker on the actor-spawner palette **Resurrect all** button.
#[derive(Component)]
pub struct ResurrectAllButton;

pub fn resurrect_all_button(
    interactions: Query<&Interaction, (With<ResurrectAllButton>, Changed<Interaction>)>,
    mut charges: Query<&mut Charge>,
    mut breakables: Query<&mut Breakable>,
    mut brains: Query<&mut Brain>,
    mut black_vis: Query<&mut BlackBotVisual>,
    mut actors: Query<&mut ActorObject>,
) {
    for interaction in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        resurrect_all_actors(
            &mut charges,
            &mut breakables,
            &mut brains,
            &mut black_vis,
            &mut actors,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::black_bot::BreakablePartState;

    #[test]
    fn repair_all_clears_broken_flags() {
        let mut b = Breakable {
            movement_engine: BreakablePartState { wear: 2.0, broken: true },
            control_plane: BreakablePartState { wear: 1.0, broken: true },
            sensory_system: BreakablePartState { wear: 0.5, broken: false },
        };
        b.repair_all();
        assert!(!b.movement_engine.broken);
        assert!(!b.control_plane.broken);
        assert!(!b.sensory_system.broken);
        assert_eq!(b.movement_engine.wear, 2.0, "wear is preserved");
    }

    #[test]
    fn min_charge_clamps_up_not_down() {
        assert_eq!(Charge::new(0.0).level.max(RESURRECT_MIN_CHARGE), RESURRECT_MIN_CHARGE);
        assert_eq!(Charge::new(0.8).level.max(RESURRECT_MIN_CHARGE), 0.8);
    }
}
