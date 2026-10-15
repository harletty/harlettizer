// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from the `eac3` crate of harletty-bridge
// (https://github.com/harletty/harletty-bridge, tag v0.9.0) — Copyright (c)
// its authors — under the Apache License, Version 2.0, whose text is
// reproduced in LICENSES/Apache-2.0.txt.
// What was taken and what was changed is recorded in docs/provenance.md.

//! The 64-band complex QMF bank the decoder runs joint object coding in:
//! its analysis half, and its synthesis half.
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
//!
//! The synthesis half is the decoder's too — the real-linear transpose of the
//! analysis, the same DFT between the transposed rotations, overlap-added
//! through the prototype — so that an encoder can hear what the decoder will
//! make of its gains. Analysis then synthesis hands the input back
//! [`DELAY`] samples late, which a test checks.

use crate::prototype::PROTOTYPE;
use std::f64::consts::PI;

/// Subbands, and samples a slot advances by.
pub const SUBBANDS: usize = 64;
/// Samples analysis then synthesis puts its output behind its input: what
/// the decoder's objects lag the core by.
pub const DELAY: usize = TAPS - SUBBANDS + 1;
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
    /// `ψ_k²·α_k` and `ψ_k²·β_k`: the separation's transpose, into the DFT.
    inverse_alpha: [(f32, f32); SUBBANDS],
    inverse_beta: [(f32, f32); SUBBANDS],
    /// `−e^{iπm/64}/64`: the packing's transpose, out of it.
    unpack: [(f32, f32); SUBBANDS],
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
        let times_psi_squared = |k: usize, (re, im): (f64, f64)| {
            let phase = PI * (2 * k + 1) as f64 / 128.0;
            let (pr, pi) = (phase.cos(), phase.sin());
            ((re * pr - im * pi) as f32, (re * pi + im * pr) as f32)
        };
        let alpha = |k: usize| {
            let (re, im) = psi(k);
            ((re + im) / 2.0, -(re + im) / 2.0)
        };
        let beta = |k: usize| {
            let (re, im) = psi(k);
            ((re - im) / 2.0, (re - im) / 2.0)
        };
        Tables {
            inverse_alpha: std::array::from_fn(|k| times_psi_squared(k, alpha(k))),
            inverse_beta: std::array::from_fn(|k| times_psi_squared(k, beta(k))),
            unpack: std::array::from_fn(|m| {
                let phase = PI * m as f64 / SUBBANDS as f64;
                let scale = -1.0 / SUBBANDS as f64;
                ((phase.cos() * scale) as f32, (phase.sin() * scale) as f32)
            }),
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
        dft(&mut self.re, &mut self.im);
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
}

/// An in-place radix-2 DFT of `re`/`im`, already in bit-reversed order, with
/// the positive exponent.
fn dft(re: &mut [f32; SUBBANDS], im: &mut [f32; SUBBANDS]) {
    let t = tables();
    let mut half = 1;
    while half < SUBBANDS {
        let step = SUBBANDS / (2 * half);
        for start in (0..SUBBANDS).step_by(2 * half) {
            for j in 0..half {
                let (c, s) = t.twiddles[j * step];
                let (a, b) = (start + j, start + j + half);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        half *= 2;
    }
}

/// One synthesis bank: the last 1280 values the modulation produced, newest
/// first, overlap-added through the prototype into 64 samples a slot.
#[derive(Clone)]
pub struct Synthesis {
    history: Box<[f32; 4 * TAPS]>,
    head: usize,
    re: [f32; SUBBANDS],
    im: [f32; SUBBANDS],
}

impl Default for Synthesis {
    fn default() -> Self {
        Self::new()
    }
}

impl Synthesis {
    pub fn new() -> Self {
        Self {
            history: Box::new([0.0; 4 * TAPS]),
            head: 0,
            re: [0.0; SUBBANDS],
            im: [0.0; SUBBANDS],
        }
    }

    /// The 64 samples one slot of subbands synthesises to.
    pub fn slot(&mut self, input: &Slot, out: &mut [f32]) {
        debug_assert_eq!(out.len(), SUBBANDS);
        const RING: usize = 2 * TAPS;
        self.head = if self.head >= FOLD {
            self.head - FOLD
        } else {
            RING + self.head - FOLD
        };
        let t = tables();
        for k in 0..SUBBANDS {
            let mirror = SUBBANDS - 1 - k;
            let (vr, vi) = (input.real[k], input.imaginary[k]);
            let (mr, mi) = (input.real[mirror], input.imaginary[mirror]);
            let (ar, ai) = t.inverse_alpha[k];
            let (br, bi) = t.inverse_beta[mirror];
            // α·value + conj(β·mirrored)
            let (pr, pi) = (br * mr - bi * mi, br * mi + bi * mr);
            let slot = usize::from(t.reverse[k]);
            self.re[slot] = ar * vr - ai * vi + pr;
            self.im[slot] = ar * vi + ai * vr - pi;
        }
        dft(&mut self.re, &mut self.im);
        let head = self.head;
        for m in 0..SUBBANDS {
            let (ur, ui) = t.unpack[m];
            let (zr, zi) = (self.re[m], self.im[m]);
            let (r, i) = (zr * ur - zi * ui, zr * ui + zi * ur);
            for (offset, value) in [(2 * m, r), (2 * m + 1, -i)] {
                self.history[head + offset] = value;
                self.history[head + offset + RING] = value;
            }
        }
        let window = &self.history[head..head + RING];
        for (s, sample) in out.iter_mut().enumerate() {
            let mut value =
                window[s] * PROTOTYPE[s] + window[3 * SUBBANDS + s] * PROTOTYPE[SUBBANDS + s];
            for group in 1..TAPS / FOLD {
                let time = 4 * SUBBANDS * group;
                let coefficient = FOLD * group;
                value += window[time + s] * PROTOTYPE[coefficient + s]
                    + window[time + 3 * SUBBANDS + s] * PROTOTYPE[coefficient + SUBBANDS + s];
            }
            *sample = value;
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

    /// Analysis then synthesis is the identity, `DELAY` samples late: the
    /// bank reconstructs well enough that what an encoder fits through it is
    /// what a decoder will play.
    #[test]
    fn analysis_then_synthesis_gives_the_input_back_late() {
        let mut seed = 21u32;
        let signal: Vec<f32> = (0..40 * SUBBANDS)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        let (mut analysis, mut synthesis) = (Analysis::new(), Synthesis::new());
        let mut slot = Slot::default();
        let mut output = vec![0f32; signal.len()];
        for (input, out) in signal
            .chunks_exact(SUBBANDS)
            .zip(output.chunks_exact_mut(SUBBANDS))
        {
            analysis.slot(input, &mut slot);
            synthesis.slot(&slot, out);
        }
        let (mut power, mut error) = (0f64, 0f64);
        for n in DELAY + TAPS..signal.len() {
            let (x, y) = (f64::from(signal[n - DELAY]), f64::from(output[n]));
            power += x * x;
            error += (y - x) * (y - x);
        }
        let snr = 10.0 * (power / error).log10();
        assert!(snr > 60.0, "the round trip comes back at {snr:.1} dB");
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
