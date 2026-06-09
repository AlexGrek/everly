//! Step 9: one or two functional doorways per house onto exterior road.

use std::collections::HashSet;

use crate::rng;

use super::draft::{DraftTile, MapDraft};
use super::house::{house_contains, house_on_perimeter, House};
use super::types::HouseEntrypoint;
use crate::map::world_map::{MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

const NEIGHBORS: [(i32, i32, u8); 4] = [
    (0, -1, MASK_NORTH),
    (0, 1, MASK_SOUTH),
    (1, 0, MASK_EAST),
    (-1, 0, MASK_WEST),
];

/// Probability of placing a second exterior door on the opposite/other wall run.
const SECOND_DOOR_PROBABILITY: f64 = 0.5;

impl MapDraft {
    /// At least one door per house; a second door is placed with [`SECOND_DOOR_PROBABILITY`].
    /// Doors are widened to 2 tiles when a valid neighbor along the wall run exists.
    pub fn step_place_house_doors(&mut self) {
        let house_count = self.houses.len();
        for index in 0..house_count {
            let Some(primary) = self.place_one_house_door(index, &HashSet::new()) else {
                continue;
            };
            self.houses[index].entry = Some(primary);

            if rng::chance_f64(&mut self.rng, SECOND_DOOR_PROBABILITY) {
                let reserved: HashSet<(i32, i32)> =
                    entrypoint_reserved_cells(self.houses[index].entry.as_ref().unwrap())
                        .into_iter()
                        .collect();
                if let Some(second) = self.place_one_house_door(index, &reserved) {
                    self.houses[index].entry2 = Some(second);
                }
            }
        }
    }

    fn place_one_house_door(
        &mut self,
        house_index: usize,
        exclude: &HashSet<(i32, i32)>,
    ) -> Option<HouseEntrypoint> {
        let mut valid = filter_door_sites(collect_valid_door_sites(self, house_index), exclude);
        if valid.is_empty() {
            valid = filter_door_sites(collect_force_door_sites(self, house_index), exclude);
        }
        if valid.is_empty() {
            return None;
        }
        let widenable: Vec<_> = valid
            .iter()
            .copied()
            .filter(|&(x, z, edge)| {
                find_wide_companion(self, house_index, x, z, edge)
                    .is_some_and(|comp| !exclude.contains(&comp))
            })
            .collect();
        let pick = if !widenable.is_empty() {
            *rng::pick(&mut self.rng, &widenable)
        } else {
            *rng::pick(&mut self.rng, &valid)
        };
        open_doorway(self, pick.0, pick.1, pick.2);
        let companion = find_wide_companion(self, house_index, pick.0, pick.1, pick.2)
            .filter(|comp| !exclude.contains(comp));
        if let Some((cx, cz)) = companion {
            open_doorway(self, cx, cz, pick.2);
        }
        let (walk_x, walk_z) = entrypoint_walk_tile(pick.0, pick.1, pick.2);
        Some(HouseEntrypoint {
            walk_x,
            walk_z,
            wall_x: pick.0,
            wall_z: pick.1,
            outward_edge: pick.2,
            wall2: companion,
        })
    }
}

/// Road tile outside the wall cell along the opened edge (chunk-local).
pub(crate) fn entrypoint_walk_tile(wall_x: i32, wall_z: i32, outward_edge: u8) -> (i32, i32) {
    let (dx, dz) = outward_delta(outward_edge);
    (wall_x + dx, wall_z + dz)
}

/// Floor tile just inside the doorway (opposite the outward road step).
pub(crate) fn entrypoint_inward_tile(wall_x: i32, wall_z: i32, outward_edge: u8) -> (i32, i32) {
    let (dx, dz) = outward_delta(outward_edge);
    (wall_x - dx, wall_z - dz)
}

/// Tiles reserved around one doorway so a second door does not overlap it.
pub(crate) fn entrypoint_reserved_cells(ep: &HouseEntrypoint) -> Vec<(i32, i32)> {
    let mut cells = vec![
        (ep.wall_x, ep.wall_z),
        entrypoint_walk_tile(ep.wall_x, ep.wall_z, ep.outward_edge),
        entrypoint_inward_tile(ep.wall_x, ep.wall_z, ep.outward_edge),
    ];
    if let Some((wx2, wz2)) = ep.wall2 {
        cells.push((wx2, wz2));
        cells.push(entrypoint_inward_tile(wx2, wz2, ep.outward_edge));
    }
    cells
}

/// Every outer door wall cell for a house (primary + optional second entry).
pub(crate) fn house_entry_wall_cells(house: &House) -> Vec<(i32, i32)> {
    let mut cells = Vec::new();
    for ep in [house.entry.as_ref(), house.entry2.as_ref()].into_iter().flatten() {
        cells.push((ep.wall_x, ep.wall_z));
        if let Some(w2) = ep.wall2 {
            cells.push(w2);
        }
    }
    cells
}

fn filter_door_sites(
    sites: Vec<(i32, i32, u8)>,
    exclude: &HashSet<(i32, i32)>,
) -> Vec<(i32, i32, u8)> {
    sites
        .into_iter()
        .filter(|&(x, z, _)| !exclude.contains(&(x, z)))
        .collect()
}

fn outward_delta(outward_edge: u8) -> (i32, i32) {
    match outward_edge {
        MASK_NORTH => (0, -1),
        MASK_SOUTH => (0, 1),
        MASK_EAST => (1, 0),
        MASK_WEST => (-1, 0),
        _ => (0, 0),
    }
}

/// True when this doorway opens onto exterior road with a clear interior floor (no L-wall traps).
pub(crate) fn is_valid_door_site(
    draft: &MapDraft,
    house_index: usize,
    wall_x: i32,
    wall_z: i32,
    outward_edge: u8,
) -> bool {
    let house = &draft.houses[house_index];
    let (walk_x, walk_z) = entrypoint_walk_tile(wall_x, wall_z, outward_edge);
    let (inward_x, inward_z) = entrypoint_inward_tile(wall_x, wall_z, outward_edge);

    if !house.contains(inward_x, inward_z) || draft.get(inward_x, inward_z) != DraftTile::Open {
        return false;
    }
    if !walk_is_exterior_road(draft, walk_x, walk_z) {
        return false;
    }
    if outward_faces_other_house_wall(draft, house_index, wall_x, wall_z, outward_edge) {
        return false;
    }
    door_cell_allows_passage(draft, wall_x, wall_z, outward_edge)
}

fn walk_is_exterior_road(draft: &MapDraft, walk_x: i32, walk_z: i32) -> bool {
    if draft.get(walk_x, walk_z) != DraftTile::Open {
        return false;
    }
    !draft
        .houses
        .iter()
        .any(|h| h.contains(walk_x, walk_z))
}

fn outward_faces_other_house_wall(
    draft: &MapDraft,
    house_index: usize,
    wall_x: i32,
    wall_z: i32,
    outward_edge: u8,
) -> bool {
    let (walk_x, walk_z) = entrypoint_walk_tile(wall_x, wall_z, outward_edge);
    for (i, other) in draft.houses.iter().enumerate() {
        if i == house_index {
            continue;
        }
        if !other.contains(walk_x, walk_z) {
            continue;
        }
        return true;
    }
    let (dx, dz) = outward_delta(outward_edge);
    let beyond_x = walk_x + dx;
    let beyond_z = walk_z + dz;
    if !draft.in_bounds(beyond_x, beyond_z) {
        return false;
    }
    matches!(
        draft.get(beyond_x, beyond_z),
        DraftTile::Wall(_) | DraftTile::Corner(_)
    ) && draft
        .houses
        .iter()
        .enumerate()
        .any(|(i, h)| i != house_index && h.contains(beyond_x, beyond_z))
}

fn door_cell_allows_passage(
    draft: &MapDraft,
    wall_x: i32,
    wall_z: i32,
    outward_edge: u8,
) -> bool {
    match draft.get(wall_x, wall_z) {
        DraftTile::Corner(_) => true,
        DraftTile::Wall(bits) => {
            if bits & outward_edge == 0 {
                return false;
            }
            let remaining = bits & !outward_edge;
            remaining.count_ones() == 0
        }
        DraftTile::Open | DraftTile::Charger(_) => true,
        DraftTile::Void => false,
    }
}

fn collect_valid_door_sites(draft: &MapDraft, house_index: usize) -> Vec<(i32, i32, u8)> {
    let house = &draft.houses[house_index];
    let mut sites = Vec::new();
    for z in house.z0..=house.z1 {
        for x in house.x0..=house.x1 {
            if !house_on_perimeter(house, x, z) {
                continue;
            }
            match draft.get(x, z) {
                DraftTile::Wall(bits) => {
                    for &(dx, dz, wall_bit) in &NEIGHBORS {
                        let nx = x + dx;
                        let nz = z + dz;
                        if house_contains(house, nx, nz) {
                            continue;
                        }
                        if draft.get(nx, nz) != DraftTile::Open {
                            continue;
                        }
                        if bits & wall_bit != 0
                            && is_valid_door_site(draft, house_index, x, z, wall_bit)
                        {
                            sites.push((x, z, wall_bit));
                        }
                    }
                }
                DraftTile::Corner(_) => {
                    for &(dx, dz, wall_bit) in &NEIGHBORS {
                        let nx = x + dx;
                        let nz = z + dz;
                        if house_contains(house, nx, nz) {
                            continue;
                        }
                        if draft.get(nx, nz) != DraftTile::Open {
                            continue;
                        }
                        if is_valid_door_site(draft, house_index, x, z, wall_bit) {
                            sites.push((x, z, wall_bit));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    sites
}

fn collect_force_door_sites(draft: &MapDraft, house_index: usize) -> Vec<(i32, i32, u8)> {
    let house = &draft.houses[house_index];
    let mut sites = Vec::new();
    for z in house.z0..=house.z1 {
        for x in house.x0..=house.x1 {
            if !house_on_perimeter(house, x, z) {
                continue;
            }
            for &(dx, dz, wall_bit) in &NEIGHBORS {
                let nx = x + dx;
                let nz = z + dz;
                if house_contains(house, nx, nz) {
                    continue;
                }
                if draft.get(nx, nz) != DraftTile::Open {
                    continue;
                }
                match draft.get(x, z) {
                    DraftTile::Wall(bits) if bits & wall_bit != 0 => {
                        if is_valid_door_site(draft, house_index, x, z, wall_bit) {
                            sites.push((x, z, wall_bit));
                        }
                    }
                    DraftTile::Corner(_) => {
                        if is_valid_door_site(draft, house_index, x, z, wall_bit) {
                            sites.push((x, z, wall_bit));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    sites
}

fn open_doorway(draft: &mut MapDraft, x: i32, z: i32, bit: u8) {
    match draft.get(x, z) {
        DraftTile::Wall(mask) => {
            let new_bits = mask & !bit;
            let next = if new_bits == 0 {
                DraftTile::Open
            } else {
                DraftTile::Wall(new_bits)
            };
            draft.set(x, z, next);
        }
        DraftTile::Corner(_) => draft.set(x, z, DraftTile::Open),
        _ => {}
    }
}

/// Returns the adjacent cell along the wall run (perpendicular to `outward_edge`) that
/// can serve as a second valid door site, making the opening 2 tiles wide.  Returns
/// `None` when no such neighbor exists (degenerate geometry → 1-wide fallback).
fn find_wide_companion(
    draft: &MapDraft,
    house_index: usize,
    wall_x: i32,
    wall_z: i32,
    outward_edge: u8,
) -> Option<(i32, i32)> {
    // Candidates run along the wall axis (the axis *not* crossed by the door opening).
    let along: &[(i32, i32)] = match outward_edge {
        MASK_NORTH | MASK_SOUTH => &[(1, 0), (-1, 0)], // N/S wall → run east-west
        MASK_EAST | MASK_WEST => &[(0, 1), (0, -1)],   // E/W wall → run north-south
        _ => return None,
    };
    for &(dx, dz) in along {
        let (nx, nz) = (wall_x + dx, wall_z + dz);
        if is_valid_door_site(draft, house_index, nx, nz, outward_edge) {
            return Some((nx, nz));
        }
    }
    None
}
