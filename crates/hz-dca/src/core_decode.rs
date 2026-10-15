// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/dca_core.c, dcadsp.c, dcadct.c, dcamath.h — Copyright (C) 2016 foo86
//   libavcodec/synth_filter.c — Copyright (c) 2008 Michael Niedermayer,
//     (C) 2016 foo86
// What was taken and what was changed is recorded in docs/provenance.md.

//! The core as a decoder reconstructs it: the fixed-point path a lossless
//! decoder adds its residuals to.
//!
//! A lossless channel coded as a residual is the difference between the
//! source and the core's own reconstruction of that channel, so the encoder
//! has to know that reconstruction to the last bit — not the core encoder's
//! idea of it, but what a decoder computes from the frame it is sent. This is
//! that computation, FFmpeg's `dca_core.c` and `dcadsp.c` rounding for
//! rounding on the path its lossless decoder takes
//! (`ff_dca_core_filter_fixed`): each subband dequantised, ADPCM undone from
//! the decoder's own history, the 32-band synthesis through the fixed-point
//! cosine modulation of `dcadct.c` and the 512-tap window of
//! `synth_filter.c`, and the LFE interpolated 64 times.
//!
//! It reads the frame from the [`Encoder`] that has just written it — the
//! allocation codes, scale factor indices, predictors and quantised values
//! as the stream carries them — rather than parsing the bytes: the same
//! values, without a Huffman decoder. It handles what that encoder writes:
//! one subframe of two subsubframes, every subband coded, no transient
//! modes, joint intensity or high-frequency VQ, the non-perfect filter bank
//! and the LFE decimated 64 times.
//!
//! The state — each channel's subband history, its synthesis buffers and the
//! LFE's history — lives as long as the stream, and nothing allocates per
//! frame. It has to see every frame the stream carries, from the first, as
//! a decoder does.

use crate::adpcm::{clip23, dequantize, norm, predict};
use crate::encoder::{Encoder, FRAME_SAMPLES};
use crate::tables::{
    ADPCM_COEFFS, FIR_32BANDS_NONPERFECT_FIXED, LFE_FIR_64_FIXED, LOSSLESS_QUANT, LOSSY_QUANT,
    SCALE_FACTOR_QUANT7,
};

const SUBBANDS: usize = 32;
/// Subband samples per frame (`npcmblocks`).
const BLOCKS: usize = 16;
/// The most coded channels a core has, the LFE apart.
const MAX_CODED: usize = 5;
const LFE_SAMPLES: usize = 8;
/// LFE samples a decoder keeps from frame to frame (`DCA_LFE_HISTORY`).
const LFE_HISTORY: usize = 8;

/// One channel's synthesis filter state (`hist1`, `offset`, `hist2`).
#[derive(Clone)]
struct Synthesis {
    buffer: [i32; 512],
    offset: usize,
    overlap: [i32; 32],
}

/// The core decoder's state, and the frame it last reconstructed.
pub struct CoreDecoder {
    /// Per coded channel and subband: the four samples the frame's ADPCM
    /// starts from, then the frame's sixteen.
    subband: [[[i32; ADPCM_COEFFS + BLOCKS]; SUBBANDS]; MAX_CODED],
    synthesis: [Synthesis; MAX_CODED],
    /// The LFE's history, then the frame's samples.
    lfe: [i32; LFE_HISTORY + LFE_SAMPLES],
    /// The reconstruction, per input channel of the core encoder (WAV order),
    /// in the decoder's 24-bit range.
    output: [[i32; FRAME_SAMPLES]; MAX_CODED + 1],
}

impl Default for CoreDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreDecoder {
    /// A decoder that has seen nothing, as at the start of a stream.
    pub fn new() -> Self {
        Self {
            subband: [[[0; ADPCM_COEFFS + BLOCKS]; SUBBANDS]; MAX_CODED],
            synthesis: std::array::from_fn(|_| Synthesis {
                buffer: [0; 512],
                offset: 0,
                overlap: [0; 32],
            }),
            lfe: [0; LFE_HISTORY + LFE_SAMPLES],
            output: [[0; FRAME_SAMPLES]; MAX_CODED + 1],
        }
    }

