//! [`FixerDuty`] — the `FIXER` specialization's routine: always wish to do fixer
//! work (loiter near the home depot, claim repair requests, deliver parts), at
//! the routine-band value.
//!
//! Like patrolling, the wish sits in the routine band, so a recharge need still
//! pre-empts it — the fixer leaves its post only to recharge, then resumes.
//! The actual loiter / claim / fetch / deliver state machine lives in
//! [`GoFixBots`](crate::actor::brain::GoFixBots).

use super::behavior_utils::ROUTINE_WISH_VALUE;
use super::Behavior;
use crate::actor::brain::priority::{Priorities, PriorityKind};
use crate::actor::brain::BrainContext;

/// Always wishes to perform fixer duty, at a constant routine-band priority.
pub struct FixerDuty;

impl Behavior for FixerDuty {
    fn update_priorities(&mut self, _ctx: &BrainContext, priorities: &mut Priorities) {
        priorities.set(PriorityKind::Fixing, ROUTINE_WISH_VALUE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::brain::test_support::ctx_with_charge;

    #[test]
    fn fixer_always_wishes_to_fix() {
        let mut b = FixerDuty;
        let mut p = Priorities::new();
        let ctx = ctx_with_charge(1.0);
        b.update_priorities(&ctx, &mut p);
        assert_eq!(p.value_of(PriorityKind::Fixing), Some(ROUTINE_WISH_VALUE));
    }
}
