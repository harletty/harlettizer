// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/dcaadpcm.c, dcaadpcm.h — Copyright (C) 2017 Daniil Cherednik
//   libavcodec/dca_core.h (ff_dca_core_dequantize), dcamath.h — Copyright (C) 2016 foo86
// What was taken and what was changed is recorded in docs/provenance.md.

//! Subband ADPCM: choosing a predictor, and coding against it exactly as the
//! decoder will reconstruct.
//!
//! A core subband may be coded as the error of a fourth-order prediction from
//! its own past, the predictor named by a 12-bit index into a fixed codebook
//! of 4096. Choosing one is a search over the codebook for the smallest
//! prediction error on the frame's samples; using one means quantising each
//! error and feeding the *dequantised* value back, so that the encoder's
//! history is the decoder's.

use crate::tables::{ADPCM_COEFFS, ADPCM_VB, SoftFloat};

/// Subband samples per frame.
const LEN: usize = 16;

/// `norm__`: round away `bits` bits; nothing when `bits` is not positive.
/// The result is truncated to 32 bits, as FFmpeg's is.
#[inline(always)]
pub(crate) fn norm(a: i64, bits: i32) -> i32 {
    if bits > 0 {
        ((a + (1i64 << (bits - 1))) >> bits) as i32
    } else {
        a as i32
    }
}

/// `mul32`: the high half of a 32 × 32 product, rounded.
#[inline(always)]
pub(crate) fn mul32(a: i32, b: i32) -> i32 {
    norm(i64::from(a) * i64::from(b), 32)
}

/// `clip23`: saturate to 24 bits signed.
#[inline(always)]
fn clip23(a: i32) -> i32 {
    a.clamp(-(1 << 23), (1 << 23) - 1)
}

/// `av_log2`: the index of the highest set bit, nought for nought.
#[inline(always)]
pub(crate) fn log2(x: u32) -> i32 {
    31 - (x | 1).leading_zeros() as i32
}

/// `quantize_value`: `value · quant`, rounded to the nearest level.
#[inline(always)]
pub(crate) fn quantize_value(value: i32, quant: SoftFloat) -> i32 {
    let offset = 1i32 << (quant.e - 1);
    mul32(value, quant.m).wrapping_add(offset) >> quant.e
}

/// `ff_dca_core_dequantize` without the residual form: what the decoder
/// makes of quantised values, at the step size and scale factor they were
/// coded with.
pub(crate) fn dequantize(output: &mut [i32], input: &[i32], step_size: i32, scale: i32) {
    let mut step_scale = i64::from(step_size) * i64::from(scale);
    let mut shift = 0;
    if step_scale > 1 << 23 {
        shift = log2((step_scale >> 23) as u32) + 1;
        step_scale >>= shift;
    }
    for (o, &i) in output.iter_mut().zip(input) {
        *o = clip23(norm(i64::from(i) * step_scale, 22 - shift));
    }
}

/// `ff_dcaadpcm_predict`: the prediction of the sample after `input[0..4]`.
#[inline(always)]
pub(crate) fn predict(vq: usize, input: &[i32]) -> i32 {
    let coeff = &ADPCM_VB[vq];
    let mut pred = 0i64;
    for (i, &c) in coeff.iter().enumerate() {
        pred += i64::from(input[ADPCM_COEFFS - 1 - i]) * i64::from(c);
    }
    clip23(norm(pred, 13))
}

/// The predictor search, with each codebook entry's pairwise products
/// computed once.
#[derive(Debug, Clone)]
pub struct Adpcm {
    /// `premultiplied_coeffs`: `a_j · a_k` for `j ≤ k`, doubled off the
    /// diagonal.
    premultiplied: Box<[[i32; 10]; 4096]>,
}

impl Default for Adpcm {
    fn default() -> Self {
        Self::new()
    }
}

impl Adpcm {
    pub fn new() -> Self {
        let mut premultiplied = Box::new([[0i32; 10]; 4096]);
        for (row, vb) in premultiplied.iter_mut().zip(ADPCM_VB.iter()) {
            let mut id = 0;
            for j in 0..ADPCM_COEFFS {
                for k in j..ADPCM_COEFFS {
                    let mut t = i32::from(vb[j]) * i32::from(vb[k]);
                    if j != k {
                        t *= 2;
                    }
                    row[id] = t;
                    id += 1;
                }
            }
        }
        Self { premultiplied }
    }

