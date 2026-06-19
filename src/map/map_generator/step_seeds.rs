//! Steps 2–4: primary seeds, separation, subseed spawn.

use crate::rng;

use super::draft::MapDraft;
use super::types::{
    MIN_SEED_DISTANCE, PRIMARY_SEED_COUNT_MAX, PRIMARY_SEED_COUNT_MIN,
    SUBSEEDS_PER_PRIMARY_MAX, SUBSEEDS_PER_PRIMARY_MIN,
};

impl MapDraft {
    pub fn step_place_primary_seeds(&mut self) {
        let target = rng::range(
            &mut self.rng,
            PRIMARY_SEED_COUNT_MIN..=PRIMARY_SEED_COUNT_MAX,
        );
        self.primary_seeds.clear();
        const MAX_ATTEMPTS: u32 = 4096;
        for _ in 0..target {
            let mut placed = false;
            for _ in 0..MAX_ATTEMPTS {
                let candidate = (
                    rng::range(
                        &mut self.rng,
                        self.bounds.place_lo..=self.bounds.place_hi,
                    ),
                    rng::range(
                        &mut self.rng,
                        self.bounds.place_lo..=self.bounds.place_hi,
                    ),
                );
                if primary_seed_valid(candidate, &self.primary_seeds) {
                    self.primary_seeds.push(candidate);
                    placed = true;
                    break;
                }
            }
            if !placed {
                break;
            }
        }
    }

    /// Reserved hook after placement; spacing is enforced during [`step_place_primary_seeds`].
    pub fn step_separate_primary_seeds(&mut self) {}

    pub fn step_spawn_subseeds(&mut self) {
        self.growth_centers = self.primary_seeds.clone();
        self.subseed_centers.clear();
        let seeds = self.primary_seeds.clone();
        for &(sx, sz_seed) in &seeds {
            let sub_count = rng::range(
                &mut self.rng,
                SUBSEEDS_PER_PRIMARY_MIN..=SUBSEEDS_PER_PRIMARY_MAX,
            );
            for _ in 0..sub_count {
                let dist = rng::range(&mut self.rng, 2..=5);
                let dir = rng::range(&mut self.rng, 0..8);
                let (dx, dz) = subseed_offset(dir, dist);
                let x = (sx + dx).clamp(self.bounds.grow_lo, self.bounds.grow_hi);
                let z = (sz_seed + dz).clamp(self.bounds.grow_lo, self.bounds.grow_hi);
                self.growth_centers.push((x, z));
                self.subseed_centers.push((x, z));
            }
        }
    }
}

pub(crate) fn manhattan(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs() + (a.1 - b.1).abs()
}

fn primary_seed_valid(candidate: (i32, i32), existing: &[(i32, i32)]) -> bool {
    existing
        .iter()
        .all(|&other| manhattan(other, candidate) >= MIN_SEED_DISTANCE)
}

fn subseed_offset(dir: u8, dist: i32) -> (i32, i32) {
    match dir % 8 {
        0 => (dist, 0),
        1 => (dist, dist),
        2 => (0, dist),
        3 => (-dist, dist),
        4 => (-dist, 0),
        5 => (-dist, -dist),
        6 => (0, -dist),
        _ => (dist, -dist),
    }
}
