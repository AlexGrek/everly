//! Pathfinding on the world's [`HypermapRuntime::static_passability_map`](crate::map::hypermap_world::HypermapRuntime::static_passability_map)
//! and on finite [`crate::map::world_map::WorldMapFloor`] grids.
//!
//! World-tile pathfinding consumes a `Hypermap<f32>` of static passability
//! (see [`crate::map::world_map::cell_passability`]) — a tile is walkable iff
//! its passability is `> 0.0`. 4-neighbor grid (no diagonals), unit step cost.
//!
//! The A* heuristic uses a **line-pull tie-breaker**: among nodes with equal
//! Manhattan distance to the goal, those closest to the straight line from
//! start to goal are expanded first. This produces a Bresenham-like staircase
//! through open space rather than the L-shape that pure Manhattan yields.
//!
//! [`simplify_path_line_of_sight`] runs a greedy string-pulling pass over the
//! returned tile path: interior tiles whose neighbours have line-of-sight (the
//! Bresenham line between them passes only through walkable tiles) are
//! discarded. The result is a sparse waypoint list where intermediate nodes
//! exist only where the path must bend around an obstacle.
//!
//! [`WorldMapFloor`](crate::map::world_map::WorldMapFloor) tests can encode
//! start/end with `>A` / `>B` via
//! [`WorldMapFloor::from_ascii_with_path_markers`](crate::map::world_map::WorldMapFloor::from_ascii_with_path_markers).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use pathfinding::directed::astar::astar;

use crate::map::hypermap::Hypermap;
use crate::map::world_map::{CellType, MapParseError, WorldMapFloor};

/// Stops expanding the open set after this many **pop** operations (tile expansions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HypermapSearchLimits {
    pub max_expanded: usize,
}

