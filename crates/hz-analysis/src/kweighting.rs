// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material taken from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavfilter/f_ebur128.c — Copyright (c) 2012 Clément Bœsch
// What was taken and what was changed is recorded in docs/provenance.md.

//! The K-weighting filter pair from ITU-R BS.1770-4.
//!
//! Two biquads in series: a high-shelf standing in for the acoustic effect of
//! a head, and a high-pass ("RLB") that discounts the low end the ear does not
//! weigh heavily. The recommendation publishes their coefficients at 48 kHz
//! only, so an implementation that needs another rate has to go back to the
//! analogue design the coefficients came from.
//!
//! The parametrisation used here — the shelf's centre frequency, gain and Q,
//! and the high-pass's centre frequency and Q — comes from FFmpeg's ebur128
//! filter, which recovered it by working backwards from the published 48 kHz
//! numbers. At 48 kHz it reproduces them, which is the test below.

/// A direct-form-II transposed biquad.
///
/// Transposed rather than direct: it needs two state words instead of four and
/// it is better behaved numerically, which matters over a programme's worth of
/// samples.
#[derive(Debug, Clone, Copy, Default)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Biquad {
    /// The BS.1770 shelf, designed at `sample_rate`.
    pub fn shelf(sample_rate: f64) -> Self {
        const F0: f64 = 1681.974450955533;
        const GAIN_DB: f64 = 3.999843853973347;
        const Q: f64 = 0.7071752369554196;

        let k = (std::f64::consts::PI * F0 / sample_rate).tan();
        let vh = 10f64.powf(GAIN_DB / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1.0 + k / Q + k * k;

        Self {
            b0: (vh + vb * k / Q + k * k) / a0,
            b1: 2.0 * (k * k - vh) / a0,
            b2: (vh - vb * k / Q + k * k) / a0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / Q + k * k) / a0,
        }
    }

    /// The BS.1770 high-pass, designed at `sample_rate`.
    pub fn high_pass(sample_rate: f64) -> Self {
        const F0: f64 = 38.13547087602444;
        const Q: f64 = 0.5003270373238773;

        let k = (std::f64::consts::PI * F0 / sample_rate).tan();
        let a0 = 1.0 + k / Q + k * k;

        Self {
            b0: 1.0,
            b1: -2.0,
            b2: 1.0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / Q + k * k) / a0,
        }
    }

    /// This filter's magnitude response at `frequency`, for checking a design
    /// without running a signal through it.
    pub fn magnitude(&self, frequency: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * frequency / sample_rate;
        let (sin1, cos1) = w.sin_cos();
        let (sin2, cos2) = (2.0 * w).sin_cos();

        let num_re = self.b0 + self.b1 * cos1 + self.b2 * cos2;
        let num_im = -(self.b1 * sin1 + self.b2 * sin2);
        let den_re = 1.0 + self.a1 * cos1 + self.a2 * cos2;
        let den_im = -(self.a1 * sin1 + self.a2 * sin2);

        ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)).sqrt()
    }
}

/// One channel's filter state.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    s1: f64,
    s2: f64,
}

impl State {
    #[inline]
    pub fn step(&mut self, filter: &Biquad, x: f64) -> f64 {
        let y = filter.b0 * x + self.s1;
        self.s1 = filter.b1 * x - filter.a1 * y + self.s2;
        self.s2 = filter.b2 * x - filter.a2 * y;
        y
    }
}

/// The two filters and one channel's state through them.
#[derive(Debug, Clone, Copy)]
pub struct KWeighting {
    shelf: Biquad,
    high_pass: Biquad,
}

impl KWeighting {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            shelf: Biquad::shelf(sample_rate),
            high_pass: Biquad::high_pass(sample_rate),
        }
    }

    #[inline]
    pub fn step(&self, state: &mut (State, State), x: f64) -> f64 {
        let y = state.0.step(&self.shelf, x);
        state.1.step(&self.high_pass, y)
    }

    /// The pair's magnitude response, for checking the design.
    pub fn magnitude(&self, frequency: f64, sample_rate: f64) -> f64 {
        self.shelf.magnitude(frequency, sample_rate)
            * self.high_pass.magnitude(frequency, sample_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recommendation publishes these, and they are the only external
    /// check on the design: if the parametrisation is wrong, this is where it
    /// shows.
    #[test]
    fn the_design_reproduces_the_published_48k_coefficients() {
        let shelf = Biquad::shelf(48_000.0);
        for (got, want) in [
            (shelf.b0, 1.53512485958697),
            (shelf.b1, -2.69169618940638),
            (shelf.b2, 1.19839281085285),
            (shelf.a1, -1.69065929318241),
            (shelf.a2, 0.73248077421585),
        ] {
            assert!((got - want).abs() < 1e-12, "{got} vs {want}");
        }

        let high_pass = Biquad::high_pass(48_000.0);
        for (got, want) in [
            (high_pass.a1, -1.99004745483398),
            (high_pass.a2, 0.99007225036621),
        ] {
            assert!((got - want).abs() < 1e-10, "{got} vs {want}");
        }
    }

    /// The response has to be the same curve at every rate, because that is
    /// the whole point of designing it rather than tabulating it. A filter
    /// built at the wrong rate still looks plausible in the time domain and
    /// fails here.
    #[test]
    fn the_response_has_the_shape_the_recommendation_describes() {
        for rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let k = KWeighting::new(rate);
            let db = |f: f64| 20.0 * k.magnitude(f, rate).log10();

            // The curve, measured rather than recalled: the high-pass takes
            // the bottom out, the shelf lifts everything above a couple of
            // kilohertz by four decibels, and the crossing is near 500 Hz.
            // Two earlier guesses here — "under −20 dB at 20 Hz" and "0 dB at
            // 1 kHz" — were both wrong, and the filter was right both times.
            for (frequency, expected) in [
                (20.0, -13.28),
                (100.0, -1.14),
                (1000.0, 0.69),
                (2000.0, 3.06),
                (10_000.0, 4.03),
                (20_000.0, 4.03),
            ] {
                assert!(
                    (db(frequency) - expected).abs() < 0.05,
                    "{rate}: {} at {frequency} Hz, expected {expected}",
                    db(frequency)
                );
            }
        }
    }

    /// A biquad that is not stable will not merely sound wrong; it will grow
    /// without bound over a programme.
    #[test]
    fn both_filters_are_stable_at_every_rate() {
        for rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            for filter in [Biquad::shelf(rate), Biquad::high_pass(rate)] {
                // Poles inside the unit circle: |a2| < 1 and |a1| < 1 + a2.
                assert!(filter.a2.abs() < 1.0, "{rate}: a2 = {}", filter.a2);
                assert!(
                    filter.a1.abs() < 1.0 + filter.a2,
                    "{rate}: a1 = {}, a2 = {}",
                    filter.a1,
                    filter.a2
                );
            }
        }
    }
}
