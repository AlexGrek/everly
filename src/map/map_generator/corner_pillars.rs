//! Detect `c*` corner pillars from a wall layout alone (no room / union metadata).
//!
//! 1. Scan horizontal wall runs (N/S slabs); tile before the run start and after the run end
//!    are pillar candidates.
//! 2. Scan vertical wall runs (E/W slabs) the same way along columns.
//! 3. For each candidate, if there is a perpendicular wall of the other orientation nearby,
//!    place a pillar; variant comes from both wall directions.

use std::collections::HashSet;

use crate::map::world_map::{WallCorner, MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

/// Per-cell wall mask (`None` = not a wall). Row-major: index `[z][x]`.
#[derive(Debug, Clone)]
pub struct WallField {
    pub size: i32,
    masks: Vec<Vec<Option<u8>>>,
}

impl WallField {
    pub fn new(size: i32) -> Self {
        let sz = size as usize;
        Self {
            size,
            masks: vec![vec![None; sz]; sz],
        }
    }

    pub fn set_wall(&mut self, x: i32, z: i32, mask_bits: u8) {
        if Self::in_bounds(self.size, x, z) {
            self.masks[z as usize][x as usize] = Some(mask_bits);
        }
    }

    pub fn wall_mask(&self, x: i32, z: i32) -> Option<u8> {
        if !Self::in_bounds(self.size, x, z) {
            return None;
        }
        self.masks[z as usize][x as usize]
    }

    pub fn is_wall(&self, x: i32, z: i32) -> bool {
        self.wall_mask(x, z).is_some()
    }

    fn in_bounds(size: i32, x: i32, z: i32) -> bool {
        x >= 0 && z >= 0 && x < size && z < size
    }
}

/// One interior floor cell that should become a [`WallCorner`] pillar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CornerPillarPlacement {
    pub x: i32,
    pub z: i32,
    pub corner: WallCorner,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    North,
    South,
    East,
    West,
}

/// All concave inner-corner pillar placements for this wall field.
pub fn detect_corner_pillars(field: &WallField) -> Vec<CornerPillarPlacement> {
    let exterior = exterior_reachable(field);
    let mut candidates = HashSet::new();
    collect_run_endpoint_candidates(field, &mut candidates);
    collect_interior_concave_candidates(field, &exterior, &mut candidates);
    let mut out = Vec::new();
    let mut placed = HashSet::new();
    for &(x, z) in &candidates {
        if field.is_wall(x, z) || exterior[z as usize][x as usize] {
            continue;
        }
        let Some(corner) = pillar_corner_at(field, x, z) else {
            continue;
        };
        if placed.insert((x, z)) {
            out.push(CornerPillarPlacement { x, z, corner });
        }
    }
    out
}

fn is_horizontal_wall(mask: u8) -> bool {
    mask & (MASK_NORTH | MASK_SOUTH) != 0
}

fn is_vertical_wall(mask: u8) -> bool {
    mask & (MASK_EAST | MASK_WEST) != 0
}

/// Endpoints of horizontal runs (along +X) and vertical runs (along +Z).
fn collect_run_endpoint_candidates(field: &WallField, out: &mut HashSet<(i32, i32)>) {
    let sz = field.size;
    for z in 0..sz {
        let mut x = 0;
        while x < sz {
            let Some(mask) = field.wall_mask(x, z) else {
                x += 1;
                continue;
            };
            if !is_horizontal_wall(mask) {
                x += 1;
                continue;
            }
            let x0 = x;
            x += 1;
            while x < sz {
                match field.wall_mask(x, z) {
                    Some(m) if is_horizontal_wall(m) => x += 1,
                    _ => break,
                }
            }
            let x1 = x - 1;
            try_endpoint(field, x0 - 1, z, out);
            try_endpoint(field, x1 + 1, z, out);
        }
    }
    for x in 0..sz {
        let mut z = 0;
        while z < sz {
            let Some(mask) = field.wall_mask(x, z) else {
                z += 1;
                continue;
            };
            if !is_vertical_wall(mask) {
                z += 1;
                continue;
            }
            let z0 = z;
            z += 1;
            while z < sz {
                match field.wall_mask(x, z) {
                    Some(m) if is_vertical_wall(m) => z += 1,
                    _ => break,
                }
            }
            let z1 = z - 1;
            try_endpoint(field, x, z0 - 1, out);
            try_endpoint(field, x, z1 + 1, out);
        }
    }
}

fn try_endpoint(field: &WallField, x: i32, z: i32, out: &mut HashSet<(i32, i32)>) {
    if WallField::in_bounds(field.size, x, z) && !field.is_wall(x, z) {
        out.insert((x, z));
    }
}