    /// `ff_dcaadpcm_subband_analysis`: the predictor for a subband, or `None`
    /// when the prediction gains less than 10 dB. `input` is the four
    /// samples of history followed by the frame's sixteen; `diff` receives
    /// the prediction error the choice was made on, scaled as the subband is.
    pub fn subband_analysis(
        &self,
        input: &[i32; LEN + ADPCM_COEFFS],
        diff: &mut [i32; LEN],
    ) -> Option<u16> {
        let mut max = 0u32;
        for &x in input {
            max |= x.wrapping_abs() as u32;
        }
        // Normalise the input to keep the search's products in range.
        let shift_bits = log2(max) - 11;
        let mut input_buffer = [0i32; LEN + ADPCM_COEFFS];
        let mut input_buffer2 = [0i32; LEN + ADPCM_COEFFS];
        for (i, &x) in input.iter().enumerate() {
            input_buffer[i] = norm(i64::from(x), 7);
            input_buffer2[i] = norm(i64::from(x), shift_bits);
        }
        let vq = self.find_best_filter(&input_buffer2);
        // FFmpeg's gain is unsigned there, so a prediction without error —
        // a silent subband — reads as an infinite gain and is taken.
        let gain = prediction_gain(vq, &input_buffer, diff) as u64;
        if gain < 10 {
            return None;
        }
        for d in diff.iter_mut() {
            *d <<= 7;
        }
        Some(vq as u16)
    }

    /// `find_best_filter`: the codebook entry with the least prediction
    /// error energy, by the autocorrelation.
    fn find_best_filter(&self, input: &[i32; LEN + ADPCM_COEFFS]) -> usize {
        let mut corr = [0i64; 15];
        let mut k = 0;
        for i in 0..=ADPCM_COEFFS {
            for j in i..=ADPCM_COEFFS {
                let mut s = 0i64;
                for n in 0..LEN {
                    s += i64::from(input[ADPCM_COEFFS + n - i])
                        * i64::from(input[ADPCM_COEFFS + n - j]);
                }
                corr[k] = s;
                k += 1;
            }
        }
        let mut vq = 0;
        let mut min_err = 1i64 << 62;
        for (i, (a, aa)) in ADPCM_VB.iter().zip(self.premultiplied.iter()).enumerate() {
            let err = apply_filter(a, &corr, aa);
            if err < min_err {
                min_err = err;
                vq = i;
            }
        }
        vq
    }
}

/// `apply_filter`: the prediction error energy of one predictor, from the
/// autocorrelation.
#[inline(always)]
fn apply_filter(a: &[i16; ADPCM_COEFFS], corr: &[i64; 15], aa: &[i32; 10]) -> i64 {
    let mut err = corr[0];
    let mut tmp = 0i64;
    for i in 0..ADPCM_COEFFS {
        tmp += i64::from(a[i]) * corr[1 + i];
    }
    let tmp = i64::from(norm(tmp, 13));
    err -= tmp + tmp;
    let mut tmp = 0i64;
    for i in 0..10 {
        tmp += corr[5 + i] * i64::from(aa[i]);
    }
    err += i64::from(norm(tmp, 26));
    err.abs()
}

/// `calc_prediction_gain`: signal energy over error energy, and the error;
/// −1 when there is no error at all.
fn prediction_gain(vq: usize, input: &[i32; LEN + ADPCM_COEFFS], out: &mut [i32; LEN]) -> i64 {
    let mut signal = 0i64;
    let mut error_energy = 0i64;
    for i in 0..LEN {
        let error = input[ADPCM_COEFFS + i].wrapping_sub(predict(vq, &input[i..]));
        out[i] = error;
        signal += i64::from(input[ADPCM_COEFFS + i]) * i64::from(input[ADPCM_COEFFS + i]);
        error_energy += i64::from(error) * i64::from(error);
    }
    if error_energy == 0 {
        return -1;
    }
    signal / error_energy
}

/// `ff_dcaadpcm_do_real`: quantise a subband's prediction error, the
/// prediction made from the reconstructed past, and return the history the
/// next frame starts from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn code_subband(
    vq: usize,
    quant: SoftFloat,
    scale_factor: i32,
    step_size: i32,
    prev_hist: &[i32; ADPCM_COEFFS],
    input: &[i32],
    next_hist: &mut [i32; ADPCM_COEFFS],
    out: &mut [i32; LEN],
    peak: i32,
) {
    let mut work = [0i32; LEN + ADPCM_COEFFS];
    work[..ADPCM_COEFFS].copy_from_slice(prev_hist);
    for i in 0..LEN {
        work[ADPCM_COEFFS + i] = predict(vq, &work[i..]);
        let delta = i64::from(input[i]) - (i64::from(work[ADPCM_COEFFS + i]) << 7);
        out[i] = quantize_value(delta.clamp(-i64::from(peak), i64::from(peak)) as i32, quant);
        let mut dequantised = [0i32; 1];
        dequantize(&mut dequantised, &out[i..=i], step_size, scale_factor);
        work[ADPCM_COEFFS + i] = work[ADPCM_COEFFS + i].wrapping_add(dequantised[0]);
    }
    next_hist.copy_from_slice(&work[LEN..]);
}
