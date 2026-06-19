//! Step: stamp square pond voids on exterior road (reveals chunk water below).

use crate::rng;

use super::draft::{DraftTile, MapDraft};
use super::house::House;
use super::types::{
    POND_EDGE_MAX, POND_EDGE_MIN, PONDS_PER_CHUNK_MAX, PONDS_PER_CHUNK_MIN,
};

impl MapDraft {
    /// Places 0–2 square ponds on open road tiles that do not overlap any house footprint.
    pub fn step_place_ponds(&mut self) {
        let target = rng::range(&mut self.rng, PONDS_PER_CHUNK_MIN..=PONDS_PER_CHUNK_MAX);
        let mut placed: Vec<(i32, i32, i32, i32)> = Vec::new();
        const MAX_ATTEMPTS: u32 = 512;

        for _ in 0..target {
            let mut stamped = false;
            for _ in 0..MAX_ATTEMPTS {
                let edge = rng::range(&mut self.rng, POND_EDGE_MIN..=POND_EDGE_MAX);
                let max_origin = self.bounds.grow_hi - edge + 1;
                if max_origin < self.bounds.grow_lo {
                    break;
                }
                let x0 = rng::range(&mut self.rng, self.bounds.grow_lo..=max_origin);
                let z0 = rng::range(&mut self.rng, self.bounds.grow_lo..=max_origin);
                let x1 = x0 + edge - 1;
                let z1 = z0 + edge - 1;
                let rect = (x0, z0, x1, z1);
                if pond_overlaps_house(rect, &self.houses)
                    || pond_overlaps_pond(rect, &placed)
                    || !pond_fits_on_exterior_road(self, rect, &self.houses)
                {
                    continue;
                }
                stamp_pond(self, rect);
                placed.push(rect);
                stamped = true;
                break;
            }
            if !stamped {
                break;
            }
        }
    }
}

fn pond_overlaps_house((x0, z0, x1, z1): (i32, i32, i32, i32), houses: &[House]) -> bool {
    for z in z0..=z1 {
        for x in x0..=x1 {
            if houses.iter().any(|h| h.contains(x, z)) {
                return true;
            }
        }
    }
    false
}

fn pond_overlaps_pond(
    (x0, z0, x1, z1): (i32, i32, i32, i32),
    placed: &[(i32, i32, i32, i32)],
) -> bool {
    placed.iter().any(|&(px0, pz0, px1, pz1)| {
        x0 <= px1 && px0 <= x1 && z0 <= pz1 && pz0 <= z1
    })
}

fn pond_fits_on_exterior_road(
    draft: &MapDraft,
    (x0, z0, x1, z1): (i32, i32, i32, i32),
    houses: &[House],
) -> bool {
    for z in z0..=z1 {
        for x in x0..=x1 {
            if houses.iter().any(|h| h.contains(x, z)) {
                return false;
            }
            if draft.get(x, z) != DraftTile::Open {
                return false;
            }
        }
    }
    true
}

fn stamp_pond(draft: &mut MapDraft, (x0, z0, x1, z1): (i32, i32, i32, i32)) {
    for z in z0..=z1 {
        for x in x0..=x1 {
            draft.set(x, z, DraftTile::Void);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::map_generator::draft::Room;
    use crate::map::map_generator::MapDraft;
    use crate::map::map_generator::types::MapGeneratorConfig;

    #[test]
    fn pond_rect_rejects_house_overlap() {
        let house = House::from_single_rect(Room {
            x0: 10,
            z0: 10,
            x1: 20,
            z1: 20,
        });
        let houses = [house];
        assert!(pond_overlaps_house((8, 8, 12, 12), &houses));
        assert!(!pond_overlaps_house((0, 0, 5, 5), &houses));
    }

    #[test]
    fn procedural_ponds_are_void_and_avoid_houses() {
        for seed in 0..64u64 {
            let mut draft = MapDraft::new(MapGeneratorConfig {
                seed,
                ..Default::default()
            });
            draft.step_init_carpet();
            draft.step_place_primary_seeds();
            draft.step_separate_primary_seeds();
            draft.step_spawn_subseeds();
            draft.step_grow_rooms();
            draft.step_cluster_houses();
            draft.build_house_structures();
            draft.step_place_ponds();
            let mut pond_cells = 0usize;
            for z in draft.bounds.grow_lo..=draft.bounds.grow_hi {
                for x in draft.bounds.grow_lo..=draft.bounds.grow_hi {
                    if draft.get(x, z) != DraftTile::Void {
                        continue;
                    }
                    if draft.houses.iter().any(|h| h.contains(x, z)) {
                        panic!("seed {seed}: pond void at ({x},{z}) overlaps a house");
                    }
                    pond_cells += 1;
                }
            }
            let _ = pond_cells;
        }
    }
}
