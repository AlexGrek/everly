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
//! Bresenham line between them passes only through walkable tiles, and never
//! squeezes diagonally between two wall corners) are
//! discarded. The result is a sparse waypoint list where intermediate nodes
//! exist only where the path must bend around an obstacle. A final pass keeps
//! the straight approach/exit tiles on either side of every one-tile doorway so
//! a wide follower threads the gap head-on instead of clipping the frame.
//!
//! [`WorldMapFloor`](crate::map::world_map::WorldMapFloor) tests can encode
//! start/end with `>A` / `>B` via
//! [`WorldMapFloor::from_ascii_with_path_markers`](crate::map::world_map::WorldMapFloor::from_ascii_with_path_markers).

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use bevy::math::IVec2;
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
/// Diagonal steps additionally require both orthogonal "shoulder" tiles to be
/// walkable: a line that slips between two diagonally-touching wall cells is
/// **not** line-of-sight, because a follower with real width clips the corner.
/// This corner-cutting guard is what forces string-pulling to keep a waypoint
/// on the outside of a convex wall corner instead of collapsing it.
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
        let step_x = e2 >= dy;
        let step_y = e2 <= dx;
        // A simultaneous diagonal step slips between the two "shoulder" tiles
        // `(x + sx, y)` and `(x, y + sy)`. A follower with real width would clip
        // a wall corner squeezing through that gap, so a blocked shoulder means
        // no line of sight. This stops string-pulling from cutting the outside
        // of a convex wall corner and leaving a wide actor stuck against it.
        if step_x
            && step_y
            && (!world_tile_walkable(map, x + sx, y) || !world_tile_walkable(map, x, y + sy))
        {
            return false;
        }
        if step_x {
            err += dy;
            x += sx;
        }
        if step_y {
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
///
/// A final pass re-inserts the tile on each side of every **doorway** the path
/// threads (a 1- or 2-tile-wide gap in a wall, see [`is_doorway_tile`]). A door
/// is not a bend, so string-pulling would otherwise collapse the straight approach
/// and let the follower cross the gap on a diagonal — clipping the doorframe. The
/// extra approach/exit tiles funnel a wide actor through the opening head-on.
pub fn simplify_path_line_of_sight(
    map: &Hypermap<f32>,
    path: &[(i32, i32)],
    corner_buffer: usize,
) -> Vec<(i32, i32)> {
    let buffered = simplify_path_line_of_sight_pass(map, path, corner_buffer);
    let simplified = if corner_buffer == 0 || buffered.len() <= 2 {
        buffered
    } else {
        simplify_path_line_of_sight_pass(map, &buffered, 0)
    };
    reinsert_doorway_approaches(map, path, simplified)
}

/// True when `(x, y)` is part of a doorway: a 1- or 2-tile-wide walkable gap in an
/// otherwise solid wall, flanked by walls on both ends of the band and open space on
/// the crossing axis.
///
/// For a 1-wide gap both immediate wall-axis neighbours are blocked; the crossing-axis
/// neighbours are open and widen out (not pinched the same way), so genuine doorways
/// stay distinct from straight single-tile corridors.
///
/// For a 2-wide gap the tile itself has one walkable wall-axis neighbour (its partner)
/// and one blocked one; both crossing-axis neighbours are open; and the space beyond
/// both ends of the 2-tile band is blocked — preventing a wide corridor from matching.
fn is_doorway_tile(map: &Hypermap<f32>, x: i32, y: i32) -> bool {
    if !world_tile_walkable(map, x, y) {
        return false;
    }
    let walk = |dx: i32, dy: i32| world_tile_walkable(map, x + dx, y + dy);

    // --- 1-wide gap: walls run N-S, gap crossed E-W ---
    let ns_walls_1 = !walk(0, 1) && !walk(0, -1);
    let ew_open = walk(1, 0) && walk(-1, 0);
    if ns_walls_1 && ew_open {
        let widens = |nx: i32| world_tile_walkable(map, nx, y + 1) || world_tile_walkable(map, nx, y - 1);
        return widens(x + 1) && widens(x - 1);
    }

    // --- 1-wide gap: walls run E-W, gap crossed N-S ---
    let ew_walls_1 = !walk(1, 0) && !walk(-1, 0);
    let ns_open = walk(0, 1) && walk(0, -1);
    if ew_walls_1 && ns_open {
        let widens = |ny: i32| world_tile_walkable(map, x + 1, ny) || world_tile_walkable(map, x - 1, ny);
        return widens(y + 1) && widens(y - 1);
    }

    // --- 2-wide gap: walls run N-S (along Y), gap crossed E-W (along X) ---
    // The 2-tile band occupies this tile (y) and its partner (y ± 1).
    // Both ends of the band are bounded by wall cells; the EW crossing is open.
    for partner_dz in [1i32, -1i32] {
        if world_tile_walkable(map, x, y + partner_dz)          // partner walkable
            && !world_tile_walkable(map, x, y - partner_dz)     // near end blocked
            && !world_tile_walkable(map, x, y + partner_dz * 2) // far end blocked
            && walk(1, 0) && walk(-1, 0)                        // crossing axis open
        {
            // Widening guard: at each EW crossing neighbor, space beyond the band ends
            // must exist — distinguishes a doorway (opening into a room) from a 2-wide
            // edge slab (corridor wall with no room on either end of the band).
            let widens = |nx: i32| {
                world_tile_walkable(map, nx, y - partner_dz)        // beyond near end
                    || world_tile_walkable(map, nx, y + partner_dz * 2) // beyond far end
            };
            if widens(x + 1) && widens(x - 1) {
                return true;
            }
        }
    }

    // --- 2-wide gap: walls run E-W (along X), gap crossed N-S (along Y) ---
    for partner_dx in [1i32, -1i32] {
        if world_tile_walkable(map, x + partner_dx, y)          // partner walkable
            && !world_tile_walkable(map, x - partner_dx, y)     // near end blocked
            && !world_tile_walkable(map, x + partner_dx * 2, y) // far end blocked
            && walk(0, 1) && walk(0, -1)                        // crossing axis open
        {
            let widens = |ny: i32| {
                world_tile_walkable(map, x - partner_dx, ny)
                    || world_tile_walkable(map, x + partner_dx * 2, ny)
            };
            if widens(y + 1) && widens(y - 1) {
                return true;
            }
        }
    }

    false
}

/// Adds back the immediate predecessor and successor of every doorway tile on
/// the raw `path` to the `simplified` waypoint list, preserving path order.
/// A door's only walkable neighbours lie on its open axis, so those two tiles
/// are collinear with it — keeping them forces an axis-aligned passage through
/// the gap. The output is still a subsequence of `path`.
fn reinsert_doorway_approaches(
    map: &Hypermap<f32>,
    path: &[(i32, i32)],
    simplified: Vec<(i32, i32)>,
) -> Vec<(i32, i32)> {
    if path.len() <= 2 {
        return simplified;
    }
    let index_of: HashMap<(i32, i32), usize> =
        path.iter().enumerate().map(|(i, &t)| (t, i)).collect();
    let mut keep = vec![false; path.len()];
    for w in &simplified {
        if let Some(&i) = index_of.get(w) {
            keep[i] = true;
        }
    }
    let last = path.len() - 1;
    for i in 1..last {
        let (x, y) = path[i];
        if is_doorway_tile(map, x, y) {
            keep[i - 1] = true;
            keep[i] = true;
            keep[i + 1] = true;
        }
    }
    keep.iter()
        .enumerate()
        .filter_map(|(i, &k)| k.then_some(path[i]))
        .collect()
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

/// Bounded 4-neighbour A\* on the **subtile** grid (`1 tile = SUBTILE_COUNT
/// subtiles`), used for short, local bot-on-bot detours around other actors.
///
/// Unlike the tile-level world A\*, a subtile is "walkable" only when the
/// caller-supplied `is_free(subtile)` returns `true`. The intended predicate
/// tests whether the moving actor's **whole circular footprint** (its size)
/// centered on that subtile is clear of static geometry *and* other creatures —
/// e.g. `DynamicPassabilityMap::probe_footprint(...).is_ok()`. That makes the
/// route account for actor size and steer around existing obstacles.
///
/// The search is deliberately cheap and local:
/// - it returns `None` immediately if the Manhattan span from `start` to `goal`
///   exceeds `max_span` subtiles (the detour is only for short distances);
/// - expansion is confined to the bounding box of `start`/`goal` grown by `pad`
///   subtiles on every side;
/// - it stops after `max_expanded` node expansions.
///
/// `start` is assumed already occupiable (it is where the actor stands) and is
/// never passed to `is_free`; `goal` must be `is_free` to be reachable. Returns
/// the subtile path including both endpoints, or `None` when no route fits the
/// bounds.
pub fn astar_subtile_detour(
    start: IVec2,
    goal: IVec2,
    pad: i32,
    max_span: i32,
    max_expanded: usize,
    is_free: impl Fn(IVec2) -> bool,
) -> Option<Vec<IVec2>> {
    if start == goal {
        return Some(vec![start]);
    }
    if (start.x - goal.x).abs() + (start.y - goal.y).abs() > max_span {
        return None;
    }

    let min = IVec2::new(start.x.min(goal.x) - pad, start.y.min(goal.y) - pad);
    let max = IVec2::new(start.x.max(goal.x) + pad, start.y.max(goal.y) + pad);
    let expanded = Cell::new(0usize);

    let result = astar(
        &start,
        |&p| {
            let mut out: Vec<(IVec2, u32)> = Vec::with_capacity(4);
            let n = expanded.get();
            if n > max_expanded {
                return out; // hard stop: starve the frontier so the search ends
            }
            expanded.set(n + 1);
            for d in [
                IVec2::new(1, 0),
                IVec2::new(-1, 0),
                IVec2::new(0, 1),
                IVec2::new(0, -1),
            ] {
                let np = p + d;
                if np.x < min.x || np.x > max.x || np.y < min.y || np.y > max.y {
                    continue;
                }
                // `start` is the only node never gated (the actor stands there);
                // every neighbour — goal included — must clear the footprint test.
                if !is_free(np) {
                    continue;
                }
                out.push((np, 1));
            }
            out
        },
        |&p| ((p.x - goal.x).abs() + (p.y - goal.y).abs()) as u32,
        |&p| p == goal,
    );
    result.map(|(path, _)| path)
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
    fn simplify_keeps_straight_approach_through_doorway() {
        // Two open rooms separated by a vertical wall at x = 3 with a single
        // one-tile doorway at (3, 2). Crossing the gap on a diagonal would clip
        // the doorframe, so the straight approach/exit tiles must survive.
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..7 {
            for y in 0..5 {
                if x != 3 {
                    map.set(x, y, 1.0);
                }
            }
        }
        map.set(3, 2, 1.0); // the doorway

        assert!(is_doorway_tile(&map, 3, 2));
        assert!(!is_doorway_tile(&map, 1, 2), "open-room tile is not a doorway");

        let raw = match astar_shortest_world_path(
            &map,
            (0, 0),
            (6, 4),
            HypermapSearchLimits::default(),
        ) {
            HypermapPathResult::Found { path, .. } => path,
            other => panic!("expected Found, got {other:?}"),
        };
        let simplified = simplify_path_line_of_sight(&map, &raw, 1);
        assert!(
            simplified.contains(&(3, 2)),
            "doorway tile must be a waypoint: {simplified:?}",
        );
        assert!(
            simplified.contains(&(2, 2)) && simplified.contains(&(4, 2)),
            "straight approach/exit tiles around the doorway must survive: {simplified:?}",
        );
    }

    #[test]
    fn simplify_does_not_treat_corridor_as_doorway() {
        // A straight one-wide horizontal corridor: every tile is pinched, but
        // none is a doorway, so open space still collapses to its endpoints.
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..10 {
            map.set(x, 0, 1.0);
        }
        assert!(!is_doorway_tile(&map, 5, 0));
        let raw = match astar_shortest_world_path(
            &map,
            (0, 0),
            (9, 0),
            HypermapSearchLimits::default(),
        ) {
            HypermapPathResult::Found { path, .. } => path,
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(simplify_path_line_of_sight(&map, &raw, 1), vec![(0, 0), (9, 0)]);
    }

    #[test]
    fn is_doorway_detects_two_wide_gap() {
        // Two open rooms separated by a vertical wall at x = 3, with a 2-tile doorway
        // at (3, 2) and (3, 3).  Both gap tiles must be identified as doorway tiles.
        //
        //   . . . . . . .
        //   . O O # O O .
        //   . O O . O O .   <- doorway row z=2 (tile 3 open)
        //   . O O . O O .   <- doorway row z=3 (tile 3 open)
        //   . O O # O O .
        //   . . . . . . .
        //
        // All non-wall tiles marked O are open; # = wall (blocked).
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..7i32 {
            for y in 0..6i32 {
                if x != 3 {
                    map.set(x, y, 1.0);
                }
            }
        }
        // Cut the 2-wide doorway at z=2 and z=3.
        map.set(3, 2, 1.0);
        map.set(3, 3, 1.0);

        assert!(is_doorway_tile(&map, 3, 2), "first gap tile must be a doorway");
        assert!(is_doorway_tile(&map, 3, 3), "second gap tile must be a doorway");
        assert!(!is_doorway_tile(&map, 1, 2), "open-room tile must not be a doorway");
        assert!(!is_doorway_tile(&map, 3, 0), "wall tile must not be a doorway");
    }

    #[test]
    fn does_not_treat_two_wide_corridor_as_doorway() {
        // A straight 2-wide corridor running along x: none of the interior tiles
        // should match as a doorway.
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..10i32 {
            map.set(x, 0, 1.0);
            map.set(x, 1, 1.0);
        }
        for x in 1..9 {
            assert!(
                !is_doorway_tile(&map, x, 0),
                "corridor tile ({x},0) must not be a doorway"
            );
            assert!(
                !is_doorway_tile(&map, x, 1),
                "corridor tile ({x},1) must not be a doorway"
            );
        }
    }

    #[test]
    fn simplify_keeps_approach_through_two_wide_doorway() {
        // Same setup as the 1-wide doorway test but with 2 open tiles at x=3.
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..7i32 {
            for y in 0..6i32 {
                if x != 3 {
                    map.set(x, y, 1.0);
                }
            }
        }
        map.set(3, 2, 1.0);
        map.set(3, 3, 1.0);

        let raw = match astar_shortest_world_path(
            &map,
            (0, 0),
            (6, 5),
            HypermapSearchLimits::default(),
        ) {
            HypermapPathResult::Found { path, .. } => path,
            other => panic!("expected Found, got {other:?}"),
        };
        let simplified = simplify_path_line_of_sight(&map, &raw, 1);
        // At least one doorway tile must remain in the simplified path.
        let has_doorway = simplified.iter().any(|&(x, y)| is_doorway_tile(&map, x, y));
        assert!(has_doorway, "at least one doorway tile must survive simplification: {simplified:?}");
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
    fn line_of_sight_blocks_corner_cut() {
        // Three tiles wrap a solid convex corner at (1, 1).
        let map: Hypermap<f32> = Hypermap::new(0.0);
        map.set(0, 0, 1.0);
        map.set(1, 0, 1.0);
        map.set(0, 1, 1.0);
        // The diagonal (1,0)->(0,1) slips past the solid corner (1,1): a wide
        // follower would clip it, so there is no line of sight.
        assert!(!line_of_sight(&map, (1, 0), (0, 1)));
        // Opening the corner clears both shoulders, so the diagonal is allowed.
        map.set(1, 1, 1.0);
        assert!(line_of_sight(&map, (1, 0), (0, 1)));
    }

    #[test]
    fn simplify_keeps_corner_when_diagonal_would_cut() {
        // An open field with one blocked shoulder tile on the main diagonal.
        // A straight (0,0)->(3,3) line would slip past it, so string-pulling
        // must keep an intermediate waypoint instead of cutting the corner —
        // this is the case where a wide ground bot otherwise wedges on a wall.
        let map: Hypermap<f32> = Hypermap::new(0.0);
        for x in 0..4 {
            for y in 0..4 {
                map.set(x, y, 1.0);
            }
        }
        map.set(2, 1, 0.0); // shoulder of the (1,1)->(2,2) diagonal step

        assert!(
            !line_of_sight(&map, (0, 0), (3, 3)),
            "diagonal must not cut the blocked shoulder",
        );

        let raw = match astar_shortest_world_path(
            &map,
            (0, 0),
            (3, 3),
            HypermapSearchLimits::default(),
        ) {
            HypermapPathResult::Found { path, .. } => path,
            other => panic!("expected Found, got {other:?}"),
        };
        let simplified = simplify_path_line_of_sight(&map, &raw, 0);
        assert_eq!(simplified.first().copied(), Some((0, 0)));
        assert_eq!(simplified.last().copied(), Some((3, 3)));
        assert!(
            simplified.len() >= 3,
            "must keep a corner waypoint rather than cut the diagonal: {simplified:?}",
        );
    }

    #[test]
    fn subtile_detour_routes_around_a_blocked_subtile() {
        use std::collections::HashSet;
        // Block a vertical strip of subtiles between start and goal; the detour
        // must step around it rather than straight through.
        let mut blocked: HashSet<IVec2> = HashSet::new();
        for y in -1..=1 {
            blocked.insert(IVec2::new(2, y));
        }
        let start = IVec2::new(0, 0);
        let goal = IVec2::new(4, 0);
        let path = astar_subtile_detour(start, goal, 4, 40, 4096, |s| !blocked.contains(&s))
            .expect("a detour exists around the strip");
        assert_eq!(path.first().copied(), Some(start));
        assert_eq!(path.last().copied(), Some(goal));
        assert!(
            path.iter().all(|p| !blocked.contains(p)),
            "detour must avoid every blocked subtile: {path:?}"
        );
        assert!(
            path.iter().any(|p| p.y != 0),
            "detour must leave the straight line to get around the strip: {path:?}"
        );
    }

    #[test]
    fn subtile_detour_rejects_long_spans() {
        let start = IVec2::new(0, 0);
        let goal = IVec2::new(100, 0);
        assert!(
            astar_subtile_detour(start, goal, 4, 40, 4096, |_| true).is_none(),
            "spans beyond max_span must be rejected as not a short detour"
        );
    }

    #[test]
    fn subtile_detour_none_when_goal_walled_off() {
        use std::collections::HashSet;
        // Fully enclose the goal so no footprint-free route reaches it.
        let goal = IVec2::new(4, 0);
        let mut blocked: HashSet<IVec2> = HashSet::new();
        for d in [
            IVec2::new(1, 0),
            IVec2::new(-1, 0),
            IVec2::new(0, 1),
            IVec2::new(0, -1),
        ] {
            blocked.insert(goal + d);
        }
        let start = IVec2::new(0, 0);
        assert!(
            astar_subtile_detour(start, goal, 4, 40, 4096, |s| !blocked.contains(&s)).is_none(),
            "an unreachable goal yields no detour"
        );
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
