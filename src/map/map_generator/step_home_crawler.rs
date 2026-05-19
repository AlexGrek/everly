//! Step 10: per-house marble wave (BFS) from main entry up to a random Manhattan radius.

use std::collections::{HashSet, VecDeque};

use rand::Rng;

use super::draft::{DraftTile, MapDraft};
use super::house::house_contains;
use super::house::House;
use super::step_door::{entrypoint_inward_tile, entrypoint_walk_tile};
use super::types::HouseEntrypoint;
use crate::map::world_map::TileStyle;

/// Minimum Manhattan wave radius per house (inclusive).
pub const HOME_CRAWLER_WAVE_MIN: i32 = 3;
/// Maximum Manhattan wave radius per house (inclusive).
pub const HOME_CRAWLER_WAVE_MAX: i32 = 5;

const CARDINAL: [(i32, i32); 4] = [(0, -1), (0, 1), (1, 0), (-1, 0)];

impl MapDraft {
    /// One flood-fill per house from its main entry; only styles open floor inside the footprint.
    pub fn step_home_crawlers(&mut self) {
        let houses: Vec<House> = self.houses.clone();
        for house in houses {
            let Some(ref ep) = house.entry else {
                continue;
            };
            let Some(start) = house_entry_interior_tile(self, ep) else {
                continue;
            };
            if !house.contains(start.0, start.1) {
                continue;
            }
            wave_house(self, &house, start);
        }
    }
}

/// Open floor at the house main doorway (never the exterior road walk tile).
pub(crate) fn house_entry_interior_tile(
    draft: &MapDraft,
    ep: &HouseEntrypoint,
) -> Option<(i32, i32)> {
    let inward = entrypoint_inward_tile(ep.wall_x, ep.wall_z, ep.outward_edge);
    if draft.get(inward.0, inward.1) == DraftTile::Open {
        return Some(inward);
    }
    if draft.get(ep.wall_x, ep.wall_z) == DraftTile::Open {
        return Some((ep.wall_x, ep.wall_z));
    }
    for &(dx, dz) in &CARDINAL {
        let x = ep.wall_x + dx;
        let z = ep.wall_z + dz;
        if draft.get(x, z) == DraftTile::Open {
            return Some((x, z));
        }
    }
    let (wx, wz) = entrypoint_walk_tile(ep.wall_x, ep.wall_z, ep.outward_edge);
    for &(dx, dz) in &CARDINAL {
        let x = wx + dx;
        let z = wz + dz;
        if draft.get(x, z) == DraftTile::Open {
            return Some((x, z));
        }
    }
    None
}

fn wave_house(draft: &mut MapDraft, house: &House, start: (i32, i32)) {
    if draft.get(start.0, start.1) != DraftTile::Open {
        return;
    }
    let max_dist = draft
        .rng
        .gen_range(HOME_CRAWLER_WAVE_MIN..=HOME_CRAWLER_WAVE_MAX);
    let mut queue = VecDeque::from([(start.0, start.1, 0)]);
    let mut visited = HashSet::from([start]);

    while let Some((x, z, dist)) = queue.pop_front() {
        stamp_marble_floor(draft, x, z);
        if dist >= max_dist {
            continue;
        }
        for &(dx, dz) in &CARDINAL {
            let nx = x + dx;
            let nz = z + dz;
            let next = (nx, nz);
            if visited.contains(&next) {
                continue;
            }
            if !house_contains(house, nx, nz) || draft.get(nx, nz) != DraftTile::Open {
                continue;
            }
            visited.insert(next);
            queue.push_back((nx, nz, dist + 1));
        }
    }
}

fn stamp_marble_floor(draft: &mut MapDraft, x: i32, z: i32) {
    if draft.get(x, z) == DraftTile::Open {
        draft.set_floor_style(x, z, TileStyle::FLOOR_MARBLE);
    }
}
