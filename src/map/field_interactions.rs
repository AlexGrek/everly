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

/// Deposits dirt on `tile` when it is not void. Writes the dirt **write** buffer.
pub fn deposit_dirt_on_tile(
    dirt: &DirtMap,
    world: &Hypermap<CellType>,
    tile: IVec2,
    delta: f32,
) {
    if matches!(world.get(tile.x, tile.y), CellType::Void) {
        return;
    }
    dirt.add_tile_dirt(tile.x, tile.y, delta);
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

/// Increases dirt on each tile actors left this frame. No-op when nobody changed tiles.
pub(crate) fn dirt_actor_interaction(
    mut actors: Query<&mut ActorObject>,
    dirt: Res<DirtMap>,
    hypermap: Res<HypermapRuntime>,
) {
    let transitions = collect_main_tile_transitions(&mut actors);
    if transitions.is_empty() {
        return;
    }

    let world = hypermap.map.as_ref();
    for t in transitions {
        deposit_dirt_on_tile(&dirt, world, t.left_tile, DIRT_TRACK_DEPOSIT);
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
    fn actor_main_tile_rounds_center() {
        assert_eq!(actor_main_tile(Vec2::new(1.4, 1.5)), IVec2::new(1, 2));
        assert_eq!(actor_main_tile(Vec2::new(1.6, -0.1)), IVec2::new(2, 0));
    }
}
