//! Bot genetics: a fixed-size genome of random bytes that deterministically
//! encodes immutable per-bot properties (movement speed, acceleration, …).
//!
//! ## Model
//! At creation a bot rolls a [`Genome`]: 258 random unsigned bytes ([`Genes`])
//! plus the [`GeneticTraits`] derived from them. The raw genes are generated
//! once and **never mutated**; the traits are computed once and cached. After
//! creation runtime code reads only the traits — but the raw genes stay
//! accessible on the bot ([`Genome::genes`]) for inspection and for future
//! traits decoded from the same genome.
//!
//! ## Genome → trait mapping
//! Each property derives a single **seed byte** from the genome through its own
//! deterministic formula ([`speed_seed`], [`acceleration_seed`]), then feeds that
//! byte (mixed with a per-trait salt) into [`normal_sample`] to draw a normally
//! distributed value around the trait's mean. The whole mapping is pure and
//! deterministic: the same genome always yields the same traits.
//!
//! The seed formulas intentionally cross-reference several genes so a single
//! byte flip ripples through the phenotype. The canonical example (speed):
//! 1. take byte 143 XOR `123`,
//! 2. read byte 42 in base 128 (`byte(42) % 128`) — this picks which byte `X` to
//!    use,
//! 3. XOR the value from step 1 with the value of byte `X`.

use crate::rng::{self, StdRng};

/// Number of bytes in a [`Genes`] genome.
pub const GENE_COUNT: usize = 258;

/// Raw genome: 258 unsigned bytes.
///
/// Generated once at bot creation (deterministically from a seed) and never
/// mutated afterwards. The derived [`GeneticTraits`] are the runtime-relevant
/// product; these bytes are kept only so the genome remains inspectable and so
/// new traits can be decoded later from the same source.
#[derive(Clone)]
pub struct Genes([u8; GENE_COUNT]);

impl Genes {
    /// Generate a fresh genome deterministically from `seed`.
    ///
    /// Determinism-by-default: genes come from a seeded [`StdRng`], never
    /// `thread_rng`, so a reloaded bot (whose brain seed is persisted) rolls an
    /// identical genome.
    pub fn from_seed(seed: u64) -> Self {
        let mut rng = rng::seeded(seed);
        Self::random(&mut rng)
    }

    /// Fill a genome from an existing RNG stream.
    pub fn random(rng: &mut StdRng) -> Self {
        let mut bytes = [0u8; GENE_COUNT];
        for b in bytes.iter_mut() {
            // `rng` module has no raw-byte helper; draw a uniform 0..256.
            *b = rng::range(rng, 0u16..256) as u8;
        }
        Self(bytes)
    }

    /// Byte at `index`, wrapping so any formula index is always in range.
    #[inline]
    pub fn byte(&self, index: usize) -> u8 {
        self.0[index % GENE_COUNT]
    }

    /// The raw genome bytes.
    #[inline]
    pub fn bytes(&self) -> &[u8; GENE_COUNT] {
        &self.0
    }
}

impl std::fmt::Debug for Genes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Genes([{} bytes])", GENE_COUNT)
    }
}

// ---------------------------------------------------------------------------
// Normal-distribution sampling
// ---------------------------------------------------------------------------

/// Draw one value from a normal distribution with the given `mean` and
/// `std_dev`, deterministically seeded by `seed`.
///
/// Uses the Box–Muller transform on two uniform draws from a freshly seeded
/// [`StdRng`], so the same `seed` always returns the same sample — this is what
/// makes the genome → trait mapping deterministic.
pub fn normal_sample(seed: u64, mean: f32, std_dev: f32) -> f32 {
    let mut rng = rng::seeded(seed);
    mean + standard_normal(&mut rng) * std_dev
}

