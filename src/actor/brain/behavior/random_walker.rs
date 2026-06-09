//! [`RandomWalker`] — the `DO_NOTHING` specialization's routine: always wish to
//! wander, at the constant routine-band value.

use super::behavior_utils::ROUTINE_WISH_VALUE;
use super::Behavior;
use crate::actor::brain::priority::{Priorities, PriorityKind};
use crate::actor::brain::BrainContext;

/// Always wishes to wander, at a constant low (routine-band) priority.
pub struct RandomWalker;

impl Behavior for RandomWalker {
    fn update_priorities(&mut self, _ctx: &BrainContext, priorities: &mut Priorities) {
        priorities.set(PriorityKind::RandomWalking, ROUTINE_WISH_VALUE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::brain::test_support::ctx_with_charge;

    #[test]
    fn random_walker_always_emits_routine_value() {
        let mut b = RandomWalker;
        let mut p = Priorities::new();
        let ctx = ctx_with_charge(1.0);
        b.update_priorities(&ctx, &mut p);
        assert_eq!(p.value_of(PriorityKind::RandomWalking), Some(ROUTINE_WISH_VALUE));
    }
}
