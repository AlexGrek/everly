//! Priorities — the sorted "wishes" a bot's [`Behavior`](super::Behavior)s
//! produce each high-level tick.
//!
//! Each priority carries a [`PriorityKind`] and a `value` (an uncapped `f32`,
//! conventionally `0..=100`). The single highest-value priority selects the
//! bot's one exclusive high-level action (see [`super::Brain::tick`]).
//!
//! Value bands (documentation / tuning reference — not enforced):
//!
//! | Range | Meaning |
//! |-------|---------|
//! | 0–30  | basic routine tasks |
//! | 30–50 | high-priority routine tasks |
//! | 50–70 | reaction to interruptions |
//! | 70–90 | emergency behavior |

use std::cmp::Ordering;

/// Lower bound of the basic-routine band.
pub const BAND_ROUTINE: f32 = 0.0;
/// Lower bound of the high-priority-routine band.
pub const BAND_HIGH_ROUTINE: f32 = 30.0;
/// Lower bound of the interruption-reaction band.
pub const BAND_INTERRUPTION: f32 = 50.0;
/// Lower bound of the emergency band.
pub const BAND_EMERGENCY: f32 = 70.0;

/// Discriminant for the kinds of wishes a behavior can raise. Each kind maps to
/// exactly one high-level action via the brain's factory
/// ([`super::make_high_level`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriorityKind {
    /// Routine wander. Mapped to [`super::GoToRandomPoints`].
    RandomWalking,
    /// Routine patrol of a fixed loop of cells. Mapped to [`super::GoToPatrol`].
    Patrolling,
    /// Top up the battery. Mapped to [`super::GoToChargeStation`].
    RechargeYourself,
}

/// One wish: a kind plus how strongly the bot wants it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Priority {
    pub kind: PriorityKind,
    pub value: f32,
}

/// The bot's current wishes. Each tick the brain [`clear`](Self::clear)s this and
/// lets every behavior [`set`](Self::set) its wish; [`top`](Self::top) then
/// returns the dominant one. Backed by a reused `Vec` (cleared, never shrunk) so
/// the steady-state tick allocates nothing.
#[derive(Debug, Default)]
pub struct Priorities {
    items: Vec<Priority>,
}

impl Priorities {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Drops every wish but keeps the backing capacity (no realloc next tick).
    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Priority> {
        self.items.iter()
    }

    /// Upsert: overwrite the existing wish of this kind, or append a new one.
    /// A behavior that wants nothing this tick simply does not call `set`.
    pub fn set(&mut self, kind: PriorityKind, value: f32) {
        if let Some(p) = self.items.iter_mut().find(|p| p.kind == kind) {
            p.value = value;
        } else {
            self.items.push(Priority { kind, value });
        }
    }

    /// Current value of `kind`, if any behavior raised it this tick.
    pub fn value_of(&self, kind: PriorityKind) -> Option<f32> {
        self.items.iter().find(|p| p.kind == kind).map(|p| p.value)
    }

    /// The highest-value wish (the winner). `None` only when no behavior raised
    /// anything this tick.
    pub fn top(&self) -> Option<Priority> {
        self.items
            .iter()
            .copied()
            .max_by(|a, b| a.value.partial_cmp(&b.value).unwrap_or(Ordering::Equal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_upserts_in_place() {
        let mut p = Priorities::new();
        p.set(PriorityKind::RandomWalking, 15.0);
        p.set(PriorityKind::RandomWalking, 20.0);
        assert_eq!(p.iter().count(), 1, "same kind must upsert, not duplicate");
        assert_eq!(p.value_of(PriorityKind::RandomWalking), Some(20.0));
    }

    #[test]
    fn top_picks_max_value() {
        let mut p = Priorities::new();
        p.set(PriorityKind::RandomWalking, 15.0);
        p.set(PriorityKind::RechargeYourself, 78.0);
        assert_eq!(p.top().unwrap().kind, PriorityKind::RechargeYourself);

        p.set(PriorityKind::RechargeYourself, 5.0);
        assert_eq!(p.top().unwrap().kind, PriorityKind::RandomWalking);
    }

    #[test]
    fn empty_has_no_top() {
        let p = Priorities::new();
        assert!(p.top().is_none());
        assert!(p.is_empty());
    }

    #[test]
    fn clear_keeps_capacity() {
        let mut p = Priorities::new();
        p.set(PriorityKind::RandomWalking, 1.0);
        let cap = p.items.capacity();
        p.clear();
        assert!(p.is_empty());
        assert_eq!(p.items.capacity(), cap, "clear must not shrink the buffer");
    }
}
