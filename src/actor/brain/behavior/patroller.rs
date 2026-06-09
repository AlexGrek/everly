//! [`Patroller`] — the `PATROL` specialization's routine: wish to patrol a fixed
//! loop of cells, at the routine-band value.
//!
//! This behavior only *raises the wish*. The loop itself lives on the bot's
//! [`Patrol`](crate::actor::black_bot::Patrol) component and is walked by
//! [`GoToPatrol`](crate::actor::brain::GoToPatrol). Because the wish sits in the
//! routine band (same as wandering), a recharge need still pre-empts it — the
//! bot leaves patrol only to recharge, then resumes where it stopped.

use super::behavior_utils::ROUTINE_WISH_VALUE;
use super::Behavior;
use crate::actor::brain::priority::{Priorities, PriorityKind};
use crate::actor::brain::BrainContext;

/// Always wishes to patrol the bot's fixed loop, at a constant routine-band
/// priority.
pub struct Patroller;

impl Behavior for Patroller {
    fn update_priorities(&mut self, _ctx: &BrainContext, priorities: &mut Priorities) {
        priorities.set(PriorityKind::Patrolling, ROUTINE_WISH_VALUE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::brain::test_support::ctx_with_charge;

    #[test]
    fn patroller_always_wishes_to_patrol() {
        let mut b = Patroller;
        let mut p = Priorities::new();
        let ctx = ctx_with_charge(1.0);
        b.update_priorities(&ctx, &mut p);
        assert_eq!(p.value_of(PriorityKind::Patrolling), Some(ROUTINE_WISH_VALUE));
    }
}
