//! A tiny seeded random number generator (`SplitMix64`), so simulated runs are reproducible
//! from a seed without depending on `rand`.

/// `SplitMix64` (Steele, Lea, Flood 2014): fast, statistically solid for simulation noise.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        // The top 53 bits fit an f64 mantissa exactly.
        #[allow(clippy::cast_precision_loss)]
        let top = (self.next_u64() >> 11) as f64;
        top / 9_007_199_254_740_992.0 // 2^53
    }

    /// Uniform in `[-1, 1)`.
    pub fn symmetric(&mut self) -> f64 {
        self.unit() * 2.0 - 1.0
    }

    /// `true` with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_reference_sequence() {
        // Reference values for SplitMix64 seeded with 0.
        let mut r = Rng::new(0);
        assert_eq!(r.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(r.next_u64(), 0x6e78_9e6a_a1b9_65f4);
    }

    #[test]
    fn same_seed_same_numbers() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn ranges() {
        let mut r = Rng::new(7);
        let mut hits = 0;
        for _ in 0..10_000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
            let s = r.symmetric();
            assert!((-1.0..1.0).contains(&s));
            if r.chance(0.25) {
                hits += 1;
            }
        }
        assert!((2_200..2_800).contains(&hits), "{hits}");
        assert!(!r.chance(0.0));
        assert!(r.chance(1.0));
    }
}
