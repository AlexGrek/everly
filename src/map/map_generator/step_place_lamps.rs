//! Step 15: 3–7 lamp decorations per house, placed on wall cells of both inner
//! and outer walls.
//!
//! A lamp sits on top of the wall slab in the given facing direction. Eligibility:
//! - The cell is a `Wall` tile (not Corner, not Charger, not Open).
//! - The facing direction is one of the wall's own slab bits.
//! - At least one neighbour in the facing direction is an `Open` cell (road or
//!   house interior) — prevents floating lamps on dead walls.
//! - The cell does not already have a lamp, and is not a charger cell.

use std::collections::HashSet;

use crate::rng;

use super::draft::{DraftTile, MapDraft};
use crate::map::world_map::{LampDecoration, LampFacing, MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

const MIN_LAMPS_PER_HOUSE: i32 = 3;
const MAX_LAMPS_PER_HOUSE: i32 = 7;

/// (dx, dz, mask_bit, LampFacing)
const WALL_DIRS: [(i32, i32, u8, LampFacing); 4] = [
    (0, -1, MASK_NORTH, LampFacing::North),
    (0, 1, MASK_SOUTH, LampFacing::South),
    (1, 0, MASK_EAST, LampFacing::East),
    (-1, 0, MASK_WEST, LampFacing::West),
];

impl MapDraft {
    pub fn step_place_lamps(&mut self) {
        for index in 0..self.houses.len() {
            let count = rng::range(&mut self.rng, MIN_LAMPS_PER_HOUSE..=MAX_LAMPS_PER_HOUSE);
            let mut used: HashSet<(i32, i32)> = HashSet::new();
            for _ in 0..count {
                let Some((x, z, facing)) = self.pick_lamp_site(index, &used) else {
                    break;
                };
                self.set_lamp(x, z, LampDecoration::Lamp(facing));
                used.insert((x, z));
            }
        }
    }

    fn pick_lamp_site(
        &mut self,
        house_index: usize,
        used: &HashSet<(i32, i32)>,
    ) -> Option<(i32, i32, LampFacing)> {
        let (x0, z0, x1, z1) = {
            let h = &self.houses[house_index];
            // Extend bounds by 1 to also consider outer wall cells just outside the interior.
            (h.x0 - 1, h.z0 - 1, h.x1 + 1, h.z1 + 1)
        };

        let mut candidates: Vec<(i32, i32, LampFacing)> = Vec::new();

        for z in z0..=z1 {
            for x in x0..=x1 {
                if !self.in_bounds(x, z) {
                    continue;
                }
                let DraftTile::Wall(mask) = self.get(x, z) else {
                    continue;
                };
                if used.contains(&(x, z)) {
                    continue;
                }
                // No lamp where a charger already occupies an adjacent cell
                // (chargers are Open cells next to walls, not the wall itself).
                // Also skip cells already decorated.
                if self.get_lamp(x, z) != LampDecoration::None {
                    continue;
                }

                for &(dx, dz, bit, facing) in &WALL_DIRS {
                    if mask & bit == 0 {
                        continue;
                    }
                    // The cell in the slab direction must be passable (Open) — prevents
                    // lamps hanging over solid or void territory.
                    let nx = x + dx;
                    let nz = z + dz;
                    if !self.in_bounds(nx, nz) {
                        continue;
                    }
                    if !matches!(self.get(nx, nz), DraftTile::Open | DraftTile::Charger(_)) {
                        continue;
                    }
                    candidates.push((x, z, facing));
                }
            }
        }

        if candidates.is_empty() {
            return None;
        }
        Some(*rng::pick(&mut self.rng, &candidates))
    }
}
