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
/// The DFT's input read as rows of four (see [`dft`]).
const LANES: usize = 4;
const ROWS: usize = SUBBANDS / LANES;

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
    /// The twiddles of the DFT's last two stages: `twiddles[j·32/half]` for
    /// the `j`-th butterfly of each of the stage's blocks, real and imaginary
    /// parts apart, so that four consecutive are read at once.
    last16: Twiddles<16>,
    last32: Twiddles<32>,
}

/// Twiddles copied out of [`Tables::twiddles`] for one of the DFT's stages.
struct Twiddles<const HALF: usize> {
    cos: [f32; HALF],
    sin: [f32; HALF],
}

impl<const HALF: usize> Twiddles<HALF> {
    fn of(twiddles: &[(f32, f32); SUBBANDS / 2]) -> Self {
        let step = SUBBANDS / (2 * HALF);
        Self {
            cos: std::array::from_fn(|j| twiddles[j * step].0),
            sin: std::array::from_fn(|j| twiddles[j * step].1),
        }
    }

    /// The four twiddles from `j`.
    fn four(&self, j: usize) -> ([f32; LANES], [f32; LANES]) {
        (
            self.cos[j..j + LANES].try_into().expect("four"),
            self.sin[j..j + LANES].try_into().expect("four"),
        )
    }
}

