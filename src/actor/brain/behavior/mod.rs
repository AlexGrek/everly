//! Behaviors — the rules that, each high-level tick, raise the bot's wishes.
//!
//! Every behavior receives the full [`BrainContext`] (all the bot properties it
//! could need) and mutates the shared [`Priorities`](super::Priorities) list.
//! Behaviors may hold their own state (e.g. a hysteresis latch); they persist
//! for the bot's life.
//!
//! A BlackBot's **specialization** is exactly its *set* of behaviors — see
//! [`BotSpecialization`](crate::actor::black_bot::BotSpecialization), which maps
//! each specialization to the behaviors below:
//!
//! - `DO_NOTHING` wanders to random cells ([`RandomWalker`]).
//! - `PATROL` sticks to a fixed loop of cells ([`Patroller`]).
//!
//! Both also keep themselves charged ([`ChargeSelfKeeper`]), which pre-empts the
//! routine duty when the battery runs low.
//!
//! Each behavior lives in its own module; constants shared between behaviors
//! live in [`behavior_utils`].

pub mod behavior_utils;
pub mod charge_self_keeper;
pub mod fixer;
pub mod patroller;
pub mod random_walker;

pub use charge_self_keeper::ChargeSelfKeeper;
pub use fixer::FixerDuty;
pub use patroller::Patroller;
pub use random_walker::RandomWalker;

use super::priority::Priorities;
use super::BrainContext;

/// A rule that raises priorities from the bot's current state.
pub trait Behavior: Send + Sync {
    fn update_priorities(&mut self, ctx: &BrainContext, priorities: &mut Priorities);
}
