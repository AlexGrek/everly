//! Grid fill utilities — flood-fill region size (paint-bucket area) and box counts.

use std::collections::{HashSet, VecDeque};

const CARDINAL: [(i32, i32); 4] = [(0, -1), (0, 1), (1, 0), (-1, 0)];

/// 4-connected region size from `start`, counting only cells where `can_fill` is true.
///
/// Like a fill tool's affected area: returns `0` if `start` is not fillable.
pub fn flood_fill_area(
    in_bounds: impl Fn(i32, i32) -> bool,
    can_fill: impl Fn(i32, i32) -> bool,
    start: (i32, i32),
) -> i32 {
    if !in_bounds(start.0, start.1) || !can_fill(start.0, start.1) {
        return 0;
    }
    let mut queue = VecDeque::from([start]);
    let mut visited = HashSet::from([start]);
    let mut area = 0i32;

    while let Some((x, z)) = queue.pop_front() {
        area += 1;
        for &(dx, dz) in &CARDINAL {
            let nx = x + dx;
            let nz = z + dz;
            let next = (nx, nz);
            if visited.contains(&next) {
                continue;
            }
            if !in_bounds(nx, nz) || !can_fill(nx, nz) {
                continue;
            }
            visited.insert(next);
            queue.push_back(next);
        }
    }
    area
}

/// Count cells in an axis-aligned box where `in_region` is true (connectivity not required).
pub fn count_region_area(
    x0: i32,
    z0: i32,
    x1: i32,
    z1: i32,
    in_region: impl Fn(i32, i32) -> bool,
) -> i32 {
    let mut area = 0i32;
    for z in z0..=z1 {
        for x in x0..=x1 {
            if in_region(x, z) {
                area += 1;
            }
        }
    }
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flood_fill_area_counts_connected_component() {
        let grid = [
            ".....",
            ".###.",
            ".#.#.",
            ".###.",
            ".....",
        ];
        let can_fill = |x: i32, z: i32| {
            grid[z as usize]
                .chars()
                .nth(x as usize)
                .is_some_and(|c| c == '#')
        };
        let in_bounds = |x: i32, z: i32| (0..5).contains(&x) && (0..5).contains(&z);
        assert_eq!(flood_fill_area(in_bounds, can_fill, (2, 1)), 8);

        let block = ["###", "#.#", "###"];
        let block_fill = |x: i32, z: i32| block[z as usize].chars().nth(x as usize) == Some('#');
        let block_bounds = |x: i32, z: i32| (0..3).contains(&x) && (0..3).contains(&z);
        assert_eq!(flood_fill_area(block_bounds, block_fill, (0, 0)), 8);
        assert_eq!(flood_fill_area(in_bounds, can_fill, (2, 2)), 0);
    }

    #[test]
    fn count_region_area_counts_all_matching_cells_in_box() {
        let area = count_region_area(0, 0, 4, 4, |x, z| x == 2 || z == 2);
        assert_eq!(area, 9);
    }
}
