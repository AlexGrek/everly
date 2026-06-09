//! Seeded randomness for Everly.
//!
//! All gameplay randomness goes through this module. Uses [`StdRng`] with explicit
//! seeds — never [`thread_rng`](rand::thread_rng) — so scenes stay reproducible.

use rand::distributions::{Distribution, WeightedIndex};
use rand::Rng;

pub use rand::rngs::StdRng;
pub use rand::SeedableRng;

/// New RNG from a fixed `u64` seed.
#[inline]
pub fn seeded(seed: u64) -> StdRng {
    SeedableRng::seed_from_u64(seed)
}

/// Nondeterministic `u64` for one-off procedural seeds (e.g. first chunk generation).
#[inline]
pub fn fresh_seed() -> u64 {
    rand::random()
}

/// Uniform sample over `bounds` (same semantics as [`Rng::gen_range`]).
#[inline]
pub fn range<T, R>(rng: &mut StdRng, bounds: R) -> T
where
    T: rand::distributions::uniform::SampleUniform,
    R: rand::distributions::uniform::SampleRange<T>,
{
    rng.gen_range(bounds)
}

/// Uniform `f32` in `[min, max)`.
#[inline]
pub fn f32_in(rng: &mut StdRng, min: f32, max: f32) -> f32 {
    range(rng, min..max)
}

/// Uniform `f32` in `[0, 1)`.
#[inline]
pub fn unit_f32(rng: &mut StdRng) -> f32 {
    range(rng, 0.0_f32..1.0)
}

/// Returns `true` with probability `p` in `[0, 1]`.
#[inline]
pub fn chance(rng: &mut StdRng, p: f32) -> bool {
    unit_f32(rng) < p
}

/// Returns `true` with probability `p` in `[0, 1]` (`f64` for legacy `gen_bool` sites).
#[inline]
pub fn chance_f64(rng: &mut StdRng, p: f64) -> bool {
    rng.gen_bool(p)
}

/// Fair coin flip (`50%` each side).
#[inline]
pub fn coin_flip(rng: &mut StdRng) -> bool {
    range(rng, 0..2) == 0
}

/// Returns `true` with probability `1 / n` (e.g. `one_in(rng, 4)` → 25%).
#[inline]
pub fn one_in(rng: &mut StdRng, n: u32) -> bool {
    debug_assert!(n > 0, "one_in: n must be > 0");
    range(rng, 0..n) == 0
}

/// Uniform angle in `[0, τ)`.
#[inline]
pub fn angle(rng: &mut StdRng) -> f32 {
    range(rng, 0.0_f32..std::f32::consts::TAU)
}

/// Pick a random element from a non-empty slice.
#[inline]
pub fn pick<'a, T>(rng: &mut StdRng, items: &'a [T]) -> &'a T {
    debug_assert!(!items.is_empty(), "pick: slice must not be empty");
    &items[range(rng, 0..items.len())]
}

/// Pick a random element, returning `None` when `items` is empty.
#[inline]
pub fn pick_opt<'a, T>(rng: &mut StdRng, items: &'a [T]) -> Option<&'a T> {
    if items.is_empty() {
        None
    } else {
        Some(pick(rng, items))
    }
}

/// Branch index from cumulative probability cutoffs.
///
/// Rolls `u ~ U(0, 1)` and returns the smallest `i` where `u < cutoffs[i]`, else
/// `cutoffs.len()`. Example: `categorical(rng, &[0.1, 0.4])` → `0` (10%), `1`
/// (30%), or `2` (60%).
#[inline]
pub fn categorical(rng: &mut StdRng, cutoffs: &[f32]) -> usize {
    let roll = unit_f32(rng);
    for (i, &cut) in cutoffs.iter().enumerate() {
        if roll < cut {
            return i;
        }
    }
    cutoffs.len()
}

/// Index sampled by relative weights (need not sum to `1`).
pub fn weighted_index(rng: &mut StdRng, weights: &[f32]) -> usize {
    let dist =
        WeightedIndex::new(weights).expect("weighted_index: empty or all-zero weights");
    dist.sample(rng)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_is_deterministic() {
        let a = range(&mut seeded(42), 0..100);
        let b = range(&mut seeded(42), 0..100);
        assert_eq!(a, b);
    }

    #[test]
    fn one_in_never_true_when_n_is_one() {
        let mut rng = seeded(0);
        for _ in 0..32 {
            assert!(one_in(&mut rng, 1));
        }
    }

    #[test]
    fn categorical_respects_cutoffs() {
        let mut rng = seeded(99);
        let mut counts = [0usize; 3];
        for _ in 0..1000 {
            counts[categorical(&mut rng, &[0.1, 0.4])] += 1;
        }
        assert!(counts[0] > 0 && counts[1] > 0 && counts[2] > 0);
    }

    #[test]
    fn pick_returns_slice_element() {
        let items = [10, 20, 30];
        let mut rng = seeded(7);
        assert!(items.contains(pick(&mut rng, &items)));
    }
}
