//! Steps 6–7: union interior floor and outer perimeter walls.

use super::draft::{DraftTile, MapDraft};
use super::union::{union_contains, union_perimeter_wall_mask};

impl MapDraft {
    /// Marks the union of all subseed room rectangles as walkable floor (no walls yet).
    pub fn step_paint_union_interior(&mut self) {
        let rooms = self.rooms();
        for z in 0..self.size {
            for x in 0..self.size {
                if union_contains(&rooms, x, z) {
                    self.set(x, z, DraftTile::Open);
                }
            }
        }
    }

    /// Outer shell only: perimeter cells of the union get outward-facing wall masks.
    pub fn step_build_union_outer_walls(&mut self) {
        let rooms = self.rooms();
        for z in 0..self.size {
            for x in 0..self.size {
                let Some(mask) = union_perimeter_wall_mask(&rooms, x, z) else {
                    continue;
                };
                self.set(x, z, DraftTile::Wall(mask.bits()));
            }
        }
    }
}
