//! [`ChargeSelfKeeper`] — shared by every specialization: once charge drops it
//! latches a recharge wish that pre-empts routine duty until the bot is full.

use super::Behavior;
use crate::actor::brain::priority::{Priorities, PriorityKind};
use crate::actor::brain::BrainContext;

/// Charge fraction at or below which the bot decides it must recharge.
const CHARGE_TRIGGER: f32 = 0.25;
/// Charge fraction at which the recharge latch releases (treated as full).
const CHARGE_RELEASE: f32 = 0.999;
/// Floor on the recharge wish while latched, so topping up keeps dominating
/// routine duty until the bot is full (no early-undock thrash).
const RECHARGE_ACTIVE_FLOOR: f32 = 50.0;

/// Wishes to recharge once charge drops to [`CHARGE_TRIGGER`], staying latched
/// until full. While latched the wish value is the missing-charge percentage
/// (≥75 at the trigger, rising as charge falls), floored at
/// [`RECHARGE_ACTIVE_FLOOR`] so a near-full top-up still outranks routine duty.
pub struct ChargeSelfKeeper {
    latched: bool,
}

impl ChargeSelfKeeper {
    pub fn new() -> Self {
        Self { latched: false }
    }

    pub fn latched(&self) -> bool {
        self.latched
    }
}

impl Default for ChargeSelfKeeper {
    fn default() -> Self {
        Self::new()
    }
}

impl Behavior for ChargeSelfKeeper {
    fn update_priorities(&mut self, ctx: &BrainContext, priorities: &mut Priorities) {
        if ctx.charge <= CHARGE_TRIGGER {
            self.latched = true;
        } else if ctx.charge >= CHARGE_RELEASE {
            self.latched = false;
        }
        if self.latched {
            let value = ctx.missing_charge_pct.max(RECHARGE_ACTIVE_FLOOR);
            priorities.set(PriorityKind::RechargeYourself, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::brain::test_support::ctx_with_charge;

    #[test]
    fn charge_keeper_silent_above_trigger() {
        let mut b = ChargeSelfKeeper::new();
        let mut p = Priorities::new();
        b.update_priorities(&ctx_with_charge(0.40), &mut p);
        assert_eq!(p.value_of(PriorityKind::RechargeYourself), None);
        assert!(!b.latched());
    }

    #[test]
    fn charge_keeper_triggers_at_25_pct_with_value_75() {
        let mut b = ChargeSelfKeeper::new();
        let mut p = Priorities::new();
        b.update_priorities(&ctx_with_charge(0.25), &mut p);
        assert_eq!(p.value_of(PriorityKind::RechargeYourself), Some(75.0));
    }

    #[test]
    fn charge_keeper_value_rises_as_charge_falls() {
        let mut b = ChargeSelfKeeper::new();
        let mut p = Priorities::new();
        b.update_priorities(&ctx_with_charge(0.10), &mut p);
        assert_eq!(p.value_of(PriorityKind::RechargeYourself), Some(90.0));
    }

    #[test]
    fn charge_keeper_latches_until_full() {
        let mut b = ChargeSelfKeeper::new();
        // Drop below trigger -> latch.
        let mut p = Priorities::new();
        b.update_priorities(&ctx_with_charge(0.20), &mut p);
        assert!(b.latched());

        // Charging up to 90%: still latched, floored at 50 (above routine's 15).
        let mut p = Priorities::new();
        b.update_priorities(&ctx_with_charge(0.90), &mut p);
        assert!(b.latched());
        assert_eq!(p.value_of(PriorityKind::RechargeYourself), Some(50.0));

        // Full: releases and goes silent.
        let mut p = Priorities::new();
        b.update_priorities(&ctx_with_charge(1.0), &mut p);
        assert!(!b.latched());
        assert_eq!(p.value_of(PriorityKind::RechargeYourself), None);
    }
}