/// Interior floor at a re-entrant corner (not only run endpoints): two perpendicular wall
/// neighbors and open floor diagonally across the notch (skips convex room corners).
fn collect_interior_concave_candidates(
    field: &WallField,
    exterior: &[Vec<bool>],
    out: &mut HashSet<(i32, i32)>,
) {
    let sz = field.size;
    for z in 0..sz {
        for x in 0..sz {
            if field.is_wall(x, z) || exterior[z as usize][x as usize] {
                continue;
            }
            if is_concave_elbow_floor(field, exterior, x, z) {
                out.insert((x, z));
            }
        }
    }
}

fn is_concave_elbow_floor(
    field: &WallField,
    exterior: &[Vec<bool>],
    x: i32,
    z: i32,
) -> bool {
    cardinal_wall_dirs(field, x, z)
        .iter()
        .any(|&(h, v)| is_interior_notch(field, exterior, x, z, h, v))
}

fn cardinal_wall_dirs(field: &WallField, x: i32, z: i32) -> Vec<(Dir, Dir)> {
    let mut h_dirs = Vec::new();
    let mut v_dirs = Vec::new();
    for (dx, dz, dir) in [
        (0, -1, Dir::North),
        (0, 1, Dir::South),
        (-1, 0, Dir::West),
        (1, 0, Dir::East),
    ] {
        let Some(mask) = field.wall_mask(x + dx, z + dz) else {
            continue;
        };
        if is_horizontal_wall(mask) {
            h_dirs.push(dir);
        }
        if is_vertical_wall(mask) {
            v_dirs.push(dir);
        }
    }
    let mut pairs = Vec::new();
    for &h in &h_dirs {
        for &v in &v_dirs {
            if corner_from_perpendicular_dirs(h, v).is_some() {
                pairs.push((h, v));
            }
        }
    }
    pairs
}

/// Diagonal tile across the re-entrant notch must be interior floor (not a convex outer corner).
fn is_interior_notch(
    field: &WallField,
    exterior: &[Vec<bool>],
    x: i32,
    z: i32,
    h: Dir,
    v: Dir,
) -> bool {
    let Some((dx, dz)) = notch_diagonal_offset(h, v) else {
        return false;
    };
    interior_floor(field, exterior, x + dx, z + dz)
}

fn notch_diagonal_offset(h: Dir, v: Dir) -> Option<(i32, i32)> {
    use Dir::{East, North, South, West};
    match (h, v) {
        (North, West) | (West, North) => Some((-1, -1)),
        (North, East) | (East, North) => Some((1, -1)),
        (South, West) | (West, South) => Some((-1, 1)),
        (South, East) | (East, South) => Some((1, 1)),
        _ => None,
    }
}

fn interior_floor(field: &WallField, exterior: &[Vec<bool>], x: i32, z: i32) -> bool {
    !field.is_wall(x, z)
        && WallField::in_bounds(field.size, x, z)
        && !exterior[z as usize][x as usize]
}

fn pillar_corner_at(field: &WallField, x: i32, z: i32) -> Option<WallCorner> {
    cardinal_wall_dirs(field, x, z)
        .into_iter()
        .find_map(|(h, v)| corner_from_perpendicular_dirs(h, v))
}

/// Pillar sits in the cell corner where the two slab gaps meet (world XZ).
fn corner_from_perpendicular_dirs(a: Dir, b: Dir) -> Option<WallCorner> {
    use Dir::{East, North, South, West};
    match (a, b) {
        (North, West) | (West, North) => Some(WallCorner::Sw),
        (North, East) | (East, North) => Some(WallCorner::Se),
        (South, West) | (West, South) => Some(WallCorner::Nw),
        (South, East) | (East, South) => Some(WallCorner::Ne),
        (North, South) | (South, North) | (East, West) | (West, East) => None,
        _ => None,
    }
}

