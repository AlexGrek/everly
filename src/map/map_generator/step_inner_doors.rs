//! Step 9c: open doors in inner walls so every room is reachable from the house entry.
//!
//! Connectivity here is **edge-based**, matching the runtime geometry: a `Wall(bits)`
//! cell is walkable floor carrying slabs on the named edges, not a solid blocker. Two
//! adjacent in-house cells are connected unless a slab sits on their shared edge (from
//! either side). Inner walls (`step_inner_walls`) lay full rows of `MASK_NORTH` /
//! columns of `MASK_WEST` slabs, sealing rooms off; a door is one edge with its slab
//! bits cleared.
//!
//! The loop floods the accessible region from the entry, finds any blocked edge between
//! an accessible cell and an in-house cell that is not yet accessible, clears that edge,
//! and repeats until the whole house is one connected region.

use std::collections::{HashSet, VecDeque};

use rand::Rng;

use crate::map::world_map::{MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

use super::draft::{DraftTile, MapDraft};
use super::house::House;
use super::step_home_crawler::house_entry_interior_tile;

/// (dx, dz, slab bit on this cell, slab bit on the neighbour) for the four edges.
const EDGES: [(i32, i32, u8, u8); 4] = [
    (0, -1, MASK_NORTH, MASK_SOUTH),
    (0, 1, MASK_SOUTH, MASK_NORTH),
    (1, 0, MASK_EAST, MASK_WEST),
    (-1, 0, MASK_WEST, MASK_EAST),
];

impl MapDraft {
    pub fn step_place_inner_doors(&mut self) {
        for idx in 0..self.houses.len() {
            self.connect_house_rooms(idx);
        }
    }

    fn connect_house_rooms(&mut self, idx: usize) {
        let house = self.houses[idx].clone();
        let Some(ref entry) = house.entry else {
            return;
        };
        let Some(start) = house_entry_interior_tile(self, entry)
            .or_else(|| first_walkable_in_house(self, &house))
        else {
            return;
        };

        loop {
            let accessible = flood_rooms(self, &house, start);
            let candidates = door_candidates(self, &house, &accessible);
            if candidates.is_empty() {
                break;
            }
            let (x, z, nx, nz, this_bit, nbr_bit) =
                candidates[self.rng.gen_range(0..candidates.len())];
            clear_edge_bit(self, x, z, this_bit);
            clear_edge_bit(self, nx, nz, nbr_bit);
        }
    }
}

/// In-house cells reachable from `start` over passable edges (slab-free shared edges).
fn flood_rooms(draft: &MapDraft, house: &House, start: (i32, i32)) -> HashSet<(i32, i32)> {
    let mut visited = HashSet::from([start]);
    let mut queue = VecDeque::from([start]);
    while let Some((x, z)) = queue.pop_front() {
        for &(dx, dz, this_bit, nbr_bit) in &EDGES {
            let n = (x + dx, z + dz);
            if visited.contains(&n) {
                continue;
            }
            if edge_passable(draft, house, (x, z), n, this_bit, nbr_bit) {
                visited.insert(n);
                queue.push_back(n);
            }
        }
    }
    visited
}

/// Blocked edges separating the accessible region from an in-house, not-yet-accessible
/// cell — `(x, z, nx, nz, this_bit, nbr_bit)`; clearing both bits opens the door.
fn door_candidates(
    draft: &MapDraft,
    house: &House,
    accessible: &HashSet<(i32, i32)>,
) -> Vec<(i32, i32, i32, i32, u8, u8)> {
    let mut out = Vec::new();
    for &(x, z) in accessible {
        for &(dx, dz, this_bit, nbr_bit) in &EDGES {
            let (nx, nz) = (x + dx, z + dz);
            if accessible.contains(&(nx, nz)) {
                continue;
            }
            if !house.contains(nx, nz) || !walkable(draft, nx, nz) {
                continue;
            }
            // Only blocked edges are doors-to-be; an open edge is already flooded through.
            if edge_passable(draft, house, (x, z), (nx, nz), this_bit, nbr_bit) {
                continue;
            }
            out.push((x, z, nx, nz, this_bit, nbr_bit));
        }
    }
    out
}

fn edge_passable(
    draft: &MapDraft,
    house: &House,
    cell: (i32, i32),
    nbr: (i32, i32),
    this_bit: u8,
    nbr_bit: u8,
) -> bool {
    if !house.contains(nbr.0, nbr.1) {
        return false;
    }
    let (Some(b), Some(nb)) = (
        cell_bits(draft, cell.0, cell.1),
        cell_bits(draft, nbr.0, nbr.1),
    ) else {
        return false;
    };
    b & this_bit == 0 && nb & nbr_bit == 0
}

/// Slab bits on a walkable cell, or `None` when the cell cannot be walked at all.
///
/// `Open` and `Corner` carry no edge slabs; `Wall(bits)` floor carries the named slabs.
fn cell_bits(draft: &MapDraft, x: i32, z: i32) -> Option<u8> {
    match draft.get(x, z) {
        DraftTile::Open | DraftTile::Corner(_) => Some(0),
        DraftTile::Wall(bits) => Some(bits),
        DraftTile::Void => None,
    }
}

fn walkable(draft: &MapDraft, x: i32, z: i32) -> bool {
    cell_bits(draft, x, z).is_some()
}

fn clear_edge_bit(draft: &mut MapDraft, x: i32, z: i32, bit: u8) {
    if let DraftTile::Wall(bits) = draft.get(x, z) {
        let cleared = bits & !bit;
        if cleared == 0 {
            draft.set(x, z, DraftTile::Open);
        } else {
            draft.set(x, z, DraftTile::Wall(cleared));
        }
    }
}

fn first_walkable_in_house(draft: &MapDraft, house: &House) -> Option<(i32, i32)> {
    for z in house.z0..=house.z1 {
        for x in house.x0..=house.x1 {
            if house.contains(x, z) && walkable(draft, x, z) {
                return Some((x, z));
            }
        }
    }
    None
}
