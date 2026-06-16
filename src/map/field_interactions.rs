//! Actor ↔ hypermap field coupling (dirt deposits; bot occupancy heating).
//!
//! Runs **after** [`crate::actor::process_actors`] so [`ActorState::center`] reflects
//! the movement step. See `docs/field-interactions.md`.

use std::collections::HashSet;

use bevy::prelude::*;

use crate::actor::{actor_main_tile, process_actor_moves, ActorObject};
use crate::map::dirt::{DirtMap, DIRT_TRACK_DEPOSIT};
use crate::map::hypermap::Hypermap;
use crate::map::hypermap_world::HypermapRuntime;
use crate::map::temperature::{
    TemperatureMap, BOT_OCCUPANCY_HEAT_DELTA_C, BOT_OCCUPANCY_HEAT_INTERVAL_S,
};
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

/// Repeating timer for [`bot_occupancy_heat`].
#[derive(Resource)]
struct BotOccupancyHeatTimer(Timer);

impl Default for BotOccupancyHeatTimer {
    fn default() -> Self {
        Self(Timer::from_seconds(
            BOT_OCCUPANCY_HEAT_INTERVAL_S,
            TimerMode::Repeating,
        ))
    }
}

pub struct FieldInteractionsPlugin;

impl Plugin for FieldInteractionsPlugin {
    fn build(&self, app: &mut App) {
        // Deposits tick with the fixed 60 Hz movement pipeline, right after
        // arbitration. The field flushes (`flush_dirt_map` /
        // `flush_temperature_map`) stay in `Update`, which always runs after
        // the frame's fixed ticks, so a deposit is still flushed the same
        // render frame — no cross-schedule ordering needed.
        app.init_resource::<BotOccupancyHeatTimer>()
            .add_systems(
                FixedUpdate,
                (
                    dirt_actor_interaction.after(process_actor_moves),
                    bot_occupancy_heat.after(process_actor_moves),
                )
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

/// Unique main tiles currently occupied by the given actor centers (one tile per bot).
pub fn collect_bot_occupied_tiles(centers: impl IntoIterator<Item = Vec2>) -> HashSet<IVec2> {
    centers
        .into_iter()
        .map(actor_main_tile)
        .collect()
}

/// Adds [`BOT_OCCUPANCY_HEAT_DELTA_C`] to each non-void main tile occupied by a bot.
pub fn apply_bot_occupancy_heat_to_tiles(
    temperature: &TemperatureMap,
    world: &Hypermap<CellType>,
    tiles: impl IntoIterator<Item = IVec2>,
) {
    for tile in tiles {
        if matches!(world.get(tile.x, tile.y), CellType::Void) {
            continue;
        }
        temperature.add_tile_c(tile.x, tile.y, BOT_OCCUPANCY_HEAT_DELTA_C);
    }
}

/// Every [`BOT_OCCUPANCY_HEAT_INTERVAL_S`], heat each main tile that currently holds a bot.
fn bot_occupancy_heat(
    time: Res<Time>,
    mut timer: ResMut<BotOccupancyHeatTimer>,
    actors: Query<&ActorObject>,
    temperature: Res<TemperatureMap>,
    hypermap: Res<HypermapRuntime>,
) {
    timer.0.tick(time.delta());
    if !timer.0.just_finished() {
        return;
    }

    let tiles = collect_bot_occupied_tiles(actors.iter().map(|a| a.inner.state().center));
    apply_bot_occupancy_heat_to_tiles(&temperature, hypermap.map.as_ref(), tiles);
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

    #[test]
    fn collect_bot_occupied_tiles_dedupes_shared_tile() {
        let tiles = collect_bot_occupied_tiles([
            Vec2::new(1.2, 2.8),
            Vec2::new(1.6, 2.1),
            Vec2::new(5.0, 5.0),
        ]);
        assert_eq!(tiles.len(), 2);
        assert!(tiles.contains(&IVec2::new(1, 2)));
        assert!(tiles.contains(&IVec2::new(5, 5)));
    }
}
