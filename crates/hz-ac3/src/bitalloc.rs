// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/ac3.c — Copyright (c) 2000 Fabrice Bellard
//   libavcodec/ac3dsp.c — Copyright (c) 2011 Justin Ruggles
// What was taken and what was changed is recorded in docs/provenance.md.

//! The parametric bit allocation of A/52 §7.2.
//!
//! The decoder runs this too, from the exponents and the handful of
//! parameters the stream carries, and arrives at the same pointers — which is
//! the whole point: the stream never says how many bits a mantissa has, so an
//! encoder that computed it differently would write mantissas the decoder
//! reads at the wrong width. Every step is integer arithmetic for that reason,
//! and a port rather than a reconstruction.

use crate::tables::{
    BAND_START, BAP, BAP_BITS, BIN_TO_BAND, DB_PER_BIT, FAST_DECAY, FLOOR, HEARING_THRESHOLD,
    LOG_ADD, SLOW_DECAY, SLOW_GAIN,
};

/// Bins in a block.
pub const COEFS: usize = 256;
/// Bit allocation bands.
pub const BANDS: usize = 50;

/// The parameters the stream states once and every channel shares.
#[derive(Debug, Clone, Copy)]
pub struct Parameters {
    pub sr_code: usize,
    pub slow_decay: i32,
    pub fast_decay: i32,
    pub slow_gain: i32,
    pub db_per_bit: i32,
    pub floor: i32,
    /// Coupling channel leak, in the codes the stream carries them in.
    pub cpl_fast_leak: i32,
    pub cpl_slow_leak: i32,
}

/// The codes behind [`Parameters`], as the stream carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codes {
    pub slow_decay: u8,
    pub fast_decay: u8,
    pub slow_gain: u8,
    pub db_per_bit: u8,
    pub floor: u8,
}

impl Codes {
    /// FFmpeg's encoder's defaults, which every syncframe here states.
    pub const DEFAULT: Self = Self {
        slow_decay: 2,
        fast_decay: 1,
        slow_gain: 1,
        db_per_bit: 3,
        floor: 7,
    };

    pub fn parameters(self, sr_code: usize) -> Parameters {
        Parameters {
            sr_code,
            slow_decay: SLOW_DECAY[usize::from(self.slow_decay)],
            fast_decay: FAST_DECAY[usize::from(self.fast_decay)],
            slow_gain: SLOW_GAIN[usize::from(self.slow_gain)],
            db_per_bit: DB_PER_BIT[usize::from(self.db_per_bit)],
            floor: FLOOR[usize::from(self.floor)],
            cpl_fast_leak: 0,
            cpl_slow_leak: 0,
        }
    }
}

/// Exponents to power spectral density, per bin and integrated per band.
pub fn psd(
    exp: &[u8; COEFS],
    start: usize,
    end: usize,
    psd: &mut [i16; COEFS],
    band_psd: &mut [i16; BANDS],
) {
    for bin in start..end {
        psd[bin] = 3072 - (i16::from(exp[bin]) << 7);
    }

    let mut bin = start;
    let mut band = usize::from(BIN_TO_BAND[start]);
    loop {
        let mut v = i32::from(psd[bin]);
        bin += 1;
        let band_end = usize::from(BAND_START[band + 1]).min(end);
        while bin < band_end {
            let p = i32::from(psd[bin]);
            let max = v.max(p);
            let address = (max - ((v + p + 1) >> 1)).min(255) as usize;
            v = max + i32::from(LOG_ADD[address]);
            bin += 1;
        }
        band_psd[band] = v as i16;
        band += 1;
        if end <= usize::from(BAND_START[band]) {
            break;
        }
    }
}

fn lowcomp1(a: i32, b0: i32, b1: i32, c: i32) -> i32 {
    if b0 + 256 == b1 {
        c
    } else if b0 > b1 {
        (a - 64).max(0)
    } else {
        a
    }
}

fn lowcomp(a: i32, b0: i32, b1: i32, band: usize) -> i32 {
    if band < 7 {
        lowcomp1(a, b0, b1, 384)
    } else if band < 20 {
        lowcomp1(a, b0, b1, 320)
    } else {
        (a - 128).max(0)
    }
}

