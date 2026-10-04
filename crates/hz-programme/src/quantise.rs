//! A sample at ±1 into a writer's integers.
//!
//! Every writer ends the same way: a sum it made — a fold's element, a
//! render, an overlay's mix — or a master's sample with a gain on it goes
//! into the codec's integers, rounded, clamped and counted when it had to be
//! clamped. What differs between writers is only the domain: how many bits,
//! and what a mixed sample is rounded to. So the rule is stated once, here.

/// What the samples a [`Quantiser`] coded came to: the loudest, and how many
/// had to be clamped.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Levels {
    /// The loudest sample, as a fraction of full scale.
    pub peak: f64,
    /// Samples outside the domain, clamped to it.
    pub clipped: u64,
}

/// One writer's integer domain, and the step a sample is rounded to.
///
/// An element is a *sum*, and a sum of objects that happen to agree is louder
/// than any of them. Clamped rather than wrapped, and counted, because a mix
/// that clips is one whose elements had no headroom left for what was added
/// to them. Counted against the domain *before* the step is applied: a sample
/// the mix pushed past full scale is a clip, and one the rounding nudged over
/// is not. The ceiling is the top of the domain that lands on the step, so
/// that one sample near full scale does not take a block's wasted low bits
/// with it.
///
/// The bounds are worked out once: [`Quantiser::code`] runs once a sample
/// for the length of a programme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quantiser {
    full_scale: f64,
    step: f64,
    floor: f64,
    ceiling: f64,
    top: f64,
}

impl Quantiser {
    /// `full_scale` integers to the unit, every sample rounded to `step`.
    pub fn new(full_scale: f64, step: f64) -> Self {
        let (floor, ceiling) = (-full_scale, full_scale - 1.0);
        Self {
            full_scale,
            step,
            floor,
            ceiling,
            top: (ceiling / step).floor() * step,
        }
    }

    /// `bits` deep, every integer of the domain used.
    pub fn bits(bits: u32) -> Self {
        Self::new(f64::from(1u32 << (bits - 1)), 1.0)
    }

    /// The same domain, rounded to `step` instead.
    pub fn stepped(self, step: f64) -> Self {
        Self::new(self.full_scale, step)
    }

    /// Integers to the unit.
    pub fn full_scale(&self) -> f64 {
        self.full_scale
    }

    /// The top of the domain as a fraction of full scale — what a limiter
    /// keeps a mix under.
    pub fn ceiling(&self) -> f64 {
        self.ceiling / self.full_scale
    }

    /// `value`, at ±1, into the domain: rounded to the step, clamped, and
    /// counted in `levels`.
    #[inline]
    pub fn code(&self, value: f64, levels: &mut Levels) -> i32 {
        let raw = (value * self.full_scale).round();
        levels.peak = levels.peak.max(raw.abs() / self.full_scale);
        if !(self.floor..=self.ceiling).contains(&raw) {
            levels.clipped += 1;
        }
        ((raw / self.step).round() * self.step).clamp(self.floor, self.top) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rounded to the step, clamped to the top of the domain that lands on
    /// it, and a clip counted only where the mix itself went past full scale.
    #[test]
    fn a_mixed_sample_lands_on_the_step_inside_the_domain() {
        let mut levels = Levels::default();
        let full = 8_388_608.0;
        let q = Quantiser::new(full, 16.0);
        assert_eq!(q.code(0.5, &mut levels), 4_194_304);
        assert_eq!(q.code(f64::from(1.0 / full as f32 * 23.0), &mut levels), 16);
        assert_eq!(levels.clipped, 0);
        assert_eq!(q.code(1.0, &mut levels), 8_388_592);
        assert_eq!(levels.clipped, 1);
        assert_eq!(q.code(-1.0, &mut levels), -8_388_608);
        assert_eq!(levels.clipped, 1);
    }

    /// At a step of one every integer of the domain is reachable, and the
    /// top is the domain's own.
    #[test]
    fn at_a_step_of_one_the_domain_is_whole() {
        let mut levels = Levels::default();
        let q = Quantiser::bits(16);
        assert_eq!(q.code(1.0, &mut levels), 32_767);
        assert_eq!(levels.clipped, 1);
        assert_eq!(q.code(-1.0, &mut levels), -32_768);
        assert_eq!(q.code(3.0 / 32_768.0, &mut levels), 3);
        assert_eq!(levels.clipped, 1);
        assert_eq!(q.ceiling(), 32_767.0 / 32_768.0);
        assert_eq!(levels.peak, 1.0);
    }
}
