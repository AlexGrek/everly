//! Bot memory — four fixed-size storages a [`Brain`](super::Brain) carries as
//! **persistent runtime state**.
//!
//! Each storage is a flat `[T; 256]` array addressed by a `#[repr(u8)]` enum ID,
//! so a memory slot is a stable, named byte address rather than a magic index:
//!
//! | Storage | Element | ID enum |
//! |---|---|---|
//! | [`IntegerMemory`](BotMemory::integer) | `i64` | [`IntegerMemoryId`] |
//! | [`FloatMemory`](BotMemory::float) | `f32` | [`FloatMemoryId`] |
//! | [`CoordinatesMemory`](BotMemory::coordinates) | [`IVec2`] | [`CoordinatesMemoryId`] |
//! | [`FreeformMemory`](BotMemory::freeform) | `Option<Box<dyn MemoryRecord>>` | [`FreeformMemoryId`] |
//!
//! **Persistence invariant:** memory survives [`Brain::reset()`](super::Brain::reset).
//! A reset wipes the bot's *plan* (current high-level action, low-level action,
//! priorities) but never its memory — so a counter like
//! [`HelpFailuresCount`](IntegerMemoryId::HelpFailuresCount) keeps accumulating
//! across the very resets it is meant to count.

use bevy::math::IVec2;

/// Number of addressable slots per storage (one byte of address space).
pub const MEMORY_SLOTS: usize = 256;

/// Marker trait for values stored in [`FreeformMemory`](BotMemory::freeform).
/// Consumers downcast (via [`std::any::Any`]-style patterns) as needed; the trait
/// itself only guarantees the record is thread-safe and inspectable.
pub trait MemoryRecord: Send + Sync + std::fmt::Debug {}

/// Named slots in [`IntegerMemory`](BotMemory::integer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IntegerMemoryId {
    /// Consecutive collision/stall resets a fixer has suffered on its *current*
    /// help task. Reset to `0` on a fresh claim or a successful delivery; once it
    /// passes 4 the fixer gives the task up (see `docs/dispatch.md`).
    HelpFailuresCount = 0,
}

/// Named slots in [`FloatMemory`](BotMemory::float). Reserved — no functions yet.
/// (No `#[repr(u8)]`: a zero-variant enum can't carry it; values are unconstructable
/// until a variant is added, and the `as u8` index cast still compiles.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatMemoryId {}

/// Named slots in [`CoordinatesMemory`](BotMemory::coordinates). Reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatesMemoryId {}

/// Named slots in [`FreeformMemory`](BotMemory::freeform). Reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeformMemoryId {}

/// A bot's four memory storages. Addressed by the per-storage ID enums; the byte
/// value of an ID is its slot index.
pub struct BotMemory {
    integers: [i64; MEMORY_SLOTS],
    floats: [f32; MEMORY_SLOTS],
    coordinates: [IVec2; MEMORY_SLOTS],
    freeform: [Option<Box<dyn MemoryRecord>>; MEMORY_SLOTS],
}

impl Default for BotMemory {
    fn default() -> Self {
        Self {
            integers: [0; MEMORY_SLOTS],
            floats: [0.0; MEMORY_SLOTS],
            coordinates: [IVec2::ZERO; MEMORY_SLOTS],
            // `Option<Box<_>>` isn't `Copy`, so the array literal form can't be
            // used; build each slot as `None`.
            freeform: std::array::from_fn(|_| None),
        }
    }
}

impl BotMemory {
    // --- IntegerMemory ------------------------------------------------------

    pub fn integer(&self, id: IntegerMemoryId) -> i64 {
        self.integers[id as u8 as usize]
    }

    pub fn set_integer(&mut self, id: IntegerMemoryId, value: i64) {
        self.integers[id as u8 as usize] = value;
    }

    /// Adds `delta` to the slot and returns the new value.
    pub fn bump_integer(&mut self, id: IntegerMemoryId, delta: i64) -> i64 {
        let slot = &mut self.integers[id as u8 as usize];
        *slot += delta;
        *slot
    }

    // --- FloatMemory --------------------------------------------------------

    pub fn float(&self, id: FloatMemoryId) -> f32 {
        self.floats[id as u8 as usize]
    }

    pub fn set_float(&mut self, id: FloatMemoryId, value: f32) {
        self.floats[id as u8 as usize] = value;
    }

    // --- CoordinatesMemory --------------------------------------------------

    pub fn coordinates(&self, id: CoordinatesMemoryId) -> IVec2 {
        self.coordinates[id as u8 as usize]
    }

    pub fn set_coordinates(&mut self, id: CoordinatesMemoryId, value: IVec2) {
        self.coordinates[id as u8 as usize] = value;
    }

    // --- FreeformMemory -----------------------------------------------------

    pub fn freeform(&self, id: FreeformMemoryId) -> Option<&dyn MemoryRecord> {
        self.freeform[id as u8 as usize].as_deref()
    }

    pub fn set_freeform(&mut self, id: FreeformMemoryId, value: Option<Box<dyn MemoryRecord>>) {
        self.freeform[id as u8 as usize] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_default_to_zero() {
        let mem = BotMemory::default();
        assert_eq!(mem.integer(IntegerMemoryId::HelpFailuresCount), 0);
    }

    #[test]
    fn set_and_get_integer_roundtrips() {
        let mut mem = BotMemory::default();
        mem.set_integer(IntegerMemoryId::HelpFailuresCount, 7);
        assert_eq!(mem.integer(IntegerMemoryId::HelpFailuresCount), 7);
    }

    #[test]
    fn bump_integer_accumulates_and_returns_new_value() {
        let mut mem = BotMemory::default();
        assert_eq!(mem.bump_integer(IntegerMemoryId::HelpFailuresCount, 1), 1);
        assert_eq!(mem.bump_integer(IntegerMemoryId::HelpFailuresCount, 1), 2);
        assert_eq!(mem.bump_integer(IntegerMemoryId::HelpFailuresCount, 3), 5);
        assert_eq!(mem.integer(IntegerMemoryId::HelpFailuresCount), 5);
    }

    #[derive(Debug, PartialEq)]
    struct TestRecord(u32);
    impl MemoryRecord for TestRecord {}

    #[test]
    fn freeform_stores_and_returns_record() {
        // Slot 0 stands in for a future FreeformMemoryId; addressed by raw index
        // through the public API once a variant exists. Here we exercise the
        // storage mechanics with an explicit transmute-free path: write via the
        // array's Default (None) then a Box, read it back.
        let mut mem = BotMemory::default();
        // No FreeformMemoryId variant yet, so drive the array directly through a
        // helper that mirrors set/get semantics for slot 0.
        mem.freeform[0] = Some(Box::new(TestRecord(42)));
        let got = mem.freeform[0].as_deref();
        assert!(got.is_some());
        assert_eq!(format!("{:?}", got.unwrap()), "TestRecord(42)");
    }
}
