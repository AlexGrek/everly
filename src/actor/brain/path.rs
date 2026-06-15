//! Unified path nodes.
//!
//! A bot's route is a single `Vec<PathNode>`: coarse [`PathNode::Cell`]
//! waypoints come from the tile-level world A\*, fine [`PathNode::Sub`]
//! waypoints are spliced in from bot-on-bot / stall subtile detours. The
//! follower ([`super::low_level::FollowPath`]) steers toward `center()` for
//! either kind, so it has one cursor and one steering loop — no parallel
//! tile-path / detour lists.
//!
//! Keeping the cell/subcell distinction on the node (rather than flattening
//! everything to float centers) lets consumers that still think in tiles — the
//! occupancy overlay, the target halo — recover the containing tile via
//! [`PathNode::tile`] without guessing from a float.

use bevy::math::{IVec2, Vec2};

use crate::map::passability::SUBTILE_COUNT;

/// One waypoint in a bot's route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathNode {
    /// A whole-tile waypoint (tile coordinates), from the tile-level world A\*.
    Cell(IVec2),
    /// A subtile waypoint (world-subtile coordinates), from a spliced detour.
    Sub(IVec2),
}

impl PathNode {
    /// Convenience constructor for a cell node from raw `(x, y)` tile coords.
    #[inline]
    pub fn cell(x: i32, y: i32) -> Self {
        PathNode::Cell(IVec2::new(x, y))
    }

    /// Tile-space center the follower steers toward (`1 tile = SUBTILE_COUNT`
    /// subtiles). A cell's center is its tile midpoint; a subtile's center is the
    /// midpoint of that subtile.
    #[inline]
    pub fn center(self) -> Vec2 {
        match self {
            PathNode::Cell(c) => Vec2::new(c.x as f32 + 0.5, c.y as f32 + 0.5),
            PathNode::Sub(s) => {
                let sc = SUBTILE_COUNT as f32;
                Vec2::new((s.x as f32 + 0.5) / sc, (s.y as f32 + 0.5) / sc)
            }
        }
    }

    /// The world tile that contains this node — its own coords for a cell, or the
    /// tile owning the subtile for a sub node.
    #[inline]
    pub fn tile(self) -> (i32, i32) {
        match self {
            PathNode::Cell(c) => (c.x, c.y),
            PathNode::Sub(s) => {
                let sc = SUBTILE_COUNT as i32;
                (s.x.div_euclid(sc), s.y.div_euclid(sc))
            }
        }
    }

    #[inline]
    pub fn is_cell(self) -> bool {
        matches!(self, PathNode::Cell(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_center_is_tile_midpoint() {
        assert_eq!(PathNode::cell(3, 4).center(), Vec2::new(3.5, 4.5));
    }

    #[test]
    fn sub_center_is_subtile_midpoint() {
        // Subtile (sc/2) of tile 0 lands on the tile center column.
        let sc = SUBTILE_COUNT as i32;
        let node = PathNode::Sub(IVec2::new(sc / 2, sc / 2));
        let c = node.center();
        assert!((c.x - 0.5).abs() < 0.2, "center near tile midpoint, got {c:?}");
    }

    #[test]
    fn tile_of_sub_is_containing_tile() {
        let sc = SUBTILE_COUNT as i32;
        assert_eq!(PathNode::Sub(IVec2::new(sc + 2, 1)).tile(), (1, 0));
        assert_eq!(PathNode::Sub(IVec2::new(-1, -1)).tile(), (-1, -1));
    }

    #[test]
    fn tile_of_cell_is_its_coords() {
        assert_eq!(PathNode::cell(-3, 7).tile(), (-3, 7));
    }
}