    /// Reconstruct the frame `encoder` last wrote.
    pub fn decode(&mut self, encoder: &Encoder) {
        let steps = if encoder.lossless_steps() {
            &LOSSLESS_QUANT
        } else {
            &LOSSY_QUANT
        };
        for ch in 0..encoder.coded_channels() {
            for (band, samples) in self.subband[ch].iter_mut().enumerate() {
                reconstruct_subband(encoder, ch, band, steps, samples);
            }
            let slot = encoder.input_channel(ch);
            let (subbands, synthesis) = (&self.subband[ch], &mut self.synthesis[ch]);
            for (block, out) in self.output[slot].chunks_exact_mut(32).enumerate() {
                let input: [i32; SUBBANDS] =
                    std::array::from_fn(|b| subbands[b][ADPCM_COEFFS + block]);
                synthesis.filter(&input, out.try_into().expect("32 samples"));
            }
            // What the next frame's prediction starts from.
            for samples in &mut self.subband[ch] {
                samples.copy_within(BLOCKS.., 0);
            }
        }
        if let Some((codes, scale_index)) = encoder.lfe() {
            self.decode_lfe(codes, scale_index, encoder.lfe_channel());
        }
    }

    /// The reconstruction of input channel `channel` (in the core encoder's
    /// WAV order), in the 24-bit range the decoder hands a lossless layer.
    pub fn channel(&self, channel: usize) -> &[i32; FRAME_SAMPLES] {
        &self.output[channel]
    }

    /// The LFE: its eight samples scaled (`parse_subframe_audio`), then
    /// interpolated 64 times (`lfe_fir_fixed_c`).
    fn decode_lfe(&mut self, codes: &[i32; LFE_SAMPLES], scale_index: usize, slot: usize) {
        // The quantiser's step, 0.035, in Q27.
        let scale = mul23(4_697_620, SCALE_FACTOR_QUANT7[scale_index] as i32);
        for (sample, &code) in self.lfe[LFE_HISTORY..].iter_mut().zip(codes) {
            // Eight bits in the stream, signed.
            let code = i32::from(code as i8);
            *sample = clip23(code.wrapping_mul(scale) >> 4);
        }
        let out = &mut self.output[slot];
        for i in 0..LFE_SAMPLES {
            let at = LFE_HISTORY + i;
            for j in 0..32 {
                let (mut a, mut b) = (0i64, 0i64);
                for k in 0..8 {
                    let x = i64::from(self.lfe[at - k]);
                    a += i64::from(LFE_FIR_64_FIXED[j * 8 + k]) * x;
                    b += i64::from(LFE_FIR_64_FIXED[255 - j * 8 - k]) * x;
                }
                out[i * 64 + j] = clip23(norm(a, 23));
                out[i * 64 + 32 + j] = clip23(norm(b, 23));
            }
        }
        self.lfe.copy_within(LFE_SAMPLES.., 0);
    }
}

/// One subband of one channel: dequantised (`ff_dca_core_dequantize`), then
/// its ADPCM undone (`inverse_adpcm`) from the four samples before it.
fn reconstruct_subband(
    encoder: &Encoder,
    ch: usize,
    band: usize,
    steps: &[u32; 32],
    samples: &mut [i32; ADPCM_COEFFS + BLOCKS],
) {
    let coded = encoder.subband(ch, band);
    let frame = &mut samples[ADPCM_COEFFS..];
    if coded.abits == 0 {
        // No samples and no scale factor: nought.
        frame.fill(0);
    } else {
        let abits = coded.abits as usize;
        let mut scale = SCALE_FACTOR_QUANT7[coded.scale_index] as i32;
        if coded.huffman {
            // The scale factor adjustment the frame sends with a Huffman
            // codebook is index 0, a factor of one in Q22.
            scale = clip23(((i64::from(scale) << 22) >> 22) as i32);
        }
        let mut values = [0i32; BLOCKS];
        values.copy_from_slice(coded.samples);
        if coded.linear {
            // Read back as `abits − 3` bits, signed.
            let width = abits as u32 - 3;
            for v in &mut values {
                *v = (*v << (32 - width)) >> (32 - width);
            }
        }
        dequantize(frame, &values, steps[abits] as i32, scale);
    }
    if let Some(vq) = coded.predictor {
        for j in 0..BLOCKS {
            let x = predict(vq, &samples[j..j + ADPCM_COEFFS]);
            samples[ADPCM_COEFFS + j] = clip23(samples[ADPCM_COEFFS + j] + x);
        }
    }
}

/// `mul23`: a product rounded to 23 fractional bits.
#[inline(always)]
fn mul23(a: i32, b: i32) -> i32 {
    norm(i64::from(a) * i64::from(b), 23)
}

