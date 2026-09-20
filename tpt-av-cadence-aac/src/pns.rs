//! Perceptual Noise Substitution (ISO/IEC 14496-3 §4.6.12.4? PNS tool).
//!
//! PNS bands carry no spectral data; the decoder fills them with white
//! noise from the linear congruential generator mandated by the standard
//! and scales it to the band's transmitted energy.

/// The AAC noise LCG (identical constants in the reference decoder).
pub struct NoiseGenerator {
    state: u32,
}

impl NoiseGenerator {
    /// The reference decoder seeds the generator with this constant.
    pub fn new() -> Self {
        NoiseGenerator { state: 0x1f2e_3d4c }
    }

    /// Current LCG state (for snapshotting around speculative parses).
    pub fn state(&self) -> u32 {
        self.state
    }

    /// Restores a previously snapshotted LCG state.
    pub fn set_state(&mut self, state: u32) {
        self.state = state;
    }

    /// Next raw LCG value (wrapping 32-bit).
    pub fn next_u32(&mut self) -> u32 {
        self.state = self
            .state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        self.state
    }

    /// Fills `out` with noise scaled so its RMS energy matches
    /// `scale = sf / sqrt(Σ raw[k]²)` semantics of the reference decoder.
    /// Raw values convert to float as *signed* 32-bit (the reference keeps
    /// the LCG state in an `int`), so half the samples are negative.
    pub fn fill_scaled(&mut self, out: &mut [f32], scale: f32) {
        let mut energy = 0.0f32;
        for slot in out.iter_mut() {
            let raw = self.next_u32() as i32 as f32;
            *slot = raw;
            energy += raw * raw;
        }
        let norm = scale / energy.sqrt();
        for slot in out.iter_mut() {
            *slot *= norm;
        }
    }
}

impl Default for NoiseGenerator {
    fn default() -> Self {
        Self::new()
    }
}
