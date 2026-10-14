// SPDX-License-Identifier: GPL-3.0-or-later
//
// The window's construction carries material ported from FFmpeg, which is
// licensed under the GNU Lesser General Public License, version 2.1 or later,
// and is used here under the GPL-3.0-or-later of this project as that licence
// permits:
//   libavcodec/kbdwin.c — Copyright (c) 2009 Justin Ruggles
// What was taken and what was changed is recorded in docs/provenance.md.

//! The forward transform of A/52 §8.2.3, and the window it takes.
//!
//! ```text
//! X[k] = −2/N · Σ x[n]·cos(2π/4N·(2n+1)(2k+1) + π/4·(2k+1)),   N = 512, k < 256
//! ```
//!
//! — a standard MDCT, which is a DCT-IV of the 512 inputs folded to 256, which
//! in turn is a 128-point complex FFT between two rotations. Written from that
//! chain of identities rather than taken from anywhere, and held to the
//! definition above by a test.

use std::f64::consts::PI;

/// Samples a block advances by, and coefficients it yields.
pub const BLOCK: usize = 256;
/// Samples a transform reads: this block and the one before it.
pub const WINDOW: usize = 512;

const QUARTER: usize = BLOCK / 2;

/// The Kaiser-Bessel-derived window of A/52, α = 5: the rising half, 256
/// values; the falling half is the same read backwards.
pub fn kbd_window() -> [f32; BLOCK] {
    const ALPHA: f64 = 5.0;
    let n = BLOCK;
    let alpha2 = 4.0 * (ALPHA * PI / n as f64).powi(2);
    let mut kaiser = [0f64; BLOCK / 2 + 1];
    let mut scale = 0.0;
    for (i, value) in kaiser.iter_mut().enumerate() {
        *value = bessel_i0(((i * (n - i)) as f64 * alpha2).sqrt());
        scale += *value * if i != 0 && i < n / 2 { 2.0 } else { 1.0 };
    }
    let scale = 1.0 / (scale + 1.0);

    let mut window = [0f32; BLOCK];
    let mut sum = 0.0;
    for (i, value) in window.iter_mut().enumerate() {
        sum += kaiser[if i <= n / 2 { i } else { n - i }];
        *value = (sum * scale).sqrt() as f32;
    }
    window
}

/// The modified Bessel function of the first kind, order nought, by its power
/// series — which at the arguments a window of this α needs (up to 5π)
/// converges in a few dozen terms.
fn bessel_i0(x: f64) -> f64 {
    let quarter = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..100 {
        term *= quarter / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// The transform, its rotations and its FFT's tables precomputed, and its
/// scratch held, so a block allocates nothing.
pub struct Mdct {
    /// `e^{−iπ(4n+1)/4M}`, the rotation into the FFT.
    pre: [(f32, f32); QUARTER],
    /// `e^{−iπk/M}`, the rotation out of it.
    post: [(f32, f32); QUARTER],
    /// `e^{−2πik/128}` for the FFT's butterflies.
    twiddles: [(f32, f32); QUARTER / 2],
    reverse: [u8; QUARTER],
    re: [f32; QUARTER],
    im: [f32; QUARTER],
}

impl Default for Mdct {
    fn default() -> Self {
        Self::new()
    }
}

impl Mdct {
    pub fn new() -> Self {
        let m = BLOCK as f64;
        let mut pre = [(0f32, 0f32); QUARTER];
        let mut post = [(0f32, 0f32); QUARTER];
        for n in 0..QUARTER {
            let a = -PI * (4 * n + 1) as f64 / (4.0 * m);
            pre[n] = (a.cos() as f32, a.sin() as f32);
            let b = -PI * n as f64 / m;
            post[n] = (b.cos() as f32, b.sin() as f32);
        }
        let mut twiddles = [(0f32, 0f32); QUARTER / 2];
        for (k, twiddle) in twiddles.iter_mut().enumerate() {
            let a = -2.0 * PI * k as f64 / QUARTER as f64;
            *twiddle = (a.cos() as f32, a.sin() as f32);
        }
        let bits = QUARTER.trailing_zeros();
        let mut reverse = [0u8; QUARTER];
        for (i, r) in reverse.iter_mut().enumerate() {
            *r = ((i as u32).reverse_bits() >> (32 - bits)) as u8;
        }
        Self {
            pre,
            post,
            twiddles,
            reverse,
            re: [0.0; QUARTER],
            im: [0.0; QUARTER],
        }
    }

    /// The 256 coefficients of 512 windowed samples, scaled as A/52 scales
    /// them.
    pub fn forward(&mut self, x: &[f32; WINDOW], out: &mut [f32; BLOCK]) {
        const M: usize = BLOCK;
        const H: usize = M / 2;
        // Fold the four quarters (a, b, c, d) into the DCT-IV's input
        // (−c_r − d, a − b_r), and pair it up into the FFT's complex input,
        // rotating as it goes: u[2n] + i·u[M−1−2n].
        let u = |i: usize| -> f32 {
            if i < H {
                -x[3 * H - 1 - i] - x[3 * H + i]
            } else {
                let j = i - H;
                x[j] - x[M - 1 - j]
            }
        };
        for n in 0..QUARTER {
            let (r, i) = (u(2 * n), u(M - 1 - 2 * n));
            let (c, s) = self.pre[n];
            let slot = usize::from(self.reverse[n]);
            self.re[slot] = r * c - i * s;
            self.im[slot] = r * s + i * c;
        }

        self.fft();

        let scale = -2.0 / WINDOW as f32;
        for k in 0..QUARTER {
            let (c, s) = self.post[k];
            let (r, i) = (self.re[k], self.im[k]);
            out[2 * k] = (r * c - i * s) * scale;
            out[M - 1 - 2 * k] = -(r * s + i * c) * scale;
        }
    }

    /// An in-place radix-2 FFT over `re`/`im`, already in bit-reversed order.
    fn fft(&mut self) {
        let mut half = 1;
        while half < QUARTER {
            let step = QUARTER / (2 * half);
            for start in (0..QUARTER).step_by(2 * half) {
                for j in 0..half {
                    let (c, s) = self.twiddles[j * step];
                    let a = start + j;
                    let b = a + half;
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

    /// The fast transform is the definition A/52 states.
    #[test]
    fn the_transform_is_the_definition() {
        let mut x = [0f32; WINDOW];
        let mut seed = 12345u32;
        for v in x.iter_mut() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *v = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        }
        let mut out = [0f32; BLOCK];
        Mdct::new().forward(&x, &mut out);

        let n = WINDOW as f64;
        for (k, value) in out.iter().enumerate() {
            let reference: f64 = x
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    f64::from(*v)
                        * (2.0 * PI / (4.0 * n) * ((2 * i + 1) * (2 * k + 1)) as f64
                            + PI / 4.0 * (2 * k + 1) as f64)
                            .cos()
                })
                .sum::<f64>()
                * -2.0
                / n;
            assert!(
                (f64::from(*value) - reference).abs() < 1e-5,
                "bin {k}: {value} against {reference}"
            );
        }
    }

    /// Princen-Bradley: a window and its mirror overlap to unit power, which
    /// is what makes the transform invertible by overlap-add.
    #[test]
    fn the_window_overlaps_to_unit_power() {
        let w = kbd_window();
        for n in 0..BLOCK {
            let sum = w[n] * w[n] + w[BLOCK - 1 - n] * w[BLOCK - 1 - n];
            assert!((sum - 1.0).abs() < 1e-6, "{n}: {sum}");
        }
        assert!(w[0] < 1e-3 && w[BLOCK - 1] > 0.999);
    }
}