impl Default for HypermapSearchLimits {
    fn default() -> Self {
        Self {
            max_expanded: 50_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HypermapPathResult {
    /// Shortest 4-neighbor path in tile steps (includes start and goal).
    Found { path: Vec<(i32, i32)>, expansions: usize },
    NoPath { expansions: usize },
    /// Search stopped before proving optimality or exhaustively closing the goal component.
    LimitExceeded { expansions: usize },
}

/// Uniform-cost frontier exploration (equivalent to A* with zero heuristic), bounded by [`HypermapSearchLimits`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HypermapExploreResult {
    Visited { tiles: Vec<(i32, i32)>, expansions: usize },
    LimitExceeded { tiles: Vec<(i32, i32)>, expansions: usize },
}

#[inline]
pub fn tile_walkable(cell: CellType) -> bool {
    matches!(cell, CellType::Road)
}

/// Walkability predicate for a static-passability sample (see
/// [`crate::map::world_map::cell_passability`]). `> 0.0` walkable, `0.0` blocked.
#[inline]
pub fn passability_walkable(p: f32) -> bool {
    p > 0.0
}

/// Walkability for a world tile read from the static-passability hypermap.
#[inline]
pub fn world_tile_walkable(map: &Hypermap<f32>, wx: i32, wy: i32) -> bool {
    passability_walkable(map.get(wx, wy))
}

/// Manhattan distance on the tile grid (admissible for 4-neighbors, unit cost).
#[inline]
pub fn manhattan(a: (i32, i32), b: (i32, i32)) -> u32 {
    a.0.abs_diff(b.0) + a.1.abs_diff(b.1)
}

/// Each grid step costs this many scaled units. Leaves headroom for an additive
/// line-pull tie-breaker below the step cost.
const STEP_SCALE: u32 = 1024;

/// Twice the signed area of the triangle (start, goal, node), absolute value.
///
/// Proportional to the perpendicular distance from `node` to the start→goal
/// line. Clamped so the bias is strictly less than `STEP_SCALE`, guaranteeing
/// that adding it to a Manhattan estimate never reorders nodes whose Manhattan
/// distance to the goal differs by 1.
#[inline]
fn line_pull_bias(start: (i32, i32), goal: (i32, i32), node: (i32, i32)) -> u32 {
    let dx1 = (node.0 - goal.0) as i64;
    let dy1 = (node.1 - goal.1) as i64;
    let dx2 = (start.0 - goal.0) as i64;
    let dy2 = (start.1 - goal.1) as i64;
    let cross = (dx1 * dy2 - dx2 * dy1).unsigned_abs();
    cross.min((STEP_SCALE - 1) as u64) as u32
}

#[inline]
fn scaled_h(node: (i32, i32), goal: (i32, i32), start: (i32, i32)) -> u32 {
    manhattan(node, goal)
        .saturating_mul(STEP_SCALE)
        .saturating_add(line_pull_bias(start, goal, node))
}

fn four_neighbors(wx: i32, wy: i32) -> [(i32, i32); 4] {
    [
        (wx + 1, wy),
        (wx - 1, wy),
        (wx, wy + 1),
        (wx, wy - 1),
    ]
}

fn push_neighbor_if_walkable(
    map: &Hypermap<f32>,
    acc: &mut Vec<((i32, i32), u32)>,
    n: (i32, i32),
) {
    if world_tile_walkable(map, n.0, n.1) {
        acc.push((n, STEP_SCALE));
    }
}

fn hypermap_successors(map: &Hypermap<f32>, pos: &(i32, i32)) -> Vec<((i32, i32), u32)> {
    let mut out = Vec::with_capacity(4);
    for n in four_neighbors(pos.0, pos.1) {
        push_neighbor_if_walkable(map, &mut out, n);
    }
    out
}

/// A* shortest path on world tile coordinates. Honors [`HypermapSearchLimits::max_expanded`].
pub fn astar_shortest_world_path(
    map: &Hypermap<f32>,
    start: (i32, i32),
    goal: (i32, i32),
    limits: HypermapSearchLimits,
) -> HypermapPathResult {
    if !world_tile_walkable(map, start.0, start.1) || !world_tile_walkable(map, goal.0, goal.1) {
        return HypermapPathResult::NoPath { expansions: 0 };
    }
    if start == goal {
        return HypermapPathResult::Found {
            path: vec![start],
            expansions: 0,
        };
    }

    #[derive(Clone, Eq, PartialEq)]
    struct Node {
        pos: (i32, i32),
        g: u32,
        f: u32,
    }

    impl Ord for Node {
        fn cmp(&self, other: &Self) -> Ordering {
            other.f.cmp(&self.f).then_with(|| other.g.cmp(&self.g))
        }
    }

    impl PartialOrd for Node {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut open = BinaryHeap::new();
    let mut g_best: HashMap<(i32, i32), u32> = HashMap::new();
    let mut parent: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
    let h0 = scaled_h(start, goal, start);
    open.push(Node {
        pos: start,
        g: 0,
        f: h0,
    });
    g_best.insert(start, 0);

    let mut expansions = 0usize;

    while let Some(Node { pos, g, f: _ }) = open.pop() {
        let known = g_best.get(&pos).copied().unwrap_or(u32::MAX);
        if g > known {
            continue;
        }
        expansions += 1;
        if expansions > limits.max_expanded {
            return HypermapPathResult::LimitExceeded { expansions };
        }
        if pos == goal {
            let mut path = vec![goal];
            let mut cur = goal;
            while cur != start {
                let Some(&p) = parent.get(&cur) else {
                    break;
                };
                path.push(p);
                cur = p;
            }
            path.reverse();
            return HypermapPathResult::Found { path, expansions };
        }

        for (n, step) in hypermap_successors(map, &pos) {
            let ng = g.saturating_add(step);
            let better = match g_best.get(&n) {
                None => true,
                Some(&old) => ng < old,
            };
            if better {
                g_best.insert(n, ng);
                parent.insert(n, pos);
                let hn = scaled_h(n, goal, start);
                open.push(Node {
                    pos: n,
                    g: ng,
                    f: ng.saturating_add(hn),
                });
            }
        }
    }

    HypermapPathResult::NoPath { expansions }
}

/// Walks the integer Bresenham line from `a` to `b` and returns true iff every
/// tile on that line (including both endpoints) is walkable per `map`.
///
/// Used by [`simplify_path_line_of_sight`] to test whether two waypoints can
/// be joined by a straight floating-point trajectory without crossing a wall.
pub fn line_of_sight(map: &Hypermap<f32>, a: (i32, i32), b: (i32, i32)) -> bool {
    let (mut x, mut y) = a;
    let (x1, y1) = b;
    let dx = (x1 - x).abs();
    let dy = -(y1 - y).abs();
    let sx = if x < x1 { 1 } else { -1 };
    let sy = if y < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if !world_tile_walkable(map, x, y) {
            return false;
        }
        if (x, y) == b {
            return true;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Greedy string-pulling on a 4-neighbor tile path.
///
/// Keeps the start, the goal, and only those interior waypoints where the
/// path must bend around an obstacle (i.e. the previous anchor cannot reach
/// the next tile by line-of-sight). Tiles in open space are dropped so the
/// follower moves diagonally in floating-point coordinates rather than
/// stair-stepping along the grid.
///
/// `corner_buffer` keeps that many extra tiles on **each side** of every
/// detected bend (clamped to the path bounds). The tile-level line-of-sight
/// test ignores actor radius, so a single bend waypoint can leave a wide
/// follower hugging a wall too tightly to make the turn; a small buffer
/// (typically `1`) gives the follower axis-aligned approach and departure
/// tiles around each corner. When `corner_buffer > 0`, a second string-pull
/// pass with zero buffer collapses redundant buffered tiles while keeping
/// bends that still require a detour.
pub fn simplify_path_line_of_sight(
    map: &Hypermap<f32>,
    path: &[(i32, i32)],
    corner_buffer: usize,
) -> Vec<(i32, i32)> {
    let buffered = simplify_path_line_of_sight_pass(map, path, corner_buffer);
    if corner_buffer == 0 || buffered.len() <= 2 {
        buffered
    } else {
        simplify_path_line_of_sight_pass(map, &buffered, 0)
    }
}

fn simplify_path_line_of_sight_pass(
    map: &Hypermap<f32>,
    path: &[(i32, i32)],
    corner_buffer: usize,
) -> Vec<(i32, i32)> {
    if path.len() <= 2 {
        return path.to_vec();
    }

    let mut bend_indices = Vec::new();
    let mut anchor = 0;
    let mut i = 1;
    while i + 1 < path.len() {
        if line_of_sight(map, path[anchor], path[i + 1]) {
            i += 1;
        } else {
            bend_indices.push(i);
            anchor = i;
            i += 1;
        }
    }

    let last = path.len() - 1;
    let mut keep = vec![false; path.len()];
    keep[0] = true;
    keep[last] = true;
    for &b in &bend_indices {
        let lo = b.saturating_sub(corner_buffer);
        let hi = (b + corner_buffer).min(last);
        for k in lo..=hi {
            keep[k] = true;
        }
    }

    keep.iter()
        .enumerate()
        .filter_map(|(idx, k)| if *k { Some(path[idx]) } else { None })
        .collect()
}

/// Bounded uniform-cost expansion from `start` (Dijkstra / A* with \(h=0\)).
pub fn explore_walkable_tiles_limited(
    map: &Hypermap<f32>,
    start: (i32, i32),
    limits: HypermapSearchLimits,
) -> HypermapExploreResult {
    if !world_tile_walkable(map, start.0, start.1) {
        return HypermapExploreResult::Visited {
            tiles: vec![],
            expansions: 0,
        };
    }

    #[derive(Clone, Eq, PartialEq)]
    struct Node {
        pos: (i32, i32),
        g: u32,
    }

    impl Ord for Node {
        fn cmp(&self, other: &Self) -> Ordering {
            other.g.cmp(&self.g).then_with(|| other.pos.cmp(&self.pos))
        }
    }

    impl PartialOrd for Node {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut open = BinaryHeap::new();
    let mut g_best: HashMap<(i32, i32), u32> = HashMap::new();
    let mut visited_order = Vec::new();
    open.push(Node { pos: start, g: 0 });
    g_best.insert(start, 0);

    let mut expansions = 0usize;

    while let Some(Node { pos, g }) = open.pop() {
        let known = g_best.get(&pos).copied().unwrap_or(u32::MAX);
        if g > known {
            continue;
        }
        expansions += 1;
        if expansions > limits.max_expanded {
            return HypermapExploreResult::LimitExceeded {
                tiles: visited_order,
                expansions,
            };
        }
        visited_order.push(pos);

        for (n, step) in hypermap_successors(map, &pos) {
            let ng = g.saturating_add(step);
            let better = match g_best.get(&n) {
                None => true,
                Some(&old) => ng < old,
            };
            if better {
                g_best.insert(n, ng);
                open.push(Node { pos: n, g: ng });
            }
        }
    }

    HypermapExploreResult::Visited {
        tiles: visited_order,
        expansions,
    }
}

fn floor_walkable(floor: &WorldMapFloor, x: usize, y: usize) -> bool {
    floor
        .get(x, y)
        .is_some_and(|c| tile_walkable(c.get_cell_type()))
}

fn floor_successors(floor: &WorldMapFloor, pos: &(usize, usize)) -> Vec<((usize, usize), u32)> {
    let w = floor.width();
    let h = floor.height();
    let (x, y) = *pos;
    let mut out = Vec::with_capacity(4);
    if x + 1 < w && floor_walkable(floor, x + 1, y) {
        out.push(((x + 1, y), 1));
    }
    if x > 0 && floor_walkable(floor, x - 1, y) {
        out.push(((x - 1, y), 1));
    }
    if y + 1 < h && floor_walkable(floor, x, y + 1) {
        out.push(((x, y + 1), 1));
    }
    if y > 0 && floor_walkable(floor, x, y - 1) {
        out.push(((x, y - 1), 1));
    }
    out
}

/// Shortest 4-neighbor path on a parsed floor (library A*). Returns `None` if unreachable.
pub fn shortest_path_on_floor(
    floor: &WorldMapFloor,
    start: (usize, usize),
    goal: (usize, usize),
) -> Option<Vec<(usize, usize)>> {
    if !floor_walkable(floor, start.0, start.1) || !floor_walkable(floor, goal.0, goal.1) {
        return None;
    }
    astar(
        &start,
        |p| floor_successors(floor, p),
        |p| {
            (p.0.abs_diff(goal.0) + p.1.abs_diff(goal.1))
                .try_into()
                .unwrap_or(u32::MAX)
        },
        |p| *p == goal,
    )
    .map(|(path, _)| path)
}

/// Convenience: parse a text map with `>A` / `>B`, run [`shortest_path_on_floor`], and assert both markers exist.
pub fn shortest_path_from_ascii_markers(ascii: &str) -> Result<Option<Vec<(usize, usize)>>, MapParseError> {
    let (floor, markers) = WorldMapFloor::from_ascii_with_path_markers(ascii)?;
    let Some(a) = markers.path_a else {
        return Ok(None);
    };
    let Some(b) = markers.path_b else {
        return Ok(None);
    };
    Ok(shortest_path_on_floor(&floor, a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::hypermap::Hypermap;
    use crate::map::world_map::WorldMapFloor;

    #[test]
    fn floor_astar_straight_line() {
        let (floor, markers) = WorldMapFloor::from_ascii_with_path_markers(
            "\
            >A____>B\n\
            ",
        )
        .expect("parse");
        let a = markers.path_a.expect("A");
        let b = markers.path_b.expect("B");
        let path = shortest_path_on_floor(&floor, a, b).expect("path");
        assert_eq!(path.len(), 4);
        assert_eq!(path.first().copied(), Some(a));
        assert_eq!(path.last().copied(), Some(b));
    }

    #[test]
    fn floor_astar_around_wall() {
        let (floor, markers) = WorldMapFloor::from_ascii_with_path_markers(
            "\
            >Awnwn\n\
            ____>B\n\
            ",
        )
        .expect("parse");
        let path = shortest_path_on_floor(&floor, markers.path_a.unwrap(), markers.path_b.unwrap()).expect("path");
        assert!(
            path.len() > 3,
            "detour along row 1 should beat blocked straight east on row 0"
        );
    }

    #[test]
    fn hypermap_crosses_chunk_boundary() {
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..70 {
            map.set(x, 0, 1.0);
        }
        let r = astar_shortest_world_path(
            &map,
            (0, 0),
            (69, 0),
            HypermapSearchLimits {
                max_expanded: 100_000,
            },
        );
        match r {
            HypermapPathResult::Found { path, .. } => {
                assert_eq!(path.first().copied(), Some((0, 0)));
                assert_eq!(path.last().copied(), Some((69, 0)));
                assert_eq!(path.len(), 70);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn hypermap_limit_triggers() {
        let map: Hypermap<f32> = Hypermap::new(1.0);
        let r = astar_shortest_world_path(
            &map,
            (0, 0),
            (1000, 0),
            HypermapSearchLimits { max_expanded: 20 },
        );
        assert!(matches!(r, HypermapPathResult::LimitExceeded { .. }));
    }

    #[test]
    fn shortest_path_from_ascii_markers_helper() {
        let path = shortest_path_from_ascii_markers(">A____>B\n")
            .expect("parse")
            .expect("both markers");
        assert_eq!(path.len(), 4);
    }

    #[test]
    fn simplify_collapses_open_space() {
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..10 {
            map.set(x, 0, 1.0);
        }
        let raw = match astar_shortest_world_path(
            &map,
            (0, 0),
            (9, 0),
            HypermapSearchLimits::default(),
        ) {
            HypermapPathResult::Found { path, .. } => path,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(raw.len(), 10);
        let simplified = simplify_path_line_of_sight(&map, &raw, 0);
        assert_eq!(simplified, vec![(0, 0), (9, 0)]);
    }

    #[test]
    fn simplify_keeps_corner_around_wall() {
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..5 {
            map.set(x, 0, 1.0);
        }
        for y in 0..3 {
            map.set(4, y, 1.0);
        }
        for x in 0..5 {
            map.set(x, 2, 1.0);
        }
        let raw = match astar_shortest_world_path(
            &map,
            (0, 0),
            (0, 2),
            HypermapSearchLimits::default(),
        ) {
            HypermapPathResult::Found { path, .. } => path,
            other => panic!("expected Found, got {other:?}"),
        };
        let simplified = simplify_path_line_of_sight(&map, &raw, 0);
        assert_eq!(simplified.first().copied(), Some((0, 0)));
        assert_eq!(simplified.last().copied(), Some((0, 2)));
        assert!(
            simplified.len() >= 3,
            "expected at least one corner waypoint, got {simplified:?}",
        );

        let buffered = simplify_path_line_of_sight(&map, &raw, 1);
        assert!(
            simplified.len() >= 3,
            "corner path must keep at least one bend: {simplified:?}",
        );
        assert!(
            buffered.len() <= simplified.len() + 2,
            "buffered path should not grow much after the second string-pull: {buffered:?}",
        );
        assert!(
            buffered.len() >= 3,
            "buffered path must still bend around the wall: {buffered:?}",
        );
    }

    #[test]
    fn line_of_sight_detects_wall() {
        let map: Hypermap<f32> = Hypermap::new(0.0);
        map.set(0, 0, 1.0);
        map.set(2, 0, 1.0);
        assert!(!line_of_sight(&map, (0, 0), (2, 0)));
        map.set(1, 0, 1.0);
        assert!(line_of_sight(&map, (0, 0), (2, 0)));
    }

    #[test]
    fn explore_respects_void_default() {
        let map: Hypermap<f32> = Hypermap::new(0.0);
        map.set(0, 0, 1.0);
        map.set(1, 0, 1.0);
        map.set(2, 0, 1.0);
        let r = explore_walkable_tiles_limited(
            &map,
            (0, 0),
            HypermapSearchLimits { max_expanded: 10 },
        );
        match r {
            HypermapExploreResult::Visited { tiles, .. } => {
                assert_eq!(tiles.len(), 3);
            }
            other => panic!("expected Visited, got {other:?}"),
        }
    }
}