fn tables() -> &'static Tables {
    static TABLES: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let c = |phase: f64| (phase.cos() as f32, phase.sin() as f32);
        let psi = |k: usize| {
            let phase = PI * (2 * k + 1) as f64 / 256.0;
            (phase.cos(), phase.sin())
        };
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
        let twiddles: [(f32, f32); SUBBANDS / 2] =
            std::array::from_fn(|k| c(2.0 * PI * k as f64 / SUBBANDS as f64));
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
            last16: Twiddles::of(&twiddles),
            last32: Twiddles::of(&twiddles),
            twiddles,
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
        let head = self.head;
        let (front, back) = self.history.split_at_mut(TAPS);
        let newest = front[head..head + SUBBANDS]
            .iter_mut()
            .zip(&mut back[head..head + SUBBANDS]);
        for ((front, back), sample) in newest.zip(input.iter().rev()) {
            *front = *sample;
            *back = *sample;
        }
        let window = &self.history[head..head + TAPS];

        let fold: [f32; FOLD] = std::array::from_fn(|n| {
            (0..TAPS / FOLD)
                .map(|j| window[FOLD * j + n] * PROTOTYPE[FOLD * j + n])
                .sum()
        });

        let t = tables();
        for m in 0..SUBBANDS {
            let (pr, pi) = t.pack[m];
            let (a, b) = (fold[2 * m], fold[2 * m + 1]);
            self.re[m] = a * pr - b * pi;
            self.im[m] = a * pi + b * pr;
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

/// An in-place 64-point DFT of `re`/`im` with the positive exponent, in
/// order: `X[k] = Σ x[n]·e^{+2πink/64}`, taken the radix-2 way — the input
/// to place `rev(n)`, `rev` reversing six bits, then six stages of
/// butterflies, the `s`-th pairing places that differ in bit `s − 1`.
///
/// Every butterfly is the one that plain radix-2 DFT takes, its products and
/// sums the same and its twiddle the same, and every place goes through its
/// stages in the same order: a stage's butterflies each read and write their
/// own two places, so taking some side by side, or one quarter of the last
/// two stages before another, changes no value. Read as sixteen rows of
/// four, input `n` is in row `n / 4` and lane `n % 4`, and place `rev(n)` puts
/// the lane in the place's top two bits: the first four stages pair places
/// whose top bits agree, so each of their butterflies pairs two rows, lane
/// by lane, by one twiddle. The last two pair places differing in those
/// bits. So four rows, those whose places agree in bits two and three, are
/// turned so that each row holds four places consecutive, and each of the
/// last two stages' butterflies pairs two of these, lane by lane, by four
/// twiddles consecutive. What comes out is in order.
fn dft(re: &mut [f32; SUBBANDS], im: &mut [f32; SUBBANDS]) {
    let t = tables();
    for &(a, b, w) in &ROW_BUTTERFLIES {
        let (c, s) = t.twiddles[usize::from(w)];
        let (a, b) = (LANES * usize::from(a), LANES * usize::from(b));
        let (lo_re, hi_re) = re.split_at_mut(b);
        let (lo_im, hi_im) = im.split_at_mut(b);
        let ra: &mut [f32; LANES] = (&mut lo_re[a..a + LANES]).try_into().expect("a row");
        let ia: &mut [f32; LANES] = (&mut lo_im[a..a + LANES]).try_into().expect("a row");
        let rb: &mut [f32; LANES] = (&mut hi_re[..LANES]).try_into().expect("a row");
        let ib: &mut [f32; LANES] = (&mut hi_im[..LANES]).try_into().expect("a row");
        for lane in 0..LANES {
            butterfly(
                &mut ra[lane],
                &mut ia[lane],
                &mut rb[lane],
                &mut ib[lane],
                c,
                s,
            );
        }
    }
    // Each quarter is read from the rows its places are in and written to
    // those its places go to, the same for quarters nought and three;
    // quarters one and two trade rows, so both are read before either is
    // written.
    for quarter in [0, 3] {
        let turned = last_two(re, im, t, quarter);
        put(re, im, quarter, &turned);
    }
    let (one, two) = (last_two(re, im, t, 1), last_two(re, im, t, 2));
    put(re, im, 1, &one);
    put(re, im, 2, &two);
}

/// A quarter's four rows for the DFT's last two stages, through them: the
/// places whose bits two and three are the quarter's nought and one.
#[inline(always)]
fn last_two(
    re: &[f32; SUBBANDS],
    im: &[f32; SUBBANDS],
    t: &Tables,
    quarter: usize,
) -> [[[f32; LANES]; LANES]; 2] {
    // A place with bits `b0` to `b5` is in row `8·b0 + 4·b1 + 2·b2 + b3`,
    // lane `2·b4 + b5`: so turned row `2·b4 + b5` holds the quarter's four
    // places with those bits four and five, bits nought and one running
    // through its lanes as `b0 + 2·b1`.
    let (two, three) = (quarter & 1, quarter >> 1);
    let turn = |rows: &[f32; SUBBANDS]| -> [[f32; LANES]; LANES] {
        std::array::from_fn(|lane| {
            std::array::from_fn(|r| {
                rows[LANES * (8 * (r & 1) + 4 * (r >> 1) + 2 * two + three) + lane]
            })
        })
    };
    let [mut turned_re, mut turned_im] = [turn(re), turn(im)];
    // Each turned row's twiddles run from `4·quarter`, and from 16 more in
    // the last stage for bit four.
    let j = LANES * quarter;
    for (a, b, (c, s)) in [
        (0, 2, t.last16.four(j)),
        (1, 3, t.last16.four(j)),
        (0, 1, t.last32.four(j)),
        (2, 3, t.last32.four(16 + j)),
    ] {
        let ([ra, rb], [ia, ib]) = (
            turned_re.get_disjoint_mut([a, b]).expect("two rows"),
            turned_im.get_disjoint_mut([a, b]).expect("two rows"),
        );
        for lane in 0..LANES {
            butterfly(
                &mut ra[lane],
                &mut ia[lane],
                &mut rb[lane],
                &mut ib[lane],
                c[lane],
                s[lane],
            );
        }
    }
    [turned_re, turned_im]
}

/// A quarter [`last_two`] took through, to its places in order.
#[inline(always)]
fn put(
    re: &mut [f32; SUBBANDS],
    im: &mut [f32; SUBBANDS],
    quarter: usize,
    [turned_re, turned_im]: &[[[f32; LANES]; LANES]; 2],
) {
    for (r, (turned_re, turned_im)) in turned_re.iter().zip(turned_im).enumerate() {
        let at = LANES * (quarter + 4 * (r >> 1) + 8 * (r & 1));
        re[at..at + LANES].copy_from_slice(turned_re);
        im[at..at + LANES].copy_from_slice(turned_im);
    }
}

/// The butterflies of the DFT's first four stages, stage by stage: the two
/// rows each pairs and its twiddle's index. They are the plain radix-2 DFT's
/// for the places below sixteen, one a row, each standing for the four of
/// its rows' lanes, whose places differ from it only in the top two bits and
/// so take the same twiddle.
const ROW_BUTTERFLIES: [(u8, u8, u8); 4 * ROWS / 2] = {
    let mut rows = [(0, 0, 0); 4 * ROWS / 2];
    let mut at = 0;
    let mut half = 1;
    while half <= 8 {
        let step = SUBBANDS / (2 * half);
        let mut start = 0;
        while start < ROWS {
            let mut j = 0;
            while j < half {
                rows[at] = (
                    (reversed(start + j) / LANES) as u8,
                    (reversed(start + j + half) / LANES) as u8,
                    (j * step) as u8,
                );
                at += 1;
                j += 1;
            }
            start += 2 * half;
        }
        half *= 2;
    }
    rows
};

/// Six bits reversed.
const fn reversed(i: usize) -> usize {
    ((i as u32).reverse_bits() >> (32 - SUBBANDS.trailing_zeros())) as usize
}

/// The radix-2 butterfly on a pair of places, by the twiddle `c + is`.
#[inline(always)]
fn butterfly(ra: &mut f32, ia: &mut f32, rb: &mut f32, ib: &mut f32, c: f32, s: f32) {
    let tr = *rb * c - *ib * s;
    let ti = *rb * s + *ib * c;
    *rb = *ra - tr;
    *ib = *ia - ti;
    *ra += tr;
    *ia += ti;
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
            self.re[k] = ar * vr - ai * vi + pr;
            self.im[k] = ar * vi + ai * vr - pi;
        }
        dft(&mut self.re, &mut self.im);
        let head = self.head;
        let (front, back) = self.history.split_at_mut(RING);
        let pairs = front[head..head + FOLD]
            .chunks_exact_mut(2)
            .zip(back[head..head + FOLD].chunks_exact_mut(2));
        for (m, (front, back)) in pairs.enumerate() {
            let (ur, ui) = t.unpack[m];
            let (zr, zi) = (self.re[m], self.im[m]);
            let (r, i) = (zr * ur - zi * ui, zr * ui + zi * ur);
            for pair in [front, back] {
                pair[0] = r;
                pair[1] = -i;
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

    /// The bank as it was before its DFT took butterflies side by side, kept
    /// as the oracle the bank is held to: the input bit-reversed into place
    /// and each stage's butterflies taken one at a time, and each of the
    /// slots' loops a sample at a time.
    mod plain {
        use super::super::{FOLD, PROTOTYPE, SUBBANDS, Slot, TAPS, tables};

        fn reverse(m: usize) -> usize {
            ((m as u32).reverse_bits() >> (32 - SUBBANDS.trailing_zeros())) as usize
        }

        /// An in-place radix-2 DFT of `re`/`im`, already in bit-reversed
        /// order, with the positive exponent.
        pub fn dft(re: &mut [f32; SUBBANDS], im: &mut [f32; SUBBANDS]) {
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

        /// [`dft`] of an input in order.
        pub fn dft_in_order(re: &mut [f32; SUBBANDS], im: &mut [f32; SUBBANDS]) {
            let (input_re, input_im) = (*re, *im);
            for n in 0..SUBBANDS {
                re[reverse(n)] = input_re[n];
                im[reverse(n)] = input_im[n];
            }
            dft(re, im);
        }

        pub struct Analysis {
            history: Box<[f32; 2 * TAPS]>,
            head: usize,
            re: [f32; SUBBANDS],
            im: [f32; SUBBANDS],
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

            pub fn slot(&mut self, input: &[f32], out: &mut Slot) {
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
                    let slot = reverse(m);
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

        pub struct Synthesis {
            history: Box<[f32; 4 * TAPS]>,
            head: usize,
            re: [f32; SUBBANDS],
            im: [f32; SUBBANDS],
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

            pub fn slot(&mut self, input: &Slot, out: &mut [f32]) {
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
                    let (pr, pi) = (br * mr - bi * mi, br * mi + bi * mr);
                    let slot = reverse(k);
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
                    let mut value = window[s] * PROTOTYPE[s]
                        + window[3 * SUBBANDS + s] * PROTOTYPE[SUBBANDS + s];
                    for group in 1..TAPS / FOLD {
                        let time = 4 * SUBBANDS * group;
                        let coefficient = FOLD * group;
                        value += window[time + s] * PROTOTYPE[coefficient + s]
                            + window[time + 3 * SUBBANDS + s]
                                * PROTOTYPE[coefficient + SUBBANDS + s];
                    }
                    *sample = value;
                }
            }
        }
    }

    /// The same value: the same bits, or both NaN — which NaN an operation
    /// on two of them gives is not something Rust promises.
    fn same(a: f32, b: f32) -> bool {
        a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
    }

    /// Values a sum can be thrown by, beside ordinary ones: both zeros,
    /// both infinities, NaN, the least normal, a subnormal, and ones whose
    /// products and sums overflow.
    const AWKWARD: [f32; 10] = [
        0.0,
        -0.0,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        f32::MIN_POSITIVE,
        -1e-40,
        f32::MAX,
        -3e38,
        1e30,
    ];

    /// A generator of ordinary values in [−½, ½), and now and then, at a
    /// rate of `awkward` in 256, one of [`AWKWARD`].
    fn values(seed: u32, awkward: u32) -> impl FnMut() -> f32 {
        let mut seed = seed;
        move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let ordinary = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            if (seed >> 16) & 255 < awkward {
                AWKWARD[(seed >> 24) as usize % AWKWARD.len()]
            } else {
                ordinary
            }
        }
    }

    /// The DFT is the plain radix-2 one to the bit: every butterfly the
    /// same, so every output the same, signed zeros, infinities and NaN
    /// included.
    #[test]
    fn the_dft_is_the_plain_one_to_the_bit() {
        for (trial, awkward) in [0, 4, 32, 128, 256]
            .into_iter()
            .cycle()
            .take(5000)
            .enumerate()
        {
            let mut next = values(trial as u32, awkward);
            let mut re: [f32; SUBBANDS] = std::array::from_fn(|_| next());
            let mut im: [f32; SUBBANDS] = std::array::from_fn(|_| next());
            // Some all one value: every butterfly then sums equal values.
            if trial % 7 == 0 {
                let v = next();
                re = [v; SUBBANDS];
                im = [AWKWARD[trial % AWKWARD.len()]; SUBBANDS];
            }
            let (mut plain_re, mut plain_im) = (re, im);
            dft(&mut re, &mut im);
            plain::dft_in_order(&mut plain_re, &mut plain_im);
            for k in 0..SUBBANDS {
                assert!(
                    same(re[k], plain_re[k]) && same(im[k], plain_im[k]),
                    "trial {trial}, bin {k}: {} + {}i, not {} + {}i",
                    re[k],
                    im[k],
                    plain_re[k],
                    plain_im[k]
                );
            }
        }
    }

    /// Both banks are the plain ones to the bit, slot after slot: on noise,
    /// on noise with awkward values through it, and on runs of either zero.
    #[test]
    fn the_banks_are_the_plain_ones_to_the_bit() {
        for (seed, awkward) in [(1, 0), (2, 1), (3, 16), (4, 256)] {
            let mut next = values(seed, awkward);
            let (mut analysis, mut plain_analysis) = (Analysis::new(), plain::Analysis::new());
            let (mut synthesis, mut plain_synthesis) = (Synthesis::new(), plain::Synthesis::new());
            let (mut slot, mut plain_slot) = (Slot::default(), Slot::default());
            let (mut out, mut plain_out) = ([0f32; SUBBANDS], [0f32; SUBBANDS]);
            for s in 0..200 {
                let input: [f32; SUBBANDS] = match s / 25 % 4 {
                    1 => [-0.0; SUBBANDS],
                    2 => std::array::from_fn(|n| if n % 2 == 0 { 0.0 } else { -0.0 }),
                    _ => std::array::from_fn(|_| next()),
                };
                analysis.slot(&input, &mut slot);
                plain_analysis.slot(&input, &mut plain_slot);
                for k in 0..SUBBANDS {
                    assert!(
                        same(slot.real[k], plain_slot.real[k])
                            && same(slot.imaginary[k], plain_slot.imaginary[k]),
                        "seed {seed}, analysis slot {s}, band {k}"
                    );
                }
                // The synthesis bank on what the analysis gave, and on a slot
                // of its own now and then.
                let given = if s % 3 == 0 {
                    Slot {
                        real: std::array::from_fn(|_| next()),
                        imaginary: std::array::from_fn(|_| next()),
                    }
                } else {
                    slot
                };
                synthesis.slot(&given, &mut out);
                plain_synthesis.slot(&given, &mut plain_out);
                for n in 0..SUBBANDS {
                    assert!(
                        same(out[n], plain_out[n]),
                        "seed {seed}, synthesis slot {s}, sample {n}"
                    );
                }
            }
        }
    }

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
