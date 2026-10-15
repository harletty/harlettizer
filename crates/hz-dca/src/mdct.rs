// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavutil/tx_template.c, tx.c, tx_priv.h — Copyright (c) Lynne;
//     power-of-two FFT (c) 2008 Loren Merritt, (c) 2002 Fabrice Bellard,
//     partly based on libdjbfft by D. J. Bernstein
// What was taken and what was changed is recorded in docs/provenance.md.

//! The fixed-point MDCT the core encoder's masking model runs on.
//!
//! FFmpeg's encoder asks `av_tx` for a 32-bit integer forward MDCT of 256
//! coefficients, and what it gets is one particular algorithm: a fold and
//! pre-rotation, a 128-point split-radix FFT in place, a post-rotation, each
//! product rounded to 31 bits as it is made. The masking curve is computed
//! from those coefficients, the bit allocation from the curve, so a transform
//! that is merely accurate — rounded differently — moves the allocation of an
//! occasional band and the stream is no longer FFmpeg's. This is that
//! algorithm at that one size, nothing more general.
//!
//! The twiddles are FFmpeg's too, down to its `RESCALE`, which goes through a
//! `float` on the way to an integer and so keeps 24 bits of each cosine.

/// Coefficients out; twice as many samples in.
pub const LEN: usize = 256;
const HALF: usize = LEN / 2;

#[derive(Debug, Clone, Copy, Default)]
struct Complex {
    re: i32,
    im: i32,
}

