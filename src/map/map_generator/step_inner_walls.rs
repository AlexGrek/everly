//! Step 9b: split each house into rooms with axis-aligned inner walls (no doors yet).
//!
//! Total lines allowed = `footprint_area / 30` (one per 30 sq units). The budget
//! is split evenly between horizontal and vertical orientations (ceiling to H).
//! A candidate cut is accepted only when:
//!   1. every resulting sub-room has area >= [`MIN_ROOM_AREA`] cells,
//!   2. every resulting sub-room is at least [`MIN_ROOM_DIM`] cells in either
//!      direction (no 1-unit-wide spaces),
//!   3. the cut is at least [`MIN_PARALLEL_WALL_DISTANCE`] cells away from any
//!      existing parallel wall — outer perimeter or inner wall placed earlier.

use rand::Rng;

use crate::map::world_map::{MASK_NORTH, MASK_WEST};

use super::draft::{DraftTile, MapDraft};
use super::types::MIN_HOUSE_AREA_FOR_CENTER_WAVE;

/// One inner wall is permitted per this many cells of house footprint area.
const AREA_PER_INNER_WALL: i32 = 80;
const MIN_ROOM_AREA: i32 = 6;
const MIN_ROOM_DIM: i32 = 2;
const MIN_PARALLEL_WALL_DISTANCE: i32 = 2;

impl MapDraft {
    pub fn step_split_houses_into_rooms(&mut self) {
        for idx in 0..self.houses.len() {
            if self.houses[idx].footprint_area < MIN_HOUSE_AREA_FOR_CENTER_WAVE {
                continue;
            }
            self.split_house_into_rooms(idx);
        }
    }

    fn split_house_into_rooms(&mut self, idx: usize) {
        let (x0, z0, x1, z1, area) = {
            let h = &self.houses[idx];
            (h.x0, h.z0, h.x1, h.z1, h.footprint_area)
        };

        // Total lines = floor(area / 30); split ceiling-to-H, floor-to-V.
        let budget = (area / AREA_PER_INNER_WALL) as usize;
        let h_max = (budget + 1) / 2;
        let v_max = budget / 2;

        // Walls live on lines *between* cell rows/columns. Outer walls are at
        // lines z0 / z1+1 (horizontal) and x0 / x1+1 (vertical).
        let mut h_cuts: Vec<i32> = vec![z0, z1 + 1];
        let mut v_cuts: Vec<i32> = vec![x0, x1 + 1];
        let mut inner_h: Vec<i32> = Vec::new();
        let mut inner_v: Vec<i32> = Vec::new();

        for _ in 0..h_max {
            let Some(zw) = self.pick_cut(z0 + 1, z1, &h_cuts, &v_cuts, true) else {
                break;
            };
            insert_sorted(&mut h_cuts, zw);
            inner_h.push(zw);
        }
        for _ in 0..v_max {
            let Some(xw) = self.pick_cut(x0 + 1, x1, &v_cuts, &h_cuts, false) else {
                break;
            };
            insert_sorted(&mut v_cuts, xw);
            inner_v.push(xw);
        }

        for zw in inner_h {
            self.stamp_horizontal_inner_wall(idx, zw);
        }
        for xw in inner_v {
            self.stamp_vertical_inner_wall(idx, xw);
        }
    }

    fn pick_cut(
        &mut self,
        lo: i32,
        hi: i32,
        own_cuts: &[i32],
        other_cuts: &[i32],
        is_horizontal: bool,
    ) -> Option<i32> {
        if hi < lo {
            return None;
        }
        let candidates: Vec<i32> = (lo..=hi)
            .filter(|&c| {
                own_cuts
                    .iter()
                    .all(|&oc| (c - oc).abs() >= MIN_PARALLEL_WALL_DISTANCE)
            })
            .filter(|&c| {
                let mut new_own = own_cuts.to_vec();
                new_own.push(c);
                new_own.sort();
                let (h, v) = if is_horizontal {
                    (new_own.as_slice(), other_cuts)
                } else {
                    (other_cuts, new_own.as_slice())
                };
                grid_rooms_satisfy(h, v)
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(candidates[self.rng.gen_range(0..candidates.len())])
    }

    fn stamp_horizontal_inner_wall(&mut self, house_idx: usize, zw: i32) {
        let (x0, x1) = {
            let h = &self.houses[house_idx];
            (h.x0, h.x1)
        };
        let door_cell = self.houses[house_idx]
            .entry
            .as_ref()
            .map(|e| (e.wall_x, e.wall_z));
        for x in x0..=x1 {
            if !self.houses[house_idx].contains(x, zw) {
                continue;
            }
            // Don't re-seal the outer door cell with an inner wall slab.
            if door_cell == Some((x, zw)) {
                continue;
            }
            add_wall_bit(self, x, zw, MASK_NORTH);
        }
    }

    fn stamp_vertical_inner_wall(&mut self, house_idx: usize, xw: i32) {
        let (z0, z1) = {
            let h = &self.houses[house_idx];
            (h.z0, h.z1)
        };
        let door_cell = self.houses[house_idx]
            .entry
            .as_ref()
            .map(|e| (e.wall_x, e.wall_z));
        for z in z0..=z1 {
            if !self.houses[house_idx].contains(xw, z) {
                continue;
            }
            if door_cell == Some((xw, z)) {
                continue;
            }
            add_wall_bit(self, xw, z, MASK_WEST);
        }
    }
}

fn add_wall_bit(draft: &mut MapDraft, x: i32, z: i32, bit: u8) {
    match draft.get(x, z) {
        DraftTile::Open => draft.set(x, z, DraftTile::Wall(bit)),
        DraftTile::Wall(bits) => draft.set(x, z, DraftTile::Wall(bits | bit)),
        // Concave union corner pillars and void cells stay as-is; the inner wall
        // simply has a gap at that cell.
        DraftTile::Corner(_) | DraftTile::Void => {}
    }
}

/// All bbox sub-rooms defined by `h_cuts` × `v_cuts` satisfy area + min-dim rules.
fn grid_rooms_satisfy(h_cuts: &[i32], v_cuts: &[i32]) -> bool {
    for hi in 0..h_cuts.len() - 1 {
        let h_diff = h_cuts[hi + 1] - h_cuts[hi];
        if h_diff < MIN_ROOM_DIM {
            return false;
        }
        for vi in 0..v_cuts.len() - 1 {
            let v_diff = v_cuts[vi + 1] - v_cuts[vi];
            if v_diff < MIN_ROOM_DIM {
                return false;
            }
            if h_diff * v_diff < MIN_ROOM_AREA {
                return false;
            }
        }
    }
    true
}

fn insert_sorted(v: &mut Vec<i32>, x: i32) {
    v.push(x);
    v.sort();
}
