//! SplitMix64: the only randomness in the harness.
//!
//! Both the task generator and the bootstrap resampler use it, so every random
//! decision in an evaluation is a pure function of a recorded `u64` seed.

/// Small, seedable and platform-independent generator.
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    pub(crate) fn below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0);
        self.next_u64() % bound
    }

    pub(crate) fn pick<T: Copy>(&mut self, values: &[T]) -> T {
        values[self.below(values.len() as u64) as usize]
    }

    pub(crate) fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            let other = self.below(index as u64 + 1) as usize;
            values.swap(index, other);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SplitMix64;

    #[test]
    fn stream_is_deterministic() {
        let mut first = SplitMix64::new(7);
        let mut second = SplitMix64::new(7);
        for _ in 0..8 {
            assert_eq!(first.next_u64(), second.next_u64());
        }
        assert_ne!(SplitMix64::new(7).next_u64(), SplitMix64::new(8).next_u64());
    }
}
