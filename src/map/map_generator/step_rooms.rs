//! Step 5: grow axis-aligned rooms from subseed centers.

use rand::Rng;

use super::draft::{MapDraft, Room, RoomRecord};

impl MapDraft {
    pub fn step_grow_rooms(&mut self) {
        self.room_records.clear();
        for center in self.subseed_centers.clone() {
            let bounds = Room::from_center_growth(center, &mut self.rng, self.bounds);
            if bounds.area() >= 4 {
                self.room_records.push(RoomRecord { bounds });
            }
        }
    }
}

impl Room {
    fn from_center_growth(
        center: (i32, i32),
        rng: &mut rand::rngs::StdRng,
        bounds: super::draft::DraftBounds,
    ) -> Self {
        let rx = rng.gen_range(3..=6);
        let rz = rng.gen_range(3..=6);
        let x0 = (center.0 - rx).clamp(bounds.grow_lo, bounds.grow_hi);
        let x1 = (center.0 + rx).clamp(bounds.grow_lo, bounds.grow_hi);
        let z0 = (center.1 - rz).clamp(bounds.grow_lo, bounds.grow_hi);
        let z1 = (center.1 + rz).clamp(bounds.grow_lo, bounds.grow_hi);
        Self { x0, z0, x1, z1 }
    }

    fn area(&self) -> i32 {
        (self.x1 - self.x0 + 1).max(0) * (self.z1 - self.z0 + 1).max(0)
    }
}
