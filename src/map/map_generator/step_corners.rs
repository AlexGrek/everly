//! Step 8: stamp `c*` pillars from wall layout via [`corner_pillars`].

use super::corner_pillars::{detect_corner_pillars, WallField};
use super::draft::{DraftTile, MapDraft};

impl MapDraft {
    pub fn step_stamp_union_inner_corner_pillars(&mut self) {
        let field = wall_field_from_draft(self);
        for placement in detect_corner_pillars(&field) {
            if self.get(placement.x, placement.z) == DraftTile::Open {
                self.set(
                    placement.x,
                    placement.z,
                    DraftTile::Corner(placement.corner),
                );
            }
        }
    }
}

fn wall_field_from_draft(draft: &MapDraft) -> WallField {
    let sz = draft.size;
    let mut field = WallField::new(sz);
    for z in 0..sz {
        for x in 0..sz {
            if let DraftTile::Wall(bits) = draft.get(x, z) {
                field.set_wall(x, z, bits);
            }
        }
    }
    field
}
