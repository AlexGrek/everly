//! Steps 2–4: primary seeds, separation, subseed spawn.

use crate::rng;

use super::draft::MapDraft;
use super::types::{
    BORDER_CLEARANCE, MIN_SEED_DISTANCE, PRIMARY_SEED_COUNT_MAX, PRIMARY_SEED_COUNT_MIN,
    SUBSEEDS_PER_PRIMARY_MAX, SUBSEEDS_PER_PRIMARY_MIN,
};

impl MapDraft {
    pub fn step_place_primary_seeds(&mut self) {
        let count = rng::range(
            &mut self.rng,
            PRIMARY_SEED_COUNT_MIN..=PRIMARY_SEED_COUNT_MAX,
        );
        self.primary_seeds = (0..count)
            .map(|_| {
                (
                    rng::range(
                        &mut self.rng,
                        self.bounds.place_lo..=self.bounds.place_hi,
                    ),
                    rng::range(
                        &mut self.rng,
                        self.bounds.place_lo..=self.bounds.place_hi,
                    ),
                )
            })
            .collect();
    }

    pub fn step_separate_primary_seeds(&mut self) {
        separate_positions(
            &mut self.primary_seeds,
            self.bounds.place_lo,
            self.bounds.place_hi,
            self.bounds.place_lo,
            self.bounds.place_hi,
        );
    }

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
                let dist = rng::range(&mut self.rng, 2..=6);
                let dir = rng::range(&mut self.rng, 0..8);
                let (dx, dz) = subseed_offset(dir, dist);
                let x = (sx + dx).clamp(self.bounds.place_lo, self.bounds.place_hi);
                let z = (sz_seed + dz).clamp(self.bounds.place_lo, self.bounds.place_hi);
                self.growth_centers.push((x, z));
                self.subseed_centers.push((x, z));
            }
        }
    }
}

pub(crate) fn manhattan(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs() + (a.1 - b.1).abs()
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

fn separate_positions(
    positions: &mut [(i32, i32)],
    min_x: i32,
    max_x: i32,
    min_z: i32,
    max_z: i32,
) {
    const MAX_ITERS: usize = 128;
    for _ in 0..MAX_ITERS {
        let mut moved = false;

        for i in 0..positions.len() {
            for j in (i + 1)..positions.len() {
                let d = manhattan(positions[i], positions[j]);
                if d >= MIN_SEED_DISTANCE {
                    continue;
                }
                let push = MIN_SEED_DISTANCE - d;
                let (dx, dz) = separation_delta(positions[i], positions[j]);
                positions[i].0 = (positions[i].0 + dx * push).clamp(min_x, max_x);
                positions[i].1 = (positions[i].1 + dz * push).clamp(min_z, max_z);
                positions[j].0 = (positions[j].0 - dx * push).clamp(min_x, max_x);
                positions[j].1 = (positions[j].1 - dz * push).clamp(min_z, max_z);
                moved = true;
            }
        }

        for p in positions.iter_mut() {
            if p.0 - min_x < BORDER_CLEARANCE {
                p.0 = (p.0 + 1).min(max_x);
                moved = true;
            }
            if max_x - p.0 < BORDER_CLEARANCE {
                p.0 = (p.0 - 1).max(min_x);
                moved = true;
            }
            if p.1 - min_z < BORDER_CLEARANCE {
                p.1 = (p.1 + 1).min(max_z);
                moved = true;
            }
            if max_z - p.1 < BORDER_CLEARANCE {
                p.1 = (p.1 - 1).max(min_z);
                moved = true;
            }
        }

        if !moved {
            break;
        }
    }
}

fn separation_delta(a: (i32, i32), b: (i32, i32)) -> (i32, i32) {
    let dx = a.0 - b.0;
    let dz = a.1 - b.1;
    if dx == 0 && dz == 0 {
        return (1, 0);
    }
    let ax = dx.signum();
    let az = dz.signum();
    if dx.abs() >= dz.abs() {
        (ax, 0)
    } else {
        (0, az)
    }
}
