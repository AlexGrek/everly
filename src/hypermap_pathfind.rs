//! Pathfinding on [`crate::hypermap::Hypermap`] world tiles and on finite [`crate::world_map::WorldMapFloor`] grids.
//!
//! Walkability matches movement intent: only [`crate::world_map::CellType::Road`] is traversable
//! (4-neighbor grid, unit step cost). [`WorldMapFloor`](crate::world_map::WorldMapFloor) tests can
//! encode start/end with `>A` / `>B` via [`WorldMapFloor::from_ascii_with_path_markers`](crate::world_map::WorldMapFloor::from_ascii_with_path_markers).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use pathfinding::directed::astar::astar;

use crate::hypermap::Hypermap;
use crate::world_map::{CellType, MapParseError, WorldMapFloor};

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

#[inline]
pub fn world_tile_walkable(map: &Hypermap<CellType>, wx: i32, wy: i32) -> bool {
    tile_walkable(map.get(wx, wy))
}

/// Manhattan distance on the tile grid (admissible for 4-neighbors, unit cost).
#[inline]
pub fn manhattan(a: (i32, i32), b: (i32, i32)) -> u32 {
    a.0.abs_diff(b.0) + a.1.abs_diff(b.1)
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
    map: &Hypermap<CellType>,
    acc: &mut Vec<((i32, i32), u32)>,
    n: (i32, i32),
) {
    if world_tile_walkable(map, n.0, n.1) {
        acc.push((n, 1));
    }
}

fn hypermap_successors(map: &Hypermap<CellType>, pos: &(i32, i32)) -> Vec<((i32, i32), u32)> {
    let mut out = Vec::with_capacity(4);
    for n in four_neighbors(pos.0, pos.1) {
        push_neighbor_if_walkable(map, &mut out, n);
    }
    out
}

/// A* shortest path on world tile coordinates. Honors [`HypermapSearchLimits::max_expanded`].
pub fn astar_shortest_world_path(
    map: &Hypermap<CellType>,
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
    let h0 = manhattan(start, goal);
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
                let hn = manhattan(n, goal);
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

/// Bounded uniform-cost expansion from `start` (Dijkstra / A* with \(h=0\)).
pub fn explore_walkable_tiles_limited(
    map: &Hypermap<CellType>,
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
    use crate::hypermap::Hypermap;
    use crate::world_map::{CellType, WorldMapFloor};

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
        let map = Hypermap::new(CellType::Void);
        for x in 0..70 {
            map.set(x, 0, CellType::Road);
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
        let map = Hypermap::new(CellType::Road);
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
    fn explore_respects_void_default() {
        let map = Hypermap::new(CellType::Void);
        map.set(0, 0, CellType::Road);
        map.set(1, 0, CellType::Road);
        map.set(2, 0, CellType::Road);
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
