//! [`CleanerDuty`] — the `CLEANER` specialization's routine: always wish to
//! clean, at the constant routine-band value so a recharge still pre-empts it.

use super::behavior_utils::ROUTINE_WISH_VALUE;
use super::Behavior;
use crate::actor::brain::priority::{Priorities, PriorityKind};
use crate::actor::brain::BrainContext;

/// Always wishes to clean, at a constant low (routine-band) priority. The
/// scan/clean/relocate machine lives in
/// [`GoClean`](crate::actor::brain::GoClean).
pub struct CleanerDuty;

impl Behavior for CleanerDuty {
    fn update_priorities(&mut self, _ctx: &BrainContext, priorities: &mut Priorities) {
        priorities.set(PriorityKind::Cleaning, ROUTINE_WISH_VALUE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::brain::test_support::ctx_with_charge;

    #[test]
    fn cleaner_duty_always_emits_routine_value() {
        let mut b = CleanerDuty;
        let mut p = Priorities::new();
        let ctx = ctx_with_charge(1.0);
        b.update_priorities(&ctx, &mut p);
        assert_eq!(p.value_of(PriorityKind::Cleaning), Some(ROUTINE_WISH_VALUE));
    }
}
