//! Step: place one parts depot per house, set against an interior wall.
//!
//! A parts depot ([`DraftTile::PartsDepot`]) is a walkable storage cabinet
//! mounted on an interior wall — visually similar to a charging station but
//! distinct (amber indicator, wider cabinet). Placement rules are identical to
//! charging stations: the chosen cell must be an interior `Open` cell with
//! **exactly one** orthogonal wall neighbor, and must not overlap door reserve
//! zones or existing charger tiles. Exactly one depot is placed per house.

use std::collections::HashSet;

use crate::rng;

use super::draft::{DraftTile, MapDraft};
use super::step_door::entrypoint_reserved_cells;
use crate::map::world_map::ChargerFacing;

const SIDES: [(i32, i32, ChargerFacing); 4] = [
    (0, -1, ChargerFacing::North),
    (0, 1, ChargerFacing::South),
    (1, 0, ChargerFacing::East),
    (-1, 0, ChargerFacing::West),
];

impl MapDraft {
    pub fn step_place_parts_depots(&mut self) {
        for index in 0..self.houses.len() {
            if let Some((x, z, facing)) = self.pick_depot_site(index) {
                self.set(x, z, DraftTile::PartsDepot(facing));
            }
        }
    }

    fn pick_depot_site(&mut self, house_index: usize) -> Option<(i32, i32, ChargerFacing)> {
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

        // Exclude cells already occupied by chargers so depots and chargers don't stack.
        for z in z0..=z1 {
            for x in x0..=x1 {
                if matches!(self.get(x, z), DraftTile::Charger(_)) {
                    forbidden.insert((x, z));
                }
            }
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
                if forbidden.contains(&(x, z)) {
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
        Some(*rng::pick(&mut self.rng, &candidates))
    }
}