fn exterior_reachable(field: &WallField) -> Vec<Vec<bool>> {
    let sz = field.size as usize;
    let mut exterior = vec![vec![false; sz]; sz];
    let mut stack = Vec::new();

    let mut push = |x: i32, z: i32, stack: &mut Vec<(i32, i32)>| {
        if !WallField::in_bounds(field.size, x, z) {
            return;
        }
        let (ux, uz) = (x as usize, z as usize);
        if exterior[uz][ux] || field.is_wall(x, z) {
            return;
        }
        exterior[uz][ux] = true;
        stack.push((x, z));
    };

    for x in 0..field.size {
        push(x, 0, &mut stack);
        push(x, field.size - 1, &mut stack);
    }
    for z in 1..field.size - 1 {
        push(0, z, &mut stack);
        push(field.size - 1, z, &mut stack);
    }

    while let Some((x, z)) = stack.pop() {
        for (dx, dz) in [(0, -1), (0, 1), (-1, 0), (1, 0)] {
            push(x + dx, z + dz, &mut stack);
        }
    }

    exterior
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::world_map::{MASK_EAST, MASK_NORTH, MASK_SOUTH, MASK_WEST};

    fn union_shell_from_rooms(field: &mut WallField, rooms: &[(i32, i32, i32, i32)]) {
        let union = |x: i32, z: i32| {
            rooms
                .iter()
                .any(|&(x0, x1, z0, z1)| x >= x0 && x <= x1 && z >= z0 && z <= z1)
        };
        for z in 0..field.size {
            for x in 0..field.size {
                if !union(x, z) {
                    continue;
                }
                let mut bits = 0u8;
                if !union(x, z - 1) {
                    bits |= MASK_NORTH;
                }
                if !union(x, z + 1) {
                    bits |= MASK_SOUTH;
                }
                if !union(x - 1, z) {
                    bits |= MASK_WEST;
                }
                if !union(x + 1, z) {
                    bits |= MASK_EAST;
                }
                if bits != 0 {
                    field.set_wall(x, z, bits);
                }
            }
        }
    }

    fn l_shape_union_walls(field: &mut WallField) {
        union_shell_from_rooms(field, &[(1, 4, 1, 4), (4, 7, 4, 6)]);
    }

    fn t_shape_union_walls(field: &mut WallField) {
        union_shell_from_rooms(field, &[(2, 4, 1, 6), (3, 6, 6, 7)]);
    }

    fn assert_has_pillar(pillars: &[CornerPillarPlacement], x: i32, z: i32) {
        assert!(
            pillars.iter().any(|p| p.x == x && p.z == z),
            "expected pillar at ({x},{z}), got {pillars:?}"
        );
    }

    #[test]
    fn corner_from_perpendicular_dirs_maps_world_xz() {
        use Dir::{East, North, South, West};
        assert_eq!(
            corner_from_perpendicular_dirs(North, West),
            Some(WallCorner::Sw)
        );
        assert_eq!(
            corner_from_perpendicular_dirs(North, East),
            Some(WallCorner::Se)
        );
        assert_eq!(
            corner_from_perpendicular_dirs(South, West),
            Some(WallCorner::Nw)
        );
        assert_eq!(
            corner_from_perpendicular_dirs(South, East),
            Some(WallCorner::Ne)
        );
        assert_eq!(corner_from_perpendicular_dirs(North, South), None);
        assert_eq!(corner_from_perpendicular_dirs(East, West), None);
    }

    #[test]
    fn l_shape_gets_sw_pillar_on_elbow() {
        let mut field = WallField::new(11);
        l_shape_union_walls(&mut field);
        let pillars = detect_corner_pillars(&field);
        let elbow = pillars
            .iter()
            .find(|p| p.x == 4 && p.z == 4)
            .expect("L union concave elbow at (4,4)");
        assert_eq!(elbow.corner, WallCorner::Sw);
    }

    #[test]
    fn rectangle_shell_has_no_interior_pillars() {
        let mut field = WallField::new(11);
        for x in 2..=5 {
            for z in 2..=5 {
                let mut bits = 0u8;
                if z == 2 {
                    bits |= MASK_NORTH;
                }
                if z == 5 {
                    bits |= MASK_SOUTH;
                }
                if x == 2 {
                    bits |= MASK_WEST;
                }
                if x == 5 {
                    bits |= MASK_EAST;
                }
                if bits != 0 {
                    field.set_wall(x, z, bits);
                }
            }
        }
        assert!(detect_corner_pillars(&field).is_empty());
    }

    #[test]
    fn horizontal_run_endpoints_become_candidates() {
        let mut field = WallField::new(9);
        for x in 1..=6 {
            field.set_wall(x, 1, MASK_NORTH);
            field.set_wall(x, 6, MASK_SOUTH);
        }
        for z in 2..=5 {
            field.set_wall(1, z, MASK_WEST);
            field.set_wall(6, z, MASK_EAST);
        }
        for x in 3..=5 {
            field.set_wall(x, 3, MASK_SOUTH);
        }
        field.set_wall(2, 2, MASK_EAST);
        let pillars = detect_corner_pillars(&field);
        assert!(
            pillars.iter().any(|p| p.x == 2 && p.z == 3),
            "tile before a horizontal run with a perpendicular vertical wall gets a pillar, got {pillars:?}"
        );
    }

    #[test]
    fn ignores_exterior_road_at_run_end() {
        let mut field = WallField::new(7);
        for x in 1..=4 {
            field.set_wall(x, 1, MASK_NORTH);
            field.set_wall(x, 4, MASK_SOUTH);
        }
        for z in 2..=3 {
            field.set_wall(1, z, MASK_WEST);
            field.set_wall(4, z, MASK_EAST);
        }
        let pillars = detect_corner_pillars(&field);
        assert!(
            pillars.iter().all(|p| p.x >= 2 && p.x <= 3 && p.z >= 2 && p.z <= 3),
            "pillars on interior only: {pillars:?}"
        );
    }

    #[test]
    fn no_duplicate_pillar_cells() {
        let mut field = WallField::new(11);
        l_shape_union_walls(&mut field);
        let pillars = detect_corner_pillars(&field);
        let mut cells = HashSet::new();
        for p in &pillars {
            assert!(cells.insert((p.x, p.z)));
        }
    }

    #[test]
    fn t_shape_gets_pillars_at_stem_bar_junction() {
        let mut field = WallField::new(11);
        t_shape_union_walls(&mut field);
        let pillars = detect_corner_pillars(&field);
        assert_has_pillar(&pillars, 3, 6);
        assert_has_pillar(&pillars, 4, 6);
    }

    #[test]
    fn single_cell_horizontal_run_endpoint_gets_pillar() {
        let mut field = WallField::new(9);
        for x in 1..=6 {
            field.set_wall(x, 1, MASK_NORTH);
            field.set_wall(x, 6, MASK_SOUTH);
        }
        for z in 2..=5 {
            field.set_wall(1, z, MASK_WEST);
            field.set_wall(6, z, MASK_EAST);
        }
        field.set_wall(3, 3, MASK_SOUTH);
        field.set_wall(2, 2, MASK_EAST);
        let pillars = detect_corner_pillars(&field);
        assert_has_pillar(&pillars, 2, 3);
    }

    #[test]
    fn single_cell_vertical_run_endpoint_gets_pillar() {
        let mut field = WallField::new(9);
        for x in 1..=6 {
            field.set_wall(x, 1, MASK_NORTH);
            field.set_wall(x, 6, MASK_SOUTH);
        }
        for z in 2..=5 {
            field.set_wall(1, z, MASK_WEST);
            field.set_wall(6, z, MASK_EAST);
        }
        field.set_wall(3, 3, MASK_EAST);
        field.set_wall(2, 2, MASK_NORTH);
        let pillars = detect_corner_pillars(&field);
        assert_has_pillar(&pillars, 3, 2);
    }

    #[test]
    fn u_shape_gets_two_inner_pillars() {
        let mut field = WallField::new(13);
        union_shell_from_rooms(&mut field, &[(2, 6, 2, 8), (2, 4, 5, 6), (6, 8, 5, 6)]);
        let pillars = detect_corner_pillars(&field);
        assert_has_pillar(&pillars, 6, 5);
        assert_has_pillar(&pillars, 6, 6);
    }

    #[test]
    fn multi_bit_corner_wall_next_to_elbow_still_gets_pillar() {
        let mut field = WallField::new(9);
        for x in 1..=6 {
            field.set_wall(x, 1, MASK_NORTH);
            field.set_wall(x, 6, MASK_SOUTH);
        }
        for z in 2..=5 {
            field.set_wall(1, z, MASK_WEST);
            field.set_wall(6, z, MASK_EAST);
        }
        field.set_wall(3, 3, MASK_SOUTH | MASK_WEST);
        field.set_wall(2, 2, MASK_EAST);
        let pillars = detect_corner_pillars(&field);
        assert_has_pillar(&pillars, 2, 3);
    }

    #[test]
    fn procedural_maps_often_need_inner_corner_pillars() {
        use crate::map::map_generator::{MapDraft, MapGeneratorConfig};
        use crate::map::world_map::CellType;

        let mut with_corners = 0u32;
        for seed in 0..256u64 {
            let cells = MapDraft::generate(MapGeneratorConfig {
                seed,
                ..Default::default()
            });
            let corners = cells
                .iter()
                .flatten()
                .filter(|c| matches!(c, CellType::Corner(_)))
                .count();
            if corners > 0 {
                with_corners += 1;
            }
        }
        assert!(
            with_corners > 20,
            "expected many procedural seeds to place corner pillars (got {with_corners}/256)"
        );
    }
}
