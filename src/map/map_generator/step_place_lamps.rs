//! Step 11: lamp decorations per house.
//!
//! Inner sconces (`Lamp`): placed on wall cells with a facing bit whose
//! adjacent cell is passable interior.  Each house gets its own dedicated
//! inner-lamp budget (`MIN_INNER_LAMPS`–`MAX_INNER_LAMPS`).
//!
//! Outer sconces (`LampOuter`): placed on road cells just outside the house
//! adjacent to an outward-facing wall slab.  Separate, smaller budget per
//! house (`MIN_OUTER_LAMPS`–`MAX_OUTER_LAMPS`).

use std::collections::HashSet;

use crate::rng;

use super::draft::{DraftTile, MapDraft};
use crate::map::world_map::{
    LampDecoration, LampFacing, MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST,
};

const MIN_INNER_LAMPS: i32 = 5;
const MAX_INNER_LAMPS: i32 = 12;

const MIN_OUTER_LAMPS: i32 = 0;
const MAX_OUTER_LAMPS: i32 = 3;

/// (dx, dz, mask_bit, LampFacing) — direction of movement, slab bitmask for
/// an inner wall in that direction, and the facing value stored on the lamp.
const WALL_DIRS: [(i32, i32, u8, LampFacing); 4] = [
    (0, -1, MASK_NORTH, LampFacing::North),
    (0, 1, MASK_SOUTH, LampFacing::South),
    (1, 0, MASK_EAST, LampFacing::East),
    (-1, 0, MASK_WEST, LampFacing::West),
];

/// Given movement direction (dx, dz) from road cell to wall cell, returns the
/// slab bitmask the wall must have facing BACK toward the road cell.
fn slab_bit_toward_road(dx: i32, dz: i32) -> u8 {
    match (dx, dz) {
        (0, -1) => MASK_SOUTH, // moved north to wall → wall faces south back to road
        (0, 1) => MASK_NORTH,
        (1, 0) => MASK_WEST,
        (-1, 0) => MASK_EAST,
        _ => 0,
    }
}

impl MapDraft {
    pub fn step_place_lamps(&mut self) {
        for index in 0..self.houses.len() {
            self.place_inner_lamps(index);
            self.place_outer_lamps(index);
        }
    }

    fn place_inner_lamps(&mut self, house_index: usize) {
        let count = rng::range(&mut self.rng, MIN_INNER_LAMPS..=MAX_INNER_LAMPS);
        let mut used: HashSet<(i32, i32)> = HashSet::new();
        for _ in 0..count {
            let Some((x, z, decoration)) = self.pick_inner_lamp(house_index, &used) else {
                break;
            };
            self.set_lamp(x, z, decoration);
            used.insert((x, z));
        }
    }

    fn place_outer_lamps(&mut self, house_index: usize) {
        let count = rng::range(&mut self.rng, MIN_OUTER_LAMPS..=MAX_OUTER_LAMPS);
        let mut used: HashSet<(i32, i32)> = HashSet::new();
        for _ in 0..count {
            let Some((x, z, decoration)) = self.pick_outer_lamp(house_index, &used) else {
                break;
            };
            self.set_lamp(x, z, decoration);
            used.insert((x, z));
        }
    }

    fn pick_inner_lamp(
        &mut self,
        house_index: usize,
        used: &HashSet<(i32, i32)>,
    ) -> Option<(i32, i32, LampDecoration)> {
        let (x0, z0, x1, z1) = {
            let h = &self.houses[house_index];
            (h.x0, h.z0, h.x1, h.z1)
        };

        let mut candidates: Vec<(i32, i32, LampDecoration)> = Vec::new();

        for z in (z0 - 1)..=(z1 + 1) {
            for x in (x0 - 1)..=(x1 + 1) {
                if !self.in_bounds(x, z) {
                    continue;
                }
                let DraftTile::Wall(mask) = self.get(x, z) else {
                    continue;
                };
                if used.contains(&(x, z)) || self.get_lamp(x, z) != LampDecoration::None {
                    continue;
                }
                for &(dx, dz, bit, facing) in &WALL_DIRS {
                    if mask & bit == 0 {
                        continue;
                    }
                    let nx = x + dx;
                    let nz = z + dz;
                    if !self.in_bounds(nx, nz) {
                        continue;
                    }
                    if !matches!(self.get(nx, nz), DraftTile::Open | DraftTile::Charger(_)) {
                        continue;
                    }
                    if !self.houses[house_index].contains(nx, nz) {
                        continue;
                    }
                    candidates.push((x, z, LampDecoration::Lamp(facing)));
                }
            }
        }

        if candidates.is_empty() {
            return None;
        }
        Some(*rng::pick(&mut self.rng, &candidates))
    }

    fn pick_outer_lamp(
        &mut self,
        house_index: usize,
        used: &HashSet<(i32, i32)>,
    ) -> Option<(i32, i32, LampDecoration)> {
        let (x0, z0, x1, z1) = {
            let h = &self.houses[house_index];
            (h.x0, h.z0, h.x1, h.z1)
        };

        let mut candidates: Vec<(i32, i32, LampDecoration)> = Vec::new();

        for z in (z0 - 2)..=(z1 + 2) {
            for x in (x0 - 2)..=(x1 + 2) {
                if !self.in_bounds(x, z) {
                    continue;
                }
                if self.get(x, z) != DraftTile::Open {
                    continue;
                }
                if self.houses[house_index].contains(x, z) {
                    continue;
                }
                if used.contains(&(x, z)) || self.get_lamp(x, z) != LampDecoration::None {
                    continue;
                }
                for &(dx, dz, _bit, facing) in &WALL_DIRS {
                    let wx = x + dx;
                    let wz = z + dz;
                    if !self.in_bounds(wx, wz) {
                        continue;
                    }
                    let DraftTile::Wall(mask) = self.get(wx, wz) else {
                        continue;
                    };
                    if mask & slab_bit_toward_road(dx, dz) == 0 {
                        continue;
                    }
                    candidates.push((x, z, LampDecoration::LampOuter(facing)));
                }
            }
        }

        if candidates.is_empty() {
            return None;
        }
        Some(*rng::pick(&mut self.rng, &candidates))
    }
}
