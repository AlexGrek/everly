//! Step 10: per-house floor waves — marble from main entry, glass from house center.

use std::collections::{HashSet, VecDeque};

use rand::Rng;

use super::draft::{DraftTile, MapDraft};
use super::house::house_contains;
use super::house::House;
use super::step_door::{entrypoint_inward_tile, entrypoint_walk_tile};
use super::step_seeds::manhattan;
use super::types::HouseEntrypoint;
use crate::map::world_map::TileStyle;

/// Minimum Manhattan wave radius per house (inclusive).
pub const HOME_CRAWLER_WAVE_MIN: i32 = 3;
/// Maximum Manhattan wave radius per house (inclusive).
pub const HOME_CRAWLER_WAVE_MAX: i32 = 5;

const CARDINAL: [(i32, i32); 4] = [(0, -1), (0, 1), (1, 0), (-1, 0)];

impl MapDraft {
    /// Marble wave from main entry, then glass wave from virtual house center.
    pub fn step_home_crawlers(&mut self) {
        let houses: Vec<House> = self.houses.clone();
        for house in houses {
            if let Some(ref ep) = house.entry {
                if let Some(start) = house_entry_interior_tile(self, ep) {
                    if house.contains(start.0, start.1) {
                        wave_house(self, &house, start, TileStyle::FLOOR_MARBLE);
                    }
                }
            }
            if house.supports_center_glass_wave() {
                if let Some(center) = house_center_floor_tile(self, &house) {
                    wave_house(self, &house, center, TileStyle::FLOOR_GLASS);
                }
            }
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

/// Open floor at the integer center of the house bounds (or closest open tile to it).
pub(crate) fn house_center_floor_tile(draft: &MapDraft, house: &House) -> Option<(i32, i32)> {
    let center = house.center();
    if house.contains(center.0, center.1) && draft.get(center.0, center.1) == DraftTile::Open {
        return Some(center);
    }
    closest_open_in_house(draft, house, center)
}

fn closest_open_in_house(
    draft: &MapDraft,
    house: &House,
    target: (i32, i32),
) -> Option<(i32, i32)> {
    let mut best: Option<((i32, i32), i32)> = None;
    for z in house.z0..=house.z1 {
        for x in house.x0..=house.x1 {
            if !house.contains(x, z) || draft.get(x, z) != DraftTile::Open {
                continue;
            }
            let d = manhattan((x, z), target);
            if best.is_none_or(|(_, best_d)| d < best_d) {
                best = Some(((x, z), d));
            }
        }
    }
    best.map(|(tile, _)| tile)
}

fn wave_house(draft: &mut MapDraft, house: &House, start: (i32, i32), style: TileStyle) {
    if draft.get(start.0, start.1) != DraftTile::Open {
        return;
    }
    let max_dist = draft
        .rng
        .gen_range(HOME_CRAWLER_WAVE_MIN..=HOME_CRAWLER_WAVE_MAX);
    let mut queue = VecDeque::from([(start.0, start.1, 0)]);
    let mut visited = HashSet::from([start]);

    while let Some((x, z, dist)) = queue.pop_front() {
        stamp_floor_style(draft, x, z, style);
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

fn stamp_floor_style(draft: &mut MapDraft, x: i32, z: i32, style: TileStyle) {
    if draft.get(x, z) == DraftTile::Open {
        draft.set_floor_style(x, z, style);
    }
}
