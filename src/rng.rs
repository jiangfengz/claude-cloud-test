//! Deterministic pseudo-randomness.
//!
//! Every random choice in a simulation — network latency, which packet gets
//! dropped, election timeouts, the client workload, the fault schedule — flows
//! from a single `u64` seed through this module. That is what makes a failing
//! run reproducible bit for bit from nothing but its seed.
//!
//! Components draw from *independent streams* ([`Rng::stream`]) rather than
//! sharing one generator, so that e.g. removing a fault while shrinking does
//! not shift the random numbers seen by every client.

/// xoshiro256** seeded through splitmix64.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut sm = seed;
        Rng { s: [splitmix64(&mut sm), splitmix64(&mut sm), splitmix64(&mut sm), splitmix64(&mut sm)] }
    }

    /// An independent generator identified by `(seed, label)`.
    pub fn stream(seed: u64, label: u64) -> Self {
        let mut a = seed;
        let mut b = label ^ 0xD1B5_4A32_D192_ED03;
        Rng::new(splitmix64(&mut a) ^ splitmix64(&mut b).rotate_left(17))
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in `[0, n)` using Lemire's nearly-divisionless method, so the
    /// result is unbiased for every `n`.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below(0)");
        let mut m = u128::from(self.next_u64()) * u128::from(n);
        let mut lo = m as u64;
        if lo < n {
            let threshold = n.wrapping_neg() % n;
            while lo < threshold {
                m = u128::from(self.next_u64()) * u128::from(n);
                lo = m as u64;
            }
        }
        (m >> 64) as u64
    }

    /// Uniform in `[lo, hi)`; returns `lo` when the range is empty.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo { lo } else { lo + self.below(hi - lo) }
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    pub fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && self.unit() < p
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }

    pub fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            xs.swap(i, j);
        }
    }
}

/// FNV-1a, used to fingerprint runs. `std`'s `DefaultHasher` makes no
/// stability promise across Rust releases; this does.
#[derive(Clone, Copy, Debug)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mix(&mut self, x: u64) {
        for b in x.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn streams_are_independent() {
        let mut a = Rng::stream(7, 1);
        let mut b = Rng::stream(7, 2);
        let same = (0..64).filter(|_| a.next_u64() == b.next_u64()).count();
        assert_eq!(same, 0);
    }

    #[test]
    fn below_is_in_range_and_roughly_uniform() {
        let mut r = Rng::new(1);
        let mut buckets = [0u32; 6];
        for _ in 0..60_000 {
            buckets[r.below(6) as usize] += 1;
        }
        for &b in &buckets {
            assert!((9_000..11_000).contains(&b), "{buckets:?}");
        }
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut r = Rng::new(3);
        let mut xs: Vec<u32> = (0..50).collect();
        r.shuffle(&mut xs);
        let mut sorted = xs.clone();
        sorted.sort();
        assert_eq!(sorted, (0..50).collect::<Vec<_>>());
        assert_ne!(xs, sorted);
    }
}