/// The masking curve per band, from the integrated spectrum. No delta bit
/// allocation: nothing here writes one.
pub fn mask(
    p: &Parameters,
    band_psd: &[i16; BANDS],
    start: usize,
    end: usize,
    fast_gain: i32,
    is_lfe: bool,
    mask: &mut [i16; BANDS],
) {
    debug_assert!(end > 0);
    let bp = |band: usize| i32::from(band_psd[band]);
    let mut excite = [0i32; BANDS];
    let band_start = usize::from(BIN_TO_BAND[start]);
    let band_end = usize::from(BIN_TO_BAND[end - 1]) + 1;

    let (begin, mut fastleak, mut slowleak);
    if band_start == 0 {
        let mut lc = 0;
        lc = lowcomp1(lc, bp(0), bp(1), 384);
        excite[0] = bp(0) - fast_gain - lc;
        lc = lowcomp1(lc, bp(1), bp(2), 384);
        excite[1] = bp(1) - fast_gain - lc;
        let mut first = 7;
        fastleak = 0;
        slowleak = 0;
        for band in 2..7 {
            if !(is_lfe && band == 6) {
                lc = lowcomp1(lc, bp(band), bp(band + 1), 384);
            }
            fastleak = bp(band) - fast_gain;
            slowleak = bp(band) - p.slow_gain;
            excite[band] = fastleak - lc;
            if !(is_lfe && band == 6) && bp(band) <= bp(band + 1) {
                first = band + 1;
                break;
            }
        }

        let end1 = band_end.min(22);
        for band in first..end1 {
            if !(is_lfe && band == 6) {
                lc = lowcomp(lc, bp(band), bp(band + 1), band);
            }
            fastleak = (fastleak - p.fast_decay).max(bp(band) - fast_gain);
            slowleak = (slowleak - p.slow_decay).max(bp(band) - p.slow_gain);
            excite[band] = (fastleak - lc).max(slowleak);
        }
        begin = 22;
    } else {
        // The coupling channel.
        begin = band_start;
        fastleak = (p.cpl_fast_leak << 8) + 768;
        slowleak = (p.cpl_slow_leak << 8) + 768;
    }

    for band in begin..band_end {
        fastleak = (fastleak - p.fast_decay).max(bp(band) - fast_gain);
        slowleak = (slowleak - p.slow_decay).max(bp(band) - p.slow_gain);
        excite[band] = fastleak.max(slowleak);
    }

    for band in band_start..band_end {
        let tmp = p.db_per_bit - bp(band);
        if tmp > 0 {
            excite[band] += tmp >> 2;
        }
        mask[band] = i32::from(HEARING_THRESHOLD[band][p.sr_code]).max(excite[band]) as i16;
    }
}

/// Bit allocation pointers for one channel of one block, at an SNR offset in
/// the decoder's units: `(csnroffst·16 + fsnroffst − 240)·4`.
pub fn bap(
    mask: &[i16; BANDS],
    psd: &[i16; COEFS],
    start: usize,
    end: usize,
    snr_offset: i32,
    floor: i32,
    bap: &mut [u8; COEFS],
) {
    if snr_offset == -960 {
        bap.fill(0);
        return;
    }
    let mut bin = start;
    let mut band = usize::from(BIN_TO_BAND[start]);
    loop {
        let m = ((i32::from(mask[band]) - snr_offset - floor).max(0) & 0x1fe0) + floor;
        band += 1;
        let band_end = usize::from(BAND_START[band]).min(end);
        while bin < band_end {
            let address = ((i32::from(psd[bin]) - m) >> 5).clamp(0, 63) as usize;
            bap[bin] = BAP[address];
            bin += 1;
        }
        if end <= band_end {
            break;
        }
    }
}

/// Mantissa counts per pointer for one block, primed so that the grouped
/// pointers round up to whole groups when [`mantissa_bits`] divides them.
pub fn primed_counts() -> [u16; 16] {
    let mut counts = [0u16; 16];
    counts[1] = 2;
    counts[2] = 2;
    counts[4] = 1;
    counts
}

/// Bits the mantissas counted in `counts` take: pointers one, two and four
/// group three, three and two mantissas into five, seven and seven bits.
pub fn mantissa_bits(counts: &[u16; 16]) -> usize {
    let c = |bap: usize| usize::from(counts[bap]);
    let mut bits = c(1) / 3 * 5;
    bits += (c(2) / 3 + c(4) / 2) * 7;
    bits += c(3) * 3;
    for bap in 5..16 {
        bits += c(bap) * usize::from(BAP_BITS[bap]);
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat spectrum: every bin the same exponent, so every band's
    /// integrated density is its own bin count's log sum — one bin adds
    /// nothing, two equal bins add the table's 3 dB step.
    #[test]
    fn integration_adds_equal_bins_by_the_log_table() {
        let exp = [10u8; COEFS];
        let mut p = [0i16; COEFS];
        let mut bands = [0i16; BANDS];
        psd(&exp, 0, 253, &mut p, &mut bands);
        assert_eq!(p[0], 3072 - (10 << 7));
        assert_eq!(bands[0], p[0], "a band of one bin is that bin");
        // Band 28 covers bins 28 to 30: three equal bins.
        let two = i32::from(p[0]) + i32::from(LOG_ADD[0]);
        let three = two.max(i32::from(p[0]))
            + i32::from(LOG_ADD[(two - ((two + i32::from(p[0]) + 1) >> 1)).min(255) as usize]);
        assert_eq!(i32::from(bands[28]), three);
    }

    /// Silence takes no bits and a loud flat spectrum takes many; and the
    /// lower the SNR offset, the fewer.
    #[test]
    fn more_offset_means_more_bits() {
        let params = Codes::DEFAULT.parameters(0);
        let exp = [4u8; COEFS];
        let mut p = [0i16; COEFS];
        let mut bands = [0i16; BANDS];
        let mut m = [0i16; BANDS];
        psd(&exp, 0, 253, &mut p, &mut bands);
        mask(&params, &bands, 0, 253, 0x200, false, &mut m);
        let bits = |offset: i32| {
            let mut b = [0u8; COEFS];
            bap(&m, &p, 0, 253, (offset - 240) * 4, params.floor, &mut b);
            let mut counts = primed_counts();
            for value in &b[..253] {
                counts[usize::from(*value)] += 1;
            }
            mantissa_bits(&counts)
        };
        assert!(bits(0) < bits(400));
        assert!(bits(400) < bits(800));
        let mut b = [7u8; COEFS];
        bap(&m, &p, 0, 253, -960, params.floor, &mut b);
        assert!(b.iter().all(|v| *v == 0), "the offset that means none");
    }
}