impl Synthesis {
    /// `synth_filter_fixed`: 32 subband samples to 32 output samples, the
    /// cosine modulation into the ring buffer and the window across it.
    fn filter(&mut self, input: &[i32; 32], out: &mut [i32; 32]) {
        let window = &FIR_32BANDS_NONPERFECT_FIXED;
        let at = self.offset;
        imdct_half_32(
            (&mut self.buffer[at..at + 32])
                .try_into()
                .expect("32 samples"),
            input,
        );
        let buf = &self.buffer;
        for i in 0..16 {
            let mut a = i64::from(self.overlap[i]) << 21;
            let mut b = i64::from(self.overlap[i + 16]) << 21;
            let mut c = 0i64;
            let mut d = 0i64;
            let mut tap = |j: usize, base: usize| {
                a += i64::from(window[i + j]) * i64::from(buf[base + i]);
                b += i64::from(window[i + j + 16]) * i64::from(buf[base + 15 - i]);
                c += i64::from(window[i + j + 32]) * i64::from(buf[base + 16 + i]);
                d += i64::from(window[i + j + 48]) * i64::from(buf[base + 31 - i]);
            };
            // The window runs from the newest block to the buffer's end, then
            // wraps to its start.
            let mut j = 0;
            while j < 512 - at {
                tap(j, at + j);
                j += 64;
            }
            while j < 512 {
                tap(j, at + j - 512);
                j += 64;
            }
            out[i] = clip23(norm(a, 21));
            out[i + 16] = clip23(norm(b, 21));
            self.overlap[i] = norm(c, 21);
            self.overlap[i + 16] = norm(d, 21);
        }
        self.offset = (at + 512 - 32) & 511;
    }
}

// `dcadct.c`: the 32-point cosine modulation in fixed point, its stages
// clipped to 24 bits between them as the decoder clips them.

fn sum_a(input: &[i32], output: &mut [i32], len: usize) {
    for i in 0..len {
        output[i] = input[2 * i].wrapping_add(input[2 * i + 1]);
    }
}

fn sum_b(input: &[i32], output: &mut [i32], len: usize) {
    output[0] = input[0];
    for i in 1..len {
        output[i] = input[2 * i].wrapping_add(input[2 * i - 1]);
    }
}

fn sum_c(input: &[i32], output: &mut [i32], len: usize) {
    for i in 0..len {
        output[i] = input[2 * i];
    }
}

fn sum_d(input: &[i32], output: &mut [i32], len: usize) {
    output[0] = input[1];
    for i in 1..len {
        output[i] = input[2 * i - 1].wrapping_add(input[2 * i + 1]);
    }
}

fn dct_a(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [[i32; 8]; 8] = [
        [
            8348215, 8027397, 7398092, 6484482, 5321677, 3954362, 2435084, 822227,
        ],
        [
            8027397, 5321677, 822227, -3954362, -7398092, -8348215, -6484482, -2435084,
        ],
        [
            7398092, 822227, -6484482, -8027397, -2435084, 5321677, 8348215, 3954362,
        ],
        [
            6484482, -3954362, -8027397, 822227, 8348215, 2435084, -7398092, -5321677,
        ],
        [
            5321677, -7398092, -2435084, 8348215, -822227, -8027397, 3954362, 6484482,
        ],
        [
            3954362, -8348215, 5321677, 2435084, -8027397, 6484482, 822227, -7398092,
        ],
        [
            2435084, -6484482, 8348215, -7398092, 3954362, 822227, -5321677, 8027397,
        ],
        [
            822227, -2435084, 3954362, -5321677, 6484482, -7398092, 8027397, -8348215,
        ],
    ];
    for (out, row) in output.iter_mut().zip(&COS_MOD) {
        let res: i64 = row
            .iter()
            .zip(input)
            .map(|(&c, &x)| i64::from(c) * i64::from(x))
            .sum();
        *out = norm(res, 23);
    }
}

fn dct_b(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [[i32; 7]; 8] = [
        [
            8227423, 7750063, 6974873, 5931642, 4660461, 3210181, 1636536,
        ],
        [
            6974873, 3210181, -1636536, -5931642, -8227423, -7750063, -4660461,
        ],
        [
            4660461, -3210181, -8227423, -5931642, 1636536, 7750063, 6974873,
        ],
        [
            1636536, -7750063, -4660461, 5931642, 6974873, -3210181, -8227423,
        ],
        [
            -1636536, -7750063, 4660461, 5931642, -6974873, -3210181, 8227423,
        ],
        [
            -4660461, -3210181, 8227423, -5931642, -1636536, 7750063, -6974873,
        ],
        [
            -6974873, 3210181, 1636536, -5931642, 8227423, -7750063, 4660461,
        ],
        [
            -8227423, 7750063, -6974873, 5931642, -4660461, 3210181, -1636536,
        ],
    ];
    for (out, row) in output.iter_mut().zip(&COS_MOD) {
        let mut res = i64::from(input[0]) << 23;
        for (&c, &x) in row.iter().zip(&input[1..]) {
            res += i64::from(c) * i64::from(x);
        }
        *out = norm(res, 23);
    }
}

