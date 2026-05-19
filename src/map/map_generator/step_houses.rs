//! Step 5b: merge subseed room rects into whole houses (drop subseed identity).

use super::draft::MapDraft;
use super::house::cluster_houses;

impl MapDraft {
    pub fn step_cluster_houses(&mut self) {
        self.houses = cluster_houses(&self.room_records);
    }
}
