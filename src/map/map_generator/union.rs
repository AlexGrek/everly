//! Union-of-rooms geometry shared by shell, corners, and door steps.

use crate::map::world_map::{WallMask, MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

use super::draft::Room;

pub(crate) fn union_contains(rooms: &[Room], x: i32, z: i32) -> bool {
    rooms.iter().any(|r| r.contains(x, z))
}

/// Outward-facing wall bits for a union perimeter cell (`None` = not on the shell).
pub(crate) fn union_perimeter_wall_mask(rooms: &[Room], x: i32, z: i32) -> Option<WallMask> {
    if !union_contains(rooms, x, z) {
        return None;
    }
    let mut bits = 0u8;
    if !union_contains(rooms, x, z - 1) {
        bits |= MASK_NORTH;
    }
    if !union_contains(rooms, x, z + 1) {
        bits |= MASK_SOUTH;
    }
    if !union_contains(rooms, x - 1, z) {
        bits |= MASK_WEST;
    }
    if !union_contains(rooms, x + 1, z) {
        bits |= MASK_EAST;
    }
    WallMask::from_bits(bits)
}
