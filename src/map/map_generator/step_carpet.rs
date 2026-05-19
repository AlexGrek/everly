//! Step 1: road carpet inside the chunk margin.

use super::draft::{DraftTile, MapDraft};

impl MapDraft {
    pub fn step_init_carpet(&mut self) {
        let m = self.margin;
        let sz = self.size;
        for z in m..(sz - m) {
            for x in m..(sz - m) {
                self.set(x, z, DraftTile::Open);
            }
        }
    }
}
