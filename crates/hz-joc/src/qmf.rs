// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from the `eac3` crate of harletty-bridge
// (https://github.com/harletty/harletty-bridge, tag v0.9.0) — Copyright (c)
// its authors — under the Apache License, Version 2.0, whose text is
// reproduced in LICENSES/Apache-2.0.txt.
// What was taken and what was changed is recorded in docs/provenance.md.

//! The analysis half of the 64-band complex QMF bank the decoder runs joint
//! object coding in.
//!
//! Each slot takes 64 new samples, windows the newest 640 by the prototype,
//! folds them to 128 and modulates:
//!
//! ```text
//! X[k] = Σ g[n]·e^{iπ(k + ½)(n − ½)/64},  n < 128, k < 64
//! ```
//!
//! The windowing, the fold and the way the modulation is taken as one 64-point
//! DFT between two rotations are the workspace decoder's, so that what an
//! encoder estimates in is the domain the decoder applies it in; the DFT is
//! this file's own, and the test holds the whole to the formula above.

use crate::prototype::PROTOTYPE;
use std::f64::consts::PI;

/// Subbands, and samples a slot advances by.
pub const SUBBANDS: usize = 64;
const TAPS: usize = 640;
const FOLD: usize = 2 * SUBBANDS;

/// One slot's subbands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub real: [f32; SUBBANDS],
    pub imaginary: [f32; SUBBANDS],
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            real: [0.0; SUBBANDS],
            imaginary: [0.0; SUBBANDS],
        }
    }
}

/// The modulation's tables, shared by every bank.
struct Tables {
    /// `e^{iπm/64}`, rotating the folded pairs into the DFT.
    pack: [(f32, f32); SUBBANDS],
    /// `α_k = (conj ψ_k − iψ_k)/2` and `β_k = (conj ψ_k + iψ_k)/2`, with
    /// `ψ_k = e^{iπ(2k+1)/256}`, separating the DFT back into the bands.
    alpha: [(f32, f32); SUBBANDS],
    beta: [(f32, f32); SUBBANDS],
    /// `e^{+2πik/64}` for the DFT's butterflies.
    twiddles: [(f32, f32); SUBBANDS / 2],
    reverse: [u8; SUBBANDS],
}

fn tables() -> &'static Tables {
    static TABLES: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let c = |phase: f64| (phase.cos() as f32, phase.sin() as f32);
        let psi = |k: usize| {
            let phase = PI * (2 * k + 1) as f64 / 256.0;
            (phase.cos(), phase.sin())
        };
        let bits = SUBBANDS.trailing_zeros();
        Tables {
            pack: std::array::from_fn(|m| c(PI * m as f64 / SUBBANDS as f64)),
            alpha: std::array::from_fn(|k| {
                let (re, im) = psi(k);
                (((re + im) / 2.0) as f32, (-(re + im) / 2.0) as f32)
            }),
            beta: std::array::from_fn(|k| {
                let (re, im) = psi(k);
                (((re - im) / 2.0) as f32, ((re - im) / 2.0) as f32)
            }),
            twiddles: std::array::from_fn(|k| c(2.0 * PI * k as f64 / SUBBANDS as f64)),
            reverse: std::array::from_fn(|i| ((i as u32).reverse_bits() >> (32 - bits)) as u8),
        }
    })
}

/// One channel's analysis bank: its last 640 samples, newest first, and the
/// scratch a slot needs, so a slot allocates nothing.
#[derive(Clone)]
pub struct Analysis {
    /// Twice over, so the window is always one contiguous slice.
    history: Box<[f32; 2 * TAPS]>,
    head: usize,
    re: [f32; SUBBANDS],
    im: [f32; SUBBANDS],
}

impl Default for Analysis {
    fn default() -> Self {
        Self::new()
    }
}

impl Analysis {
    pub fn new() -> Self {
        Self {
            history: Box::new([0.0; 2 * TAPS]),
            head: 0,
            re: [0.0; SUBBANDS],
            im: [0.0; SUBBANDS],
        }
    }

