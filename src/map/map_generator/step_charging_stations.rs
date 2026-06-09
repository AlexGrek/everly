//! Step 12: up to three charging stations per house, set against interior walls.
//!
//! A charging station ([`DraftTile::Charger`]) is a walkable metal pad whose
//! glowing cube hangs on the wall behind it. Placement rules mirror the user
//! intent: inside the house, **backing onto a wall** (the cube's wall), and
//! **not in a corner**. Concretely a candidate is an interior `Open` cell with
//! **exactly one** orthogonal neighbor that is a wall of this house — that lone
//! wall side becomes the charger's [`ChargerFacing`]. Doorway tiles are excluded.

use std::collections::HashSet;

use rand::Rng;

use super::draft::{DraftTile, MapDraft};
use super::step_door::entrypoint_reserved_cells;
use crate::map::world_map::ChargerFacing;

/// Maximum charging stations placed in one house (actual count is random `1..=MAX`).
const MAX_CHARGERS_PER_HOUSE: i32 = 3;

/// Neighbor deltas paired with the facing they imply (the wall the back faces).
const SIDES: [(i32, i32, ChargerFacing); 4] = [
    (0, -1, ChargerFacing::North),
    (0, 1, ChargerFacing::South),
    (1, 0, ChargerFacing::East),
    (-1, 0, ChargerFacing::West),
];

impl MapDraft {
    pub fn step_place_charging_stations(&mut self) {
        for index in 0..self.houses.len() {
            let target = self.rng.gen_range(1..=MAX_CHARGERS_PER_HOUSE);
            let mut used = HashSet::new();
            for _ in 0..target {
                let Some((x, z, facing)) = self.pick_charger_site(index, &used) else {
                    break;
                };
                self.set(x, z, DraftTile::Charger(facing));
                used.insert((x, z));
            }
        }
    }

    fn pick_charger_site(
        &mut self,
        house_index: usize,
        used: &HashSet<(i32, i32)>,
    ) -> Option<(i32, i32, ChargerFacing)> {
        let (x0, z0, x1, z1) = {
            let h = &self.houses[house_index];
            (h.x0, h.z0, h.x1, h.z1)
        };

        let mut forbidden: HashSet<(i32, i32)> = HashSet::new();
        for ep in [
            self.houses[house_index].entry.as_ref(),
            self.houses[house_index].entry2.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            forbidden.extend(entrypoint_reserved_cells(ep));
        }

        let mut candidates: Vec<(i32, i32, ChargerFacing)> = Vec::new();
        for z in z0..=z1 {
            for x in x0..=x1 {
                if self.get(x, z) != DraftTile::Open {
                    continue;
                }
                if !self.houses[house_index].contains(x, z) {
                    continue;
                }
                if forbidden.contains(&(x, z)) || used.contains(&(x, z)) {
                    continue;
                }

                let mut wall_sides = SIDES.iter().filter(|&&(dx, dz, _)| {
                    let (nx, nz) = (x + dx, z + dz);
                    matches!(self.get(nx, nz), DraftTile::Wall(_) | DraftTile::Corner(_))
                });

                // Exactly one wall neighbor → against a wall, not in a corner.
                if let Some(&(_, _, facing)) = wall_sides.next() {
                    if wall_sides.next().is_none() {
                        candidates.push((x, z, facing));
                    }
                }
            }
        }

        if candidates.is_empty() {
            return None;
        }
        Some(candidates[self.rng.gen_range(0..candidates.len())])
    }
}
