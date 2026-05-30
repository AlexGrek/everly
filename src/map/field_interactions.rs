//! Actor ↔ hypermap field coupling (dirt today; temperature and others later).
//!
//! Runs **after** [`crate::actor::process_actors`] so [`ActorState::center`] reflects
//! the movement step. See `docs/field-interactions.md`.

use bevy::prelude::*;

use crate::actor::{actor_main_tile, process_actors, ActorObject};
use crate::map::dirt::{DirtMap, DIRT_TRACK_DEPOSIT};
use crate::map::hypermap::Hypermap;
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::world_map::CellType;
use crate::menu::main_menu::GameState;

/// A tile the actor **left** this frame (previous main tile ≠ current).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainTileTransition {
    pub left_tile: IVec2,
}

/// Updates `stored` from `center`. Returns [`MainTileTransition`] when the actor
/// crossed into a new main tile (requires a prior stored tile).
#[inline]
pub fn main_tile_transition(
    stored: &mut Option<IVec2>,
    center: Vec2,
) -> Option<MainTileTransition> {
    let current = actor_main_tile(center);
    let transition = stored.and_then(|prev| {
        if prev != current {
            Some(MainTileTransition { left_tile: prev })
        } else {
            None
        }
    });
    *stored = Some(current);
    transition
}

/// Collects main-tile transitions for every actor after a movement step.
pub fn collect_main_tile_transitions(actors: &mut Query<&mut ActorObject>) -> Vec<MainTileTransition> {
    let mut transitions = Vec::new();
    for mut actor_obj in actors.iter_mut() {
        let center = actor_obj.inner.state().center;
        if let Some(t) =
            main_tile_transition(&mut actor_obj.inner.state_mut().field_main_tile, center)
        {
            transitions.push(t);
        }
    }
    transitions
}

/// Pure dirt-exchange rule between a `floor` value and an `actor` dirtiness.
///
/// Returns `(new_actor_dirtiness, floor_delta)`:
/// - Floor cleaner than the actor: the actor wipes [`DIRT_TRACK_DEPOSIT`] (1%) of
///   itself onto the tile (capped so it never goes below `0.0`); `floor_delta`
///   equals what the actor lost (conserved).
/// - Floor dirtier than the actor: the actor picks up `1%` *of the floor's*
///   dirtiness (clamped to `1.0`); `floor_delta` is `0.0`.
/// - Equal: no change.
#[inline]
pub fn dirt_exchange(floor: f32, actor: f32) -> (f32, f32) {
    if floor < actor {
        let transfer = DIRT_TRACK_DEPOSIT.min(actor);
        (actor - transfer, transfer)
    } else if floor > actor {
        let gain = floor * DIRT_TRACK_DEPOSIT;
        ((actor + gain).min(1.0), 0.0)
    } else {
        (actor, 0.0)
    }
}

/// Applies [`dirt_exchange`] between an actor and the floor tile it just **left**.
///
/// Skips `Void` tiles. Floor writes go to the dirt **write** buffer;
/// `actor_dirtiness` is updated in place.
pub fn exchange_dirt_with_tile(
    dirt: &DirtMap,
    world: &Hypermap<CellType>,
    tile: IVec2,
    actor_dirtiness: &mut f32,
) {
    if matches!(world.get(tile.x, tile.y), CellType::Void) {
        return;
    }
    let floor = dirt.get_tile(tile.x, tile.y);
    let (new_actor, floor_delta) = dirt_exchange(floor, *actor_dirtiness);
    *actor_dirtiness = new_actor;
    if floor_delta != 0.0 {
        dirt.add_tile_dirt(tile.x, tile.y, floor_delta);
    }
}

pub struct FieldInteractionsPlugin;

impl Plugin for FieldInteractionsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            dirt_actor_interaction
                .after(process_actors)
                .before(crate::map::dirt::flush_dirt_map)
                .run_if(in_state(GameState::InGame))
                .run_if(not(crate::actor::is_paused)),
        );
    }
}

/// Exchanges dirt between each actor and the tile it left this frame. Actors on a
/// cleaner floor wipe dirt onto it; actors on a dirtier floor pick dirt up. No
/// floor writes happen for actors that did not change main tile.
pub(crate) fn dirt_actor_interaction(
    mut actors: Query<&mut ActorObject>,
    dirt: Res<DirtMap>,
    hypermap: Res<HypermapRuntime>,
) {
    let world = hypermap.map.as_ref();
    for mut actor_obj in actors.iter_mut() {
        let state = actor_obj.inner.state_mut();
        let center = state.center;
        let Some(t) = main_tile_transition(&mut state.field_main_tile, center) else {
            continue;
        };
        exchange_dirt_with_tile(&dirt, world, t.left_tile, &mut state.dirtiness);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_tile_transition_reports_left_tile() {
        let mut stored = Some(IVec2::new(1, 2));
        let t = main_tile_transition(&mut stored, Vec2::new(2.2, 2.1))
            .expect("should transition");
        assert_eq!(t.left_tile, IVec2::new(1, 2));
        assert_eq!(stored, Some(IVec2::new(2, 2)));
    }

    #[test]
    fn main_tile_transition_none_on_first_sample() {
        let mut stored = None;
        assert!(main_tile_transition(&mut stored, Vec2::new(0.2, 0.2)).is_none());
        assert_eq!(stored, Some(IVec2::new(0, 0)));
    }

    #[test]
    fn actor_main_tile_floors_to_containing_tile() {
        assert_eq!(actor_main_tile(Vec2::new(1.4, 1.5)), IVec2::new(1, 1));
        assert_eq!(actor_main_tile(Vec2::new(1.6, -0.1)), IVec2::new(1, -1));
    }

    #[test]
    fn dirt_exchange_actor_wipes_onto_cleaner_floor() {
        let (actor, floor_delta) = dirt_exchange(0.2, 0.5);
        assert!((actor - 0.49).abs() < 1e-6, "actor loses 1%");
        assert!((floor_delta - DIRT_TRACK_DEPOSIT).abs() < 1e-6, "floor gains what actor lost");
    }

    #[test]
    fn dirt_exchange_actor_picks_up_from_dirtier_floor() {
        let (actor, floor_delta) = dirt_exchange(0.8, 0.3);
        assert!((actor - (0.3 + 0.8 * DIRT_TRACK_DEPOSIT)).abs() < 1e-6, "actor gains 1% of floor");
        assert_eq!(floor_delta, 0.0, "floor unchanged when actor picks up");
    }

    #[test]
    fn dirt_exchange_equal_is_noop() {
        let (actor, floor_delta) = dirt_exchange(0.4, 0.4);
        assert_eq!(actor, 0.4);
        assert_eq!(floor_delta, 0.0);
    }

    #[test]
    fn dirt_exchange_clean_actor_stays_nonnegative() {
        // floor (0.0) < actor (0.0) is false, so a fully-clean actor on clean floor
        // never goes negative; even on a faintly dirty floor it only gains.
        let (actor, floor_delta) = dirt_exchange(0.0, 0.0);
        assert_eq!(actor, 0.0);
        assert_eq!(floor_delta, 0.0);
        // Tiny actor dirtiness over a clean floor: loss is capped at the actor value.
        let (actor, floor_delta) = dirt_exchange(0.0, 0.005);
        assert_eq!(actor, 0.0, "loss capped so actor never goes below zero");
        assert!((floor_delta - 0.005).abs() < 1e-6);
    }

    #[test]
    fn dirt_exchange_gain_clamps_to_one() {
        let (actor, _) = dirt_exchange(1.0, 0.999);
        assert!(actor <= 1.0);
    }
}
