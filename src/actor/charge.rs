//! Battery charge for bots.
//!
//! Every bot entity carries a [`Charge`] component that drains continuously
//! while the simulation runs ([`discharge_actors`]). A depleted bot
//! (`level <= 0.0`) is immobilized: each bot's think system skips writing a
//! movement intent when its charge is gone, so it neither advances on the
//! collision grid nor drifts visually. Charge is clamped to `[0.0, 1.0]` and
//! never drains below `0.0`.

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::Rng;

use crate::actor::is_paused;
use crate::menu::main_menu::GameState;

/// Fraction of full charge drained per second while the simulation runs.
/// At this rate a fully charged bot takes ~500 s (≈8 min) to deplete.
const DISCHARGE_PER_S: f32 = 0.002;

/// Inclusive bounds for a freshly spawned bot's random starting charge.
const SPAWN_CHARGE_MIN: f32 = 0.3;
const SPAWN_CHARGE_MAX: f32 = 1.0;

/// Per-bot battery level in `[0.0, 1.0]`. A bot at `0.0` cannot move.
///
/// Lives on the same entity as [`ActorObject`](crate::actor::ActorObject) and
/// the bot's visual component.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Charge {
    /// Current charge, always within `[0.0, 1.0]`.
    pub level: f32,
}

impl Charge {
    /// Builds a charge clamped into the valid `[0.0, 1.0]` range.
    pub fn new(level: f32) -> Self {
        Self {
            level: level.clamp(0.0, 1.0),
        }
    }

    /// Random starting charge in `[SPAWN_CHARGE_MIN, SPAWN_CHARGE_MAX]`.
    pub fn random(rng: &mut StdRng) -> Self {
        Self::new(rng.gen_range(SPAWN_CHARGE_MIN..=SPAWN_CHARGE_MAX))
    }

    /// `true` when the bot has no charge left and must not move.
    #[inline]
    pub fn is_depleted(&self) -> bool {
        self.level <= 0.0
    }
}

/// Drains every actor's [`Charge`] each in-game frame, clamped at `0.0`.
pub struct ChargePlugin;

impl Plugin for ChargePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            discharge_actors
                .run_if(in_state(GameState::InGame))
                .run_if(not(is_paused)),
        );
    }
}

fn discharge_actors(time: Res<Time>, mut charges: Query<&mut Charge>) {
    let drop = DISCHARGE_PER_S * time.delta_secs();
    if drop <= 0.0 {
        return;
    }
    for mut charge in &mut charges {
        if charge.level > 0.0 {
            charge.level = (charge.level - drop).max(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn new_clamps_out_of_range() {
        assert_eq!(Charge::new(1.5).level, 1.0);
        assert_eq!(Charge::new(-0.3).level, 0.0);
        assert_eq!(Charge::new(0.42).level, 0.42);
    }

    #[test]
    fn depleted_only_at_or_below_zero() {
        assert!(Charge::new(0.0).is_depleted());
        assert!(!Charge::new(0.01).is_depleted());
        assert!(!Charge::new(1.0).is_depleted());
    }

    #[test]
    fn random_charge_within_spawn_bounds() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..256 {
            let c = Charge::random(&mut rng);
            assert!(
                (SPAWN_CHARGE_MIN..=SPAWN_CHARGE_MAX).contains(&c.level),
                "spawn charge {} out of [{SPAWN_CHARGE_MIN}, {SPAWN_CHARGE_MAX}]",
                c.level
            );
        }
    }
}
