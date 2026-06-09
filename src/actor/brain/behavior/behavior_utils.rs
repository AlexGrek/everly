//! Constants and helpers shared by more than one [`Behavior`](super::Behavior).

/// Wish value every routine, non-urgent duty raises — the basic-routine band
/// (see [`priority`](crate::actor::brain::priority) value bands).
///
/// Wandering ([`RandomWalker`](super::RandomWalker)) and patrolling
/// ([`Patroller`](super::Patroller)) share it so that, whatever a bot's
/// specialization, a recharge need (floored at `50`) always pre-empts ordinary
/// duty.
pub const ROUTINE_WISH_VALUE: f32 = 15.0;