fn mod_a(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [i32; 16] = [
        4199362, 4240198, 4323885, 4454708, 4639772, 4890013, 5221943, 5660703, -6245623, -7040975,
        -8158494, -9809974, -12450076, -17261920, -28585092, -85479984,
    ];
    for i in 0..8 {
        output[i] = mul23(COS_MOD[i], input[i].wrapping_add(input[8 + i]));
    }
    for (i, k) in (8..16).zip((0..8).rev()) {
        output[i] = mul23(COS_MOD[i], input[k].wrapping_sub(input[8 + k]));
    }
}

fn mod_b(input: &mut [i32], output: &mut [i32]) {
    const COS_MOD: [i32; 8] = [
        4214598, 4383036, 4755871, 5425934, 6611520, 8897610, 14448934, 42791536,
    ];
    for i in 0..8 {
        input[8 + i] = mul23(COS_MOD[i], input[8 + i]);
    }
    for i in 0..8 {
        output[i] = input[i].wrapping_add(input[8 + i]);
    }
    for (i, k) in (8..16).zip((0..8).rev()) {
        output[i] = input[k].wrapping_sub(input[8 + k]);
    }
}

fn mod_c(input: &[i32], output: &mut [i32]) {
    const COS_MOD: [i32; 32] = [
        1048892, 1051425, 1056522, 1064244, 1074689, 1087987, 1104313, 1123884, 1146975, 1173922,
        1205139, 1241133, 1282529, 1330095, 1384791, 1447815, -1520688, -1605358, -1704360,
        -1821051, -1959964, -2127368, -2332183, -2587535, -2913561, -3342802, -3931480, -4785806,
        -6133390, -8566050, -14253820, -42727120,
    ];
    for i in 0..16 {
        output[i] = mul23(COS_MOD[i], input[i].wrapping_add(input[16 + i]));
    }
    for (i, k) in (16..32).zip((0..16).rev()) {
        output[i] = mul23(COS_MOD[i], input[k].wrapping_sub(input[16 + k]));
    }
}

fn clip_all(values: &mut [i32]) {
    for v in values {
        *v = clip23(*v);
    }
}

/// `imdct_half_32`: the input scaled down by four when its magnitudes sum
/// past 2^22, transformed, and scaled back.
fn imdct_half_32(output: &mut [i32; 32], input: &[i32; 32]) {
    let mut a = [0i32; 32];
    let mut b = [0i32; 32];
    let magnitude = input
        .iter()
        .fold(0i32, |m, &x| m.wrapping_add(x.wrapping_abs()));
    let shift = if magnitude > 0x40_0000 { 2 } else { 0 };
    let round = if shift > 0 { 1 << (shift - 1) } else { 0 };
    for (a, &x) in a.iter_mut().zip(input) {
        *a = x.wrapping_add(round) >> shift;
    }

    sum_a(&a, &mut b[..16], 16);
    sum_b(&a, &mut b[16..], 16);
    clip_all(&mut b);

    sum_a(&b[..16], &mut a[..8], 8);
    sum_b(&b[..16], &mut a[8..16], 8);
    sum_c(&b[16..], &mut a[16..24], 8);
    sum_d(&b[16..], &mut a[24..], 8);
    clip_all(&mut a);

    dct_a(&a[..8], &mut b[..8]);
    dct_b(&a[8..16], &mut b[8..16]);
    dct_b(&a[16..24], &mut b[16..24]);
    dct_b(&a[24..], &mut b[24..]);
    clip_all(&mut b);

    mod_a(&b[..16], &mut a[..16]);
    mod_b(&mut b[16..], &mut a[16..]);
    clip_all(&mut a);

    mod_c(&a, &mut b);

    for v in &mut b {
        *v = clip23(v.wrapping_mul(1 << shift));
    }
    for (i, k) in (0..16).zip((16..32).rev()) {
        output[i] = clip23(b[i].wrapping_sub(b[k]));
        output[16 + i] = clip23(b[i].wrapping_add(b[k]));
    }
}

#[cfg(test)]
mod tests;