    /// The subbands of the next 64 samples.
    pub fn slot(&mut self, input: &[f32], out: &mut Slot) {
        debug_assert_eq!(input.len(), SUBBANDS);
        self.head = if self.head >= SUBBANDS {
            self.head - SUBBANDS
        } else {
            TAPS + self.head - SUBBANDS
        };
        for (offset, sample) in input.iter().rev().enumerate() {
            let at = self.head + offset;
            self.history[at] = *sample;
            self.history[at + TAPS] = *sample;
        }
        let window = &self.history[self.head..self.head + TAPS];

        let mut fold = [0f32; FOLD];
        for (n, g) in fold.iter_mut().enumerate() {
            *g = (0..TAPS / FOLD)
                .map(|j| window[FOLD * j + n] * PROTOTYPE[FOLD * j + n])
                .sum();
        }

        let t = tables();
        for m in 0..SUBBANDS {
            let (pr, pi) = t.pack[m];
            let (a, b) = (fold[2 * m], fold[2 * m + 1]);
            let slot = usize::from(t.reverse[m]);
            self.re[slot] = a * pr - b * pi;
            self.im[slot] = a * pi + b * pr;
        }
        self.dft();
        for k in 0..SUBBANDS {
            let (zr, zi) = (self.re[k], self.im[k]);
            let mirror = SUBBANDS - 1 - k;
            let (wr, wi) = (self.re[mirror], -self.im[mirror]);
            let (ar, ai) = t.alpha[k];
            let (br, bi) = t.beta[k];
            out.real[k] = ar * zr - ai * zi + br * wr - bi * wi;
            out.imaginary[k] = ar * zi + ai * zr + br * wi + bi * wr;
        }
    }

    /// An in-place radix-2 DFT of `re`/`im`, already in bit-reversed order,
    /// with the positive exponent.
    fn dft(&mut self) {
        let t = tables();
        let mut half = 1;
        while half < SUBBANDS {
            let step = SUBBANDS / (2 * half);
            for start in (0..SUBBANDS).step_by(2 * half) {
                for j in 0..half {
                    let (c, s) = t.twiddles[j * step];
                    let (a, b) = (start + j, start + j + half);
                    let tr = self.re[b] * c - self.im[b] * s;
                    let ti = self.re[b] * s + self.im[b] * c;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
            }
            half *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fast analysis is the formula: after enough slots to fill the
    /// window, each band is the folded window modulated by
    /// `e^{iπ(k+½)(n−½)/64}`, computed here directly in `f64`.
    #[test]
    fn the_analysis_is_the_formula() {
        let mut seed = 7u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let signal: Vec<f32> = (0..TAPS + 3 * SUBBANDS).map(|_| noise()).collect();
        let mut bank = Analysis::new();
        let mut out = Slot::default();
        let slots = signal.len() / SUBBANDS;
        for s in 0..slots {
            bank.slot(&signal[s * SUBBANDS..(s + 1) * SUBBANDS], &mut out);
        }
        // The window, newest first, ends at the last sample.
        let end = slots * SUBBANDS;
        let window: Vec<f64> = (0..TAPS).map(|i| f64::from(signal[end - 1 - i])).collect();
        for k in 0..SUBBANDS {
            let (mut re, mut im) = (0f64, 0f64);
            for n in 0..FOLD {
                let g: f64 = (0..TAPS / FOLD)
                    .map(|j| window[FOLD * j + n] * f64::from(PROTOTYPE[FOLD * j + n]))
                    .sum();
                let phase = PI * (k as f64 + 0.5) * (n as f64 - 0.5) / 64.0;
                re += g * phase.cos();
                im += g * phase.sin();
            }
            assert!((f64::from(out.real[k]) - re).abs() < 1e-5, "band {k} real");
            assert!(
                (f64::from(out.imaginary[k]) - im).abs() < 1e-5,
                "band {k} imaginary"
            );
        }
    }

    /// A tone lands in its own band and its neighbour, and not elsewhere:
    /// the bank separates frequency the way the parameter bands assume.
    #[test]
    fn a_tone_lands_in_its_band() {
        let band = 10usize;
        // The centre of band k is (k + ½)·π/64 radians a sample.
        let omega = (band as f32 + 0.5) * std::f32::consts::PI / 64.0;
        let mut bank = Analysis::new();
        let mut out = Slot::default();
        for s in 0..20 {
            let input: Vec<f32> = (0..SUBBANDS)
                .map(|n| ((s * SUBBANDS + n) as f32 * omega).cos())
                .collect();
            bank.slot(&input, &mut out);
        }
        let power: Vec<f32> = (0..SUBBANDS)
            .map(|k| out.real[k] * out.real[k] + out.imaginary[k] * out.imaginary[k])
            .collect();
        let total: f32 = power.iter().sum();
        assert!(power[band] / total > 0.9, "{}", power[band] / total);
    }
}