/// `RESCALE`: `llrintf(x · 2^31)`, clipped. `llrintf` takes a `float`.
fn rescale(x: f64) -> i32 {
    let single = (x * 2_147_483_648.0) as f32;
    (single.round_ties_even() as i64).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// `CMUL`: a complex product, each half rounded to 31 bits.
#[inline(always)]
fn cmul(are: i32, aim: i32, bre: i32, bim: i32) -> (i32, i32) {
    let re = i64::from(bre) * i64::from(are) - i64::from(bim) * i64::from(aim);
    let im = i64::from(bim) * i64::from(are) + i64::from(bre) * i64::from(aim);
    (
        ((re + 0x4000_0000) >> 31) as i32,
        ((im + 0x4000_0000) >> 31) as i32,
    )
}

/// `BUTTERFLIES`, with `t1, t2, t5, t6` the rotated third and fourth inputs.
/// The arithmetic is unsigned in FFmpeg, so it wraps here.
#[inline(always)]
fn butterflies(z: &mut [Complex], a: [usize; 4], t: [i32; 4]) {
    let [t1, t2, t5, t6] = t;
    let (r0, i0, r1, i1) = (z[a[0]].re, z[a[0]].im, z[a[1]].re, z[a[1]].im);
    let t3 = t5.wrapping_sub(t1);
    let t5 = t5.wrapping_add(t1);
    z[a[2]].re = r0.wrapping_sub(t5);
    z[a[0]].re = r0.wrapping_add(t5);
    z[a[3]].im = i1.wrapping_sub(t3);
    z[a[1]].im = i1.wrapping_add(t3);
    let t4 = t2.wrapping_sub(t6);
    let t6 = t2.wrapping_add(t6);
    z[a[3]].re = r1.wrapping_sub(t4);
    z[a[1]].re = r1.wrapping_add(t4);
    z[a[2]].im = i0.wrapping_sub(t6);
    z[a[0]].im = i0.wrapping_add(t6);
}

/// `TRANSFORM`: rotate the third input by `w*` and the fourth by `w`, then
/// the butterflies.
#[inline(always)]
fn transform(z: &mut [Complex], a: [usize; 4], wre: i32, wim: i32) {
    let (t1, t2) = cmul(z[a[2]].re, z[a[2]].im, wre, wim.wrapping_neg());
    let (t5, t6) = cmul(z[a[3]].re, z[a[3]].im, wre, wim);
    butterflies(z, a, [t1, t2, t5, t6]);
}

/// The forward MDCT of [`LEN`] coefficients, set up once.
#[derive(Debug, Clone)]
pub struct Mdct {
    /// Where the fold writes each of its outputs: the FFT's input permutation.
    map: [u16; HALF],
    /// The pre- and post-rotation.
    exp: [Complex; HALF],
    /// `cos(2πi/n)` for `i ≤ n/4`, for each split-radix stage n = 8 … 128.
    tab8: [i32; 3],
    tab16: [i32; 5],
    tab32: [i32; 9],
    tab64: [i32; 17],
    tab128: [i32; 33],
    z: [Complex; HALF],
}

impl Default for Mdct {
    fn default() -> Self {
        Self::new()
    }
}

/// `split_radix_permutation`.
fn split_radix_permutation(i: usize, len: usize, inv: bool) -> isize {
    let len = len >> 1;
    if len <= 1 {
        return (i & 1) as isize;
    }
    if i & len == 0 {
        return split_radix_permutation(i, len, inv) * 2;
    }
    let len = len >> 1;
    split_radix_permutation(i, len, inv) * 4 + 1 - 2 * isize::from((i & len == 0) ^ inv)
}

fn cos_table<const N: usize>(len: usize) -> [i32; N] {
    let freq = 2.0 * std::f64::consts::PI / len as f64;
    let mut tab = [0; N];
    for (i, t) in tab.iter_mut().take(len / 4).enumerate() {
        *t = rescale((i as f64 * freq).cos());
    }
    tab
}

impl Mdct {
    pub fn new() -> Self {
        let mut map = [0u16; HALF];
        // The scatter map of the sub-transform: `map[-perm(i) & (n-1)] = i`.
        for i in 0..HALF {
            let at = (-split_radix_permutation(i, HALF, false)) as usize & (HALF - 1);
            map[at] = i as u16;
        }
        let mut exp = [Complex::default(); HALF];
        let theta = 1.0 / 8.0;
        for (i, e) in exp.iter_mut().enumerate() {
            let alpha = std::f64::consts::FRAC_PI_2 * (i as f64 + theta) / HALF as f64;
            *e = Complex {
                re: rescale(alpha.cos()),
                im: rescale(alpha.sin()),
            };
        }
        Self {
            map,
            exp,
            tab8: cos_table(8),
            tab16: cos_table(16),
            tab32: cos_table(32),
            tab64: cos_table(64),
            tab128: cos_table(128),
            z: [Complex::default(); HALF],
        }
    }

    /// `ff_tx_mdct_fwd`: 2·[`LEN`] samples in, [`LEN`] coefficients out.
    pub fn forward(&mut self, src: &[i32; 2 * LEN], dst: &mut [i32; LEN]) {
        const LEN2: usize = HALF;
        const LEN3: usize = LEN2 * 3;
        const LEN4: usize = LEN / 4;
        // FOLD: `(int32_t)(x + (unsigned)y + 32) >> 6`.
        let fold = |x: i32, y: i32| x.wrapping_add(y).wrapping_add(32) >> 6;
        for i in 0..LEN2 {
            let k = 2 * i;
            let (re, im) = if k < LEN2 {
                (
                    fold(src[LEN2 + k].wrapping_neg(), src[LEN2 - 1 - k]),
                    fold(
                        src[LEN3 + k].wrapping_neg(),
                        src[LEN3 - 1 - k].wrapping_neg(),
                    ),
                )
            } else {
                (
                    fold(
                        src[LEN2 + k].wrapping_neg(),
                        src[5 * LEN2 - 1 - k].wrapping_neg(),
                    ),
                    fold(src[k - LEN2], src[LEN3 - 1 - k].wrapping_neg()),
                )
            };
            let (zim, zre) = cmul(re, im, self.exp[i].re, self.exp[i].im);
            self.z[usize::from(self.map[i])] = Complex { re: zre, im: zim };
        }

        self.fft128();

        for i in 0..LEN4 {
            let i0 = LEN4 + i;
            let i1 = LEN4 - i - 1;
            let src1 = self.z[i1];
            let src0 = self.z[i0];
            let (a, b) = cmul(src0.re, src0.im, self.exp[i0].im, self.exp[i0].re);
            dst[2 * i1 + 1] = a;
            dst[2 * i0] = b;
            let (a, b) = cmul(src1.re, src1.im, self.exp[i1].im, self.exp[i1].re);
            dst[2 * i0 + 1] = a;
            dst[2 * i1] = b;
        }
    }

    fn fft128(&mut self) {
        let z = &mut self.z;
        fft64(z, &self.tab64, &self.tab32, &self.tab16, self.tab8[1]);
        fft32(&mut z[64..96], &self.tab32, &self.tab16, self.tab8[1]);
        fft32(&mut z[96..128], &self.tab32, &self.tab16, self.tab8[1]);
        combine(z, &self.tab128, 16);
    }
}

fn fft64(z: &mut [Complex], tab64: &[i32], tab32: &[i32], tab16: &[i32], cos8: i32) {
    fft32(&mut z[0..32], tab32, tab16, cos8);
    fft16(&mut z[32..48], tab16, cos8);
    fft16(&mut z[48..64], tab16, cos8);
    combine(z, tab64, 8);
}

fn fft32(z: &mut [Complex], tab32: &[i32], tab16: &[i32], cos8: i32) {
    fft16(&mut z[0..16], tab16, cos8);
    fft8(&mut z[16..24], cos8);
    fft8(&mut z[24..32], cos8);
    combine(z, tab32, 4);
}

/// `ff_tx_fft_sr_combine` over `z[0 .. 8·len]`.
fn combine(z: &mut [Complex], cos: &[i32], len: usize) {
    let o1 = 2 * len;
    let o2 = 4 * len;
    let o3 = 6 * len;
    // `wim = cos + o1 - 7`, walking down as `cos` walks up.
    let mut base = 0usize;
    let mut wim = o1 as isize - 7;
    let w = |at: isize| cos[at as usize];
    let mut i = 0;
    while i < len {
        for (j, back) in [
            (0, 7),
            (2, 5),
            (4, 3),
            (6, 1),
            (1, 6),
            (3, 4),
            (5, 2),
            (7, 0),
        ] {
            let at = base + j;
            transform(z, [at, at + o1, at + o2, at + o3], cos[at], w(wim + back));
        }
        base += 8;
        wim -= 8;
        i += 4;
    }
}

fn fft4(z: &mut [Complex]) {
    let s = [z[0], z[1], z[2], z[3]];
    let t3 = s[0].re.wrapping_sub(s[1].re);
    let t1 = s[0].re.wrapping_add(s[1].re);
    let t8 = s[3].re.wrapping_sub(s[2].re);
    let t6 = s[3].re.wrapping_add(s[2].re);
    z[2].re = t1.wrapping_sub(t6);
    z[0].re = t1.wrapping_add(t6);
    let t4 = s[0].im.wrapping_sub(s[1].im);
    let t2 = s[0].im.wrapping_add(s[1].im);
    let t7 = s[2].im.wrapping_sub(s[3].im);
    let t5 = s[2].im.wrapping_add(s[3].im);
    z[3].im = t4.wrapping_sub(t8);
    z[1].im = t4.wrapping_add(t8);
    z[3].re = t3.wrapping_sub(t7);
    z[1].re = t3.wrapping_add(t7);
    z[2].im = t2.wrapping_sub(t5);
    z[0].im = t2.wrapping_add(t5);
}

fn fft8(z: &mut [Complex], cos: i32) {
    let s = [z[4], z[5], z[6], z[7]];
    fft4(z);
    // `BF(t1, dst[5].re, src[4].re, -src[5].re)` and its three siblings.
    let t1 = s[0].re.wrapping_add(s[1].re);
    z[5].re = s[0].re.wrapping_sub(s[1].re);
    let t2 = s[0].im.wrapping_add(s[1].im);
    z[5].im = s[0].im.wrapping_sub(s[1].im);
    let t5 = s[2].re.wrapping_add(s[3].re);
    z[7].re = s[2].re.wrapping_sub(s[3].re);
    let t6 = s[2].im.wrapping_add(s[3].im);
    z[7].im = s[2].im.wrapping_sub(s[3].im);
    butterflies(z, [0, 2, 4, 6], [t1, t2, t5, t6]);
    transform(z, [1, 3, 5, 7], cos, cos);
}

fn fft16(z: &mut [Complex], cos: &[i32], cos8: i32) {
    fft8(&mut z[0..8], cos8);
    fft4(&mut z[8..12]);
    fft4(&mut z[12..16]);
    let t = [z[8].re, z[8].im, z[12].re, z[12].im];
    butterflies(z, [0, 4, 8, 12], t);
    transform(z, [2, 6, 10, 14], cos[2], cos[2]);
    transform(z, [1, 5, 9, 13], cos[1], cos[3]);
    transform(z, [3, 7, 11, 15], cos[3], cos[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The MDCT's definition, in double precision, at the scale FFmpeg's
    /// fixed-point transform works at: its fold drops six bits.
    fn reference(src: &[i32; 2 * LEN]) -> [f64; LEN] {
        let n = (2 * LEN) as f64;
        let mut out = [0.0; LEN];
        for (k, o) in out.iter_mut().enumerate() {
            let mut sum = 0.0;
            for (i, &x) in src.iter().enumerate() {
                let phase = std::f64::consts::PI / LEN as f64
                    * (i as f64 + 0.5 + n / 4.0)
                    * (k as f64 + 0.5);
                sum += f64::from(x) * phase.cos();
            }
            *o = sum / 64.0;
        }
        out
    }

    /// FFmpeg's own transform — `av_tx_init(AV_TX_INT32_MDCT, 0, 256,
    /// 1.0f)` in libavutil 61.1 — run on four blocks of this generator's
    /// noise, each a bit quieter than the last, and its 1024 outputs hashed.
    /// A transform that rounds one product differently fails this.
    #[test]
    fn it_is_ffmpegs_transform() {
        let mut mdct = Mdct::new();
        let mut seed = 0x1234_5678u32;
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut first = [0; 4];
        for t in 0..4 {
            let mut src = [0i32; 2 * LEN];
            for s in src.iter_mut() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *s = (seed as i32) >> (t + 4);
            }
            let mut dst = [0; LEN];
            mdct.forward(&src, &mut dst);
            if t == 0 {
                first.copy_from_slice(&dst[..4]);
            }
            for byte in dst.iter().flat_map(|v| v.to_le_bytes()) {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
            }
        }
        assert_eq!(first, [-22_934_516, -32_935_137, -15_533_780, 33_956_424]);
        assert_eq!(hash, 0x83f4_20c1_2af6_2612);
    }

    #[test]
    fn it_is_an_mdct() {
        let mut src = [0i32; 2 * LEN];
        let mut seed = 0x1234_5678u32;
        for s in src.iter_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s = (seed as i32) >> 4;
        }
        let mut dst = [0; LEN];
        Mdct::new().forward(&src, &mut dst);
        let want = reference(&src);
        let peak = want.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let worst = dst
            .iter()
            .zip(&want)
            .map(|(&a, &b)| (f64::from(a) - b).abs())
            .fold(0.0f64, f64::max);
        // Same sign convention, same scale, and rounding at the level of a
        // fixed-point transform: a few parts per million of the peak.
        assert!(worst < peak * 1e-5, "worst {worst}, peak {peak}");
    }
}