/// One sample from the standard normal `N(0, 1)` via the Box–Muller transform.
fn standard_normal(rng: &mut StdRng) -> f32 {
    // Two independent U(0,1) → one standard normal. Clamp `u1` off zero so the
    // `ln` is finite.
    let u1 = rng::unit_f32(rng).max(f32::MIN_POSITIVE);
    let u2 = rng::unit_f32(rng);
    (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
}

// ---------------------------------------------------------------------------
// Genome → seed-byte formulas
// ---------------------------------------------------------------------------

/// Seed byte for the **movement speed** trait.
///
/// The canonical formula from the design:
/// 1. `byte(143) ^ 123`,
/// 2. `X = byte(42) % 128` selects which byte to mix in,
/// 3. XOR step 1 with `byte(X)`.
pub fn speed_seed(genes: &Genes) -> u8 {
    let step1 = genes.byte(143) ^ 123;
    let x = (genes.byte(42) % 128) as usize;
    step1 ^ genes.byte(x)
}

/// Seed byte for the **acceleration** trait.
///
/// A distinct formula in the same spirit: a different anchor byte and XOR
/// constant, a different selector byte, and a rotate on the mixed-in byte so it
/// decorrelates from [`speed_seed`].
pub fn acceleration_seed(genes: &Genes) -> u8 {
    let step1 = genes.byte(7) ^ 211;
    let x = (genes.byte(200) % 128) as usize;
    step1 ^ genes.byte(x).rotate_left(3)
}

/// Seed byte for the **battery quality** trait. Another distinct formula:
/// different anchor/XOR/selector bytes and an opposite-direction rotate.
pub fn battery_quality_seed(genes: &Genes) -> u8 {
    let step1 = genes.byte(88) ^ 57;
    let x = (genes.byte(150) % 128) as usize;
    step1 ^ genes.byte(x).rotate_right(2)
}

// ---------------------------------------------------------------------------
// GeneticTraits
// ---------------------------------------------------------------------------

/// Per-trait salt mixed into the seed byte so two traits whose formula bytes
/// happen to coincide still draw from different RNG streams.
const SPEED_SALT: u64 = 0x5_0EED;
const ACCEL_SALT: u64 = 0xACCE_1;
const BATTERY_SALT: u64 = 0xBA77_E2;

/// **Battery quality** is the upper-clamped half of a normal distribution: a
/// `N(1.0, SPREAD)` draw clamped to `[MIN, 1.0]`. The entire upper half (samples
/// ≥ 1.0) collapses onto exactly `1.0`, so a perfect battery is the single most
/// popular variant — it "cannot be more than 100%, but can be less". `MIN` floors
/// it so the worst battery is still usable.
const BATTERY_QUALITY_SPREAD: f32 = 0.2;
const BATTERY_QUALITY_MIN: f32 = 0.4;
const BATTERY_QUALITY_PEAK: f32 = 1.0;

/// Discharge multiplier of a **perfect** (quality `1.0`) battery, relative to
/// the baseline rate. Below `1.0`, so a top-quality battery drains *slower* than
/// baseline (here, half as fast → roughly double the runtime); lower-quality
/// batteries scale up from this toward and past the baseline. See
/// [`GeneticTraits::discharge_multiplier`].
const BEST_BATTERY_DRAIN_MULT: f32 = 0.5;

/// Movement-speed distribution (tiles/s). Mean tracks the historical
/// [`FollowTuning`](crate::actor::brain::FollowTuning) default; the clamp keeps
/// the normal tails from producing a stationary or absurdly fast bot.
const SPEED_MEAN: f32 = 1.2;
const SPEED_STD: f32 = 0.3;
const SPEED_MIN: f32 = 0.5;
const SPEED_MAX: f32 = 2.2;

/// Acceleration distribution (tiles/s²), same convention as speed.
const ACCEL_MEAN: f32 = 2.5;
const ACCEL_STD: f32 = 0.6;
const ACCEL_MIN: f32 = 1.0;
const ACCEL_MAX: f32 = 4.5;

/// Immutable heritable properties decoded once from a [`Genes`] genome.
///
/// Currently movement-only; extend by adding fields plus a `*_seed` formula and
/// distribution constants. All values are computed at creation and never change.
#[derive(Clone, Copy, Debug)]
pub struct GeneticTraits {
    /// Maximum continuous travel speed in tiles/s.
    pub max_speed: f32,
    /// Acceleration toward the target heading in tiles/s².
    pub acceleration: f32,
    /// Battery quality in `(0, 1]` — half-normal, peaking at `1.0` (perfect) and
    /// only falling below. Governs the battery drain rate (see
    /// [`discharge_multiplier`](Self::discharge_multiplier)).
    pub battery_quality: f32,
}

impl GeneticTraits {
    /// Decode the heritable traits from a genome (pure / deterministic).
    pub fn from_genes(genes: &Genes) -> Self {
        Self {
            max_speed: trait_value(
                speed_seed(genes),
                SPEED_SALT,
                SPEED_MEAN,
                SPEED_STD,
                SPEED_MIN,
                SPEED_MAX,
            ),
            acceleration: trait_value(
                acceleration_seed(genes),
                ACCEL_SALT,
                ACCEL_MEAN,
                ACCEL_STD,
                ACCEL_MIN,
                ACCEL_MAX,
            ),
            // Centered at the 1.0 peak with `max = 1.0`, so the upper half of the
            // normal piles up at exactly 100% (the most popular variant).
            battery_quality: trait_value(
                battery_quality_seed(genes),
                BATTERY_SALT,
                BATTERY_QUALITY_PEAK,
                BATTERY_QUALITY_SPREAD,
                BATTERY_QUALITY_MIN,
                1.0,
            ),
        }
    }

    /// Battery drain rate relative to baseline:
    /// `BEST_BATTERY_DRAIN_MULT / battery_quality`. A perfect battery
    /// (`quality == 1.0`) drains at [`BEST_BATTERY_DRAIN_MULT`] — *below* baseline,
    /// so the best batteries last longest; a worse one (`quality < 1.0`) scales up
    /// from there, draining proportionally faster.
    #[inline]
    pub fn discharge_multiplier(&self) -> f32 {
        BEST_BATTERY_DRAIN_MULT / self.battery_quality
    }
}

/// Map a seed byte to a clamped normally-distributed trait value. The `salt`
/// decorrelates traits that share a seed byte; the seed byte itself is the
/// genome-derived entropy.
fn trait_value(seed_byte: u8, salt: u64, mean: f32, std_dev: f32, min: f32, max: f32) -> f32 {
    let seed = (seed_byte as u64).wrapping_add(salt.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    normal_sample(seed, mean, std_dev).clamp(min, max)
}

// ---------------------------------------------------------------------------
// Genome (component)
// ---------------------------------------------------------------------------

use bevy::prelude::Component;

/// A bot's complete genetic makeup: the immutable raw [`Genes`] plus the
/// [`GeneticTraits`] decoded from them.
///
/// Attached to a bot at spawn. The traits are what runtime systems consume
/// (e.g. movement tuning); the raw genes remain accessible for inspection and
/// future decoding but are otherwise dormant.
#[derive(Component, Clone, Debug)]
pub struct Genome {
    genes: Genes,
    traits: GeneticTraits,
}

impl Genome {
    /// Roll a genome deterministically from `seed` and decode its traits.
    pub fn from_seed(seed: u64) -> Self {
        let genes = Genes::from_seed(seed);
        let traits = GeneticTraits::from_genes(&genes);
        Self { genes, traits }
    }

    /// The raw, immutable genome.
    #[inline]
    pub fn genes(&self) -> &Genes {
        &self.genes
    }

    /// The decoded heritable traits.
    #[inline]
    pub fn traits(&self) -> &GeneticTraits {
        &self.traits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genes_from_seed_is_deterministic() {
        let a = Genes::from_seed(42);
        let b = Genes::from_seed(42);
        assert_eq!(a.bytes(), b.bytes());
    }

    #[test]
    fn different_seeds_give_different_genomes() {
        let a = Genes::from_seed(1);
        let b = Genes::from_seed(2);
        assert_ne!(a.bytes(), b.bytes(), "distinct seeds should differ");
    }

    #[test]
    fn byte_index_wraps() {
        let g = Genes::from_seed(7);
        assert_eq!(g.byte(GENE_COUNT), g.byte(0));
        assert_eq!(g.byte(GENE_COUNT + 5), g.byte(5));
    }

    #[test]
    fn normal_sample_is_deterministic() {
        assert_eq!(normal_sample(123, 1.0, 0.5), normal_sample(123, 1.0, 0.5));
    }

    #[test]
    fn normal_sample_mean_is_centered() {
        // Average over many seeds should sit near the mean.
        let n = 5000u64;
        let sum: f32 = (0..n).map(|s| normal_sample(s, 10.0, 2.0)).sum();
        let avg = sum / n as f32;
        assert!((avg - 10.0).abs() < 0.15, "empirical mean {avg} off from 10.0");
    }

    #[test]
    fn speed_seed_matches_design_formula() {
        let g = Genes::from_seed(99);
        let step1 = g.byte(143) ^ 123;
        let x = (g.byte(42) % 128) as usize;
        let expected = step1 ^ g.byte(x);
        assert_eq!(speed_seed(&g), expected);
    }

    #[test]
    fn traits_are_deterministic_and_in_range() {
        let g = Genes::from_seed(2024);
        let t1 = GeneticTraits::from_genes(&g);
        let t2 = GeneticTraits::from_genes(&g);
        assert_eq!(t1.max_speed, t2.max_speed);
        assert_eq!(t1.acceleration, t2.acceleration);
        assert_eq!(t1.battery_quality, t2.battery_quality);
        assert!((SPEED_MIN..=SPEED_MAX).contains(&t1.max_speed));
        assert!((ACCEL_MIN..=ACCEL_MAX).contains(&t1.acceleration));
        assert!((BATTERY_QUALITY_MIN..=1.0).contains(&t1.battery_quality));
    }

    #[test]
    fn genome_traits_match_genes() {
        let genome = Genome::from_seed(555);
        let direct = GeneticTraits::from_genes(genome.genes());
        assert_eq!(genome.traits().max_speed, direct.max_speed);
        assert_eq!(genome.traits().acceleration, direct.acceleration);
    }

    #[test]
    fn battery_quality_is_half_normal_capped_at_one() {
        // Across the population: never exceeds 1.0, never below the floor, and the
        // peak (1.0) is the single most common bucket.
        let mut at_peak = 0usize;
        let n = 2000u64;
        for s in 0..n {
            let q = Genome::from_seed(s).traits().battery_quality;
            assert!(q <= 1.0 + f32::EPSILON, "quality {q} exceeded 1.0");
            assert!(q >= BATTERY_QUALITY_MIN - f32::EPSILON, "quality {q} below floor");
            if q >= 1.0 - 1e-6 {
                at_peak += 1;
            }
        }
        // The half-normal peaks at 1.0, so the capped-at-1.0 bucket must be the
        // fullest — comfortably more than a uniform share.
        assert!(
            at_peak * 5 > n as usize,
            "expected 1.0 to dominate, only {at_peak}/{n} at peak"
        );
    }

    #[test]
    fn worse_battery_drains_faster() {
        let perfect = GeneticTraits {
            max_speed: 1.0,
            acceleration: 1.0,
            battery_quality: 1.0,
        };
        let poor = GeneticTraits {
            max_speed: 1.0,
            acceleration: 1.0,
            battery_quality: 0.5,
        };
        // Perfect battery drains at the sub-baseline best rate; quality 0.5
        // doubles that. Either way a worse battery drains faster.
        assert!((perfect.discharge_multiplier() - BEST_BATTERY_DRAIN_MULT).abs() < 1e-6);
        assert!((poor.discharge_multiplier() - 2.0 * BEST_BATTERY_DRAIN_MULT).abs() < 1e-6);
        assert!(perfect.discharge_multiplier() < 1.0, "top quality must drain below baseline");
        assert!(poor.discharge_multiplier() > perfect.discharge_multiplier());
    }

    #[test]
    fn traits_vary_across_population() {
        // Across many genomes the speed trait must actually spread (not collapse
        // to one clamped value).
        let speeds: std::collections::BTreeSet<u32> = (0..200u64)
            .map(|s| (Genome::from_seed(s).traits().max_speed * 1000.0) as u32)
            .collect();
        assert!(speeds.len() > 20, "expected genetic diversity, got {} distinct speeds", speeds.len());
    }
}
