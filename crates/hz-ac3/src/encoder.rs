// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/ac3enc.c, ac3enc_template.c, ac3enc_float.c —
//     Copyright (c) 2000 Fabrice Bellard, 2006-2010 Justin Ruggles
//   libavcodec/ac3dsp.c — Copyright (c) 2011 Justin Ruggles
// What was taken and what was changed is recorded in docs/provenance.md.

//! One syncframe at a time: transform, exponents, allocation, mantissas,
//! bitstream.
//!
//! The pipeline is FFmpeg's encoder's, ported: the decisions it makes — when
//! to send new exponents and how coarsely, how far to raise the SNR offset —
//! are the ones a stream a decoder has accepted for twenty years was made
//! with, and the first thing this crate has to do is make the same stream.
//! What is not here yet is channel coupling, which FFmpeg turns on below
//! 576 kb/s for five channels; without it a stream is valid at every rate and
//! simply spends more bits on the high band.

use crate::bitalloc::{self, BANDS, COEFS, Codes, Parameters};
use crate::mdct::{BLOCK, Mdct, WINDOW, kbd_window};
use crate::tables::{BANDWIDTH, BIT_RATES, FRAME_WORDS, REMATRIX_BANDS, SAMPLE_RATES};
use hz_core::bits::BitWriter;

/// Blocks in a syncframe.
pub const BLOCKS: usize = 6;
/// Samples per channel a syncframe carries.
pub const FRAME_SAMPLES: usize = BLOCKS * BLOCK;
/// Samples a decoder's output lags the input by: the first block is
/// transformed against a block of silence before it.
pub const DELAY: usize = BLOCK;

const MAX_CHANNELS: usize = 6;

/// The speaker arrangement of the full-bandwidth channels (`acmod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    /// 1/0: centre.
    Mono = 1,
    /// 2/0: left, right.
    Stereo = 2,
    /// 3/0: left, centre, right.
    ThreeFront = 3,
    /// 2/1: left, right, surround.
    TwoOne = 4,
    /// 3/1: left, centre, right, surround.
    ThreeOne = 5,
    /// 2/2: left, right, left surround, right surround.
    TwoTwo = 6,
    /// 3/2: left, centre, right, left surround, right surround.
    ThreeTwo = 7,
}

impl ChannelMode {
    pub fn full_bandwidth_channels(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::ThreeFront | Self::TwoOne => 3,
            Self::ThreeOne | Self::TwoTwo => 4,
            Self::ThreeTwo => 5,
        }
    }

    fn has_centre(self) -> bool {
        (self as u8) & 1 == 1 && self != Self::Mono
    }

    fn has_surround(self) -> bool {
        (self as u8) & 4 == 4
    }

    /// For each channel in coded order, its index in the input, which is in
    /// WAV order: front left, front right, centre, LFE, then the surrounds.
    fn input_map(self, lfe: bool) -> &'static [usize] {
        match (self, lfe) {
            (Self::Mono, false) => &[0],
            (Self::Mono, true) => &[0, 1],
            (Self::Stereo, false) => &[0, 1],
            (Self::Stereo, true) => &[0, 1, 2],
            (Self::ThreeFront, false) => &[0, 2, 1],
            (Self::ThreeFront, true) => &[0, 2, 1, 3],
            (Self::TwoOne, false) => &[0, 1, 2],
            (Self::TwoOne, true) => &[0, 1, 3, 2],
            (Self::ThreeOne, false) => &[0, 2, 1, 3],
            (Self::ThreeOne, true) => &[0, 2, 1, 4, 3],
            (Self::TwoTwo, false) => &[0, 1, 2, 3],
            (Self::TwoTwo, true) => &[0, 1, 3, 4, 2],
            (Self::ThreeTwo, false) => &[0, 2, 1, 3, 4],
            (Self::ThreeTwo, true) => &[0, 2, 1, 4, 5, 3],
        }
    }
}

/// What the syncframe header says about the programme besides its channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    /// Dialogue level, −31 to −1 dBFS.
    pub dialnorm: i8,
    /// Bitstream mode: nought is complete main.
    pub bsmod: u8,
    /// Centre downmix level code: 0, 1, 2 for −3, −4.5, −6 dB.
    pub centre_mix: u8,
    /// Surround downmix level code: 0, 1, 2 for −3, −6 dB and off.
    pub surround_mix: u8,
    /// Dolby Surround mode code, stereo only.
    pub surround_mode: u8,
    pub copyright: bool,
    pub original: bool,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            dialnorm: -31,
            bsmod: 0,
            centre_mix: 1,
            surround_mix: 1,
            surround_mode: 0,
            copyright: false,
            original: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub sample_rate: u32,
    pub mode: ChannelMode,
    pub lfe: bool,
    /// kb/s, one of [`BIT_RATES`]. `None` takes FFmpeg's default for the
    /// channel count.
    pub bit_rate: Option<u32>,
    pub metadata: Metadata,
}

/// A configuration the format or this encoder cannot do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

/// Exponent strategies, as the stream codes them.
const EXP_REUSE: u8 = 0;
const EXP_NEW: u8 = 1;
const EXP_D15: u8 = 1;
const EXP_D25: u8 = 2;
const EXP_D45: u8 = 3;

/// The exponents of a block are sent again when they differ from the block
/// before by more than this, summed over the bins.
const EXP_DIFF_THRESHOLD: u32 = 500;

/// The strategy a run of blocks sharing exponents gets, by the run's length
/// less one: the longer the exponents stand, the more they are worth.
const EXP_STRATEGY_BY_RUN: [u8; 6] = [EXP_D45, EXP_D25, EXP_D25, EXP_D15, EXP_D15, EXP_D15];

/// Exponent groups a strategy takes to cover `coefs` bins, the DC exponent
/// apart.
fn exponent_groups(strategy: u8, coefs: usize) -> usize {
    if coefs == 7 {
        // The LFE channel's seven bins, which only D15 codes.
        return 2;
    }
    let size = 3usize << (strategy - 1);
    (coefs + size - 4) / size
}

/// A block-and-channel indexed buffer of `N` values each.
struct Planes<T, const N: usize> {
    data: Vec<[T; N]>,
}

impl<T: Copy + Default, const N: usize> Planes<T, N> {
    fn new(channels: usize) -> Self {
        Self {
            data: vec![[T::default(); N]; channels * BLOCKS],
        }
    }

    fn at(&self, ch: usize, blk: usize) -> &[T; N] {
        &self.data[ch * BLOCKS + blk]
    }

    fn at_mut(&mut self, ch: usize, blk: usize) -> &mut [T; N] {
        &mut self.data[ch * BLOCKS + blk]
    }
}

pub struct Encoder {
    mode: ChannelMode,
    lfe: bool,
    channels: usize,
    fbw: usize,
    map: &'static [usize],
    sr_code: usize,
    sample_rate: u32,
    bit_rate: u32,
    metadata: Metadata,

    frame_size_code: usize,
    /// Bytes.
    frame_size_min: usize,
    frame_size: usize,
    bits_written: i64,
    samples_written: i64,
    crc_inv: [u32; 2],

    bandwidth_code: u8,
    end_freq: [usize; MAX_CHANNELS],
    codes: Codes,
    params: Parameters,
    fast_gain_code: u8,
    coarse_snr: i32,
    fine_snr: i32,
    frame_bits_fixed: usize,

    window: [f32; BLOCK],
    mdct: Mdct,
    previous: Vec<[f32; BLOCK]>,
    windowed: [f32; WINDOW],

    coef: Planes<f32, COEFS>,
    fixed: Planes<i32, COEFS>,
    exp: Planes<u8, COEFS>,
    strategy: [[u8; BLOCKS]; MAX_CHANNELS],
    exp_ref: [[usize; BLOCKS]; MAX_CHANNELS],
    grouped: Planes<u8, 128>,
    psd: Planes<i16, COEFS>,
    band_psd: Planes<i16, BANDS>,
    mask: Planes<i16, BANDS>,
    /// Two sets of pointers: the best allocation found so far, and the one
    /// being tried.
    bap: [Planes<u8, COEFS>; 2],
    best: usize,
    qmant: Planes<i16, COEFS>,
    rematrix: [[bool; 4]; BLOCKS],
    new_rematrix: [bool; BLOCKS],
    /// Bits the allocation said the frame would take, the two closing fields
    /// included, for the writer to be held to.
    planned_bits: usize,
}

impl Encoder {
    pub fn new(config: Config) -> Result<Self, Unsupported> {
        let sr_code = SAMPLE_RATES
            .iter()
            .position(|rate| *rate == config.sample_rate)
            .ok_or_else(|| {
                Unsupported(format!(
                    "AC-3 runs at 48, 44.1 or 32 kHz, not {} Hz",
                    config.sample_rate
                ))
            })?;
        let fbw = config.mode.full_bandwidth_channels();
        let channels = fbw + usize::from(config.lfe);
        let bit_rate = config.bit_rate.unwrap_or(match fbw {
            1 => 96,
            2 => 192,
            3 => 320,
            4 => 384,
            _ => 448,
        });
        let rate_index = BIT_RATES
            .iter()
            .position(|rate| u32::from(*rate) == bit_rate)
            .ok_or_else(|| Unsupported(format!("{bit_rate} kb/s is not an AC-3 bit rate")))?;
        let m = &config.metadata;
        if !(-31..=-1).contains(&m.dialnorm) {
            return Err(Unsupported(format!(
                "a dialogue level of {} dB is outside −31 to −1",
                m.dialnorm
            )));
        }

        let frame_size_code = rate_index * 2;
        let frame_size_min = 2 * usize::from(FRAME_WORDS[frame_size_code][sr_code]);
        let crc_size = |size: usize| ((size >> 2) + (size >> 4)) << 1;
        let mut crc_inv = [0u32; 2];
        crc_inv[0] = pow_poly(CRC16_POLY >> 1, 8 * crc_size(frame_size_min) as u32 - 16);
        if sr_code == 1 {
            crc_inv[1] = pow_poly(
                CRC16_POLY >> 1,
                8 * crc_size(frame_size_min + 2) as u32 - 16,
            );
        }

        let bandwidth_code = BANDWIDTH[fbw - 1][sr_code][rate_index];
        let mut end_freq = [0usize; MAX_CHANNELS];
        for end in end_freq.iter_mut().take(fbw) {
            *end = usize::from(bandwidth_code) * 3 + 73;
        }
        if config.lfe {
            end_freq[fbw] = 7;
        }

        let codes = Codes::DEFAULT;
        let mut encoder = Self {
            mode: config.mode,
            lfe: config.lfe,
            channels,
            fbw,
            map: config.mode.input_map(config.lfe),
            sr_code,
            sample_rate: config.sample_rate,
            bit_rate,
            metadata: config.metadata,
            frame_size_code,
            frame_size_min,
            frame_size: frame_size_min,
            bits_written: 0,
            samples_written: 0,
            crc_inv,
            bandwidth_code,
            end_freq,
            codes,
            params: codes.parameters(sr_code),
            fast_gain_code: 4,
            coarse_snr: 40,
            fine_snr: 0,
            frame_bits_fixed: 0,
            window: kbd_window(),
            mdct: Mdct::new(),
            previous: vec![[0.0; BLOCK]; channels],
            windowed: [0.0; WINDOW],
            coef: Planes::new(channels),
            fixed: Planes::new(channels),
            exp: Planes::new(channels),
            strategy: [[EXP_REUSE; BLOCKS]; MAX_CHANNELS],
            exp_ref: [[0; BLOCKS]; MAX_CHANNELS],
            grouped: Planes::new(channels),
            psd: Planes::new(channels),
            band_psd: Planes::new(channels),
            mask: Planes::new(channels),
            bap: [Planes::new(channels), Planes::new(channels)],
            best: 0,
            qmant: Planes::new(channels),
            rematrix: [[false; 4]; BLOCKS],
            new_rematrix: [false; BLOCKS],
            planned_bits: 0,
        };
        encoder.frame_bits_fixed = encoder.count_frame_bits_fixed();
        Ok(encoder)
    }

    /// Input channels, in WAV order.
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn bit_rate(&self) -> u32 {
        self.bit_rate
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Encode one syncframe from [`FRAME_SAMPLES`] samples of each channel,
    /// planar, in WAV order, full scale ±1; append it to `out`.
    ///
    /// # Panics
    /// If the channel count or a channel's length is wrong.
    pub fn encode(&mut self, input: &[&[f32]], out: &mut Vec<u8>) -> Result<(), Unsupported> {
        assert_eq!(input.len(), self.channels, "one slice per channel");
        assert!(input.iter().all(|ch| ch.len() == FRAME_SAMPLES));

        if self.sr_code == 1 {
            self.adjust_frame_size();
        }
        self.transform(input);
        self.compute_rematrixing();
        self.scale_coefficients();
        self.apply_rematrixing();
        self.process_exponents();
        self.allocate()?;
        self.group_exponents();
        self.quantize();
        self.write(out);
        Ok(())
    }

    /// At 44.1 kHz a syncframe is a fraction of a word longer than the
    /// shorter of its two sizes; alternate them to keep the average rate.
    fn adjust_frame_size(&mut self) {
        let bit_rate = i64::from(self.bit_rate) * 1000;
        let sample_rate = i64::from(self.sample_rate);
        while self.bits_written >= bit_rate && self.samples_written >= sample_rate {
            self.bits_written -= bit_rate;
            self.samples_written -= sample_rate;
        }
        let longer = self.bits_written * sample_rate < self.samples_written * bit_rate;
        self.frame_size = self.frame_size_min + 2 * usize::from(longer);
        self.bits_written += self.frame_size as i64 * 8;
        self.samples_written += FRAME_SAMPLES as i64;
    }

    fn transform(&mut self, input: &[&[f32]]) {
        for ch in 0..self.channels {
            let source = input[self.map[ch]];
            for blk in 0..BLOCKS {
                let current = &source[blk * BLOCK..(blk + 1) * BLOCK];
                let previous: &[f32; BLOCK] = if blk == 0 {
                    &self.previous[ch]
                } else {
                    source[(blk - 1) * BLOCK..blk * BLOCK].try_into().unwrap()
                };
                for i in 0..BLOCK {
                    self.windowed[i] = previous[i] * self.window[i];
                    self.windowed[BLOCK + i] = current[i] * self.window[BLOCK - 1 - i];
                }
                let out = self.coef.at_mut(ch, blk);
                self.mdct.forward(&self.windowed, out);
            }
            self.previous[ch].copy_from_slice(&source[(BLOCKS - 1) * BLOCK..]);
        }
    }

    /// Whether to code the stereo pair as sum and difference, per band and
    /// block: wherever one of those has less energy than either side.
    fn compute_rematrixing(&mut self) {
        if self.mode != ChannelMode::Stereo {
            return;
        }
        let coefs = self.end_freq[0].min(self.end_freq[1]);
        for blk in 0..BLOCKS {
            self.new_rematrix[blk] = blk == 0;
            let (left, right) = (self.coef.at(0, blk), self.coef.at(1, blk));
            for band in 0..4 {
                let start = REMATRIX_BANDS[band];
                let end = coefs.min(REMATRIX_BANDS[band + 1]);
                let mut sum = [0f32; 4];
                for i in start..end {
                    let (l, r) = (left[i], right[i]);
                    sum[0] += l * l;
                    sum[1] += r * r;
                    sum[2] += (l + r) * (l + r);
                    sum[3] += (l - r) * (l - r);
                }
                let flag = sum[2].min(sum[3]) < sum[0].min(sum[1]);
                self.rematrix[blk][band] = flag;
                if blk > 0 && flag != self.rematrix[blk - 1][band] {
                    self.new_rematrix[blk] = true;
                }
            }
        }
    }

    /// To 24-bit fixed point, which everything after the transform is in.
    fn scale_coefficients(&mut self) {
        const LIMIT: f32 = 16_777_215.0;
        for ch in 0..self.channels {
            for blk in 0..BLOCKS {
                let (coef, fixed) = (
                    self.coef.at(ch, blk),
                    &mut self.fixed.data[ch * BLOCKS + blk],
                );
                for (f, c) in fixed.iter_mut().zip(coef) {
                    // Clamped where FFmpeg is not: a coefficient at full
                    // scale has no exponent to say it with.
                    *f = (c * 16_777_216.0).round_ties_even().clamp(-LIMIT, LIMIT) as i32;
                }
            }
        }
    }

    fn apply_rematrixing(&mut self) {
        if self.mode != ChannelMode::Stereo {
            return;
        }
        let coefs = self.end_freq[0].min(self.end_freq[1]);
        for blk in 0..BLOCKS {
            for band in 0..4 {
                if !self.rematrix[blk][band] {
                    continue;
                }
                let start = REMATRIX_BANDS[band];
                let end = coefs.min(REMATRIX_BANDS[band + 1]);
                for i in start..end {
                    let l = self.fixed.at(0, blk)[i];
                    let r = self.fixed.at(1, blk)[i];
                    self.fixed.at_mut(0, blk)[i] = (l + r) >> 1;
                    self.fixed.at_mut(1, blk)[i] = (l - r) >> 1;
                }
            }
        }
    }

    fn process_exponents(&mut self) {
        // Extract.
        for ch in 0..self.channels {
            for blk in 0..BLOCKS {
                let fixed = self.fixed.at(ch, blk);
                let exp = &mut self.exp.data[ch * BLOCKS + blk];
                for (e, c) in exp.iter_mut().zip(fixed) {
                    let v = c.unsigned_abs();
                    *e = if v == 0 { 24 } else { (23 - v.ilog2()) as u8 };
                }
            }
        }

        // Strategy: new exponents where they moved, then a coarser grid the
        // longer they are kept.
        for ch in 0..self.fbw {
            let strategy = &mut self.strategy[ch];
            strategy[0] = EXP_NEW;
            for blk in 1..BLOCKS {
                let diff: u32 = self
                    .exp
                    .at(ch, blk)
                    .iter()
                    .zip(self.exp.at(ch, blk - 1))
                    .map(|(a, b)| u32::from(a.abs_diff(*b)))
                    .sum();
                strategy[blk] = if diff > EXP_DIFF_THRESHOLD {
                    EXP_NEW
                } else {
                    EXP_REUSE
                };
            }
            let mut blk = 0;
            while blk < BLOCKS {
                let mut next = blk + 1;
                while next < BLOCKS && strategy[next] == EXP_REUSE {
                    next += 1;
                }
                strategy[blk] = EXP_STRATEGY_BY_RUN[next - blk - 1];
                blk = next;
            }
        }
        if self.lfe {
            let strategy = &mut self.strategy[self.fbw];
            strategy[0] = EXP_D15;
            strategy[1..].fill(EXP_REUSE);
        }

        // Encode: the exponents the decoder will see, in the first block of
        // each run.
        for ch in 0..self.channels {
            let mut blk = 0;
            while blk < BLOCKS {
                let mut next = blk + 1;
                self.exp_ref[ch][blk] = blk;
                while next < BLOCKS && self.strategy[ch][next] == EXP_REUSE {
                    self.exp_ref[ch][next] = blk;
                    next += 1;
                }
                for reuse in blk + 1..next {
                    let later = *self.exp.at(ch, reuse);
                    for (e, l) in self.exp.at_mut(ch, blk).iter_mut().zip(later) {
                        *e = (*e).min(l);
                    }
                }
                let strategy = self.strategy[ch][blk];
                let coefs = self.end_freq[ch];
                encode_exponents(self.exp.at_mut(ch, blk), coefs, strategy);
                blk = next;
            }
        }
    }

    fn allocate(&mut self) -> Result<(), Unsupported> {
        let frame_bits = self.frame_bits_fixed + self.count_frame_bits();
        let mut exponent_bits = 0;
        for blk in 0..BLOCKS {
            for ch in 0..self.channels {
                let strategy = self.strategy[ch][blk];
                if strategy != EXP_REUSE {
                    exponent_bits += 4 + 7 * exponent_groups(strategy, self.end_freq[ch]);
                }
            }
        }

        // Masking curves, once: they do not depend on the SNR offset.
        for blk in 0..BLOCKS {
            for ch in 0..self.channels {
                if self.strategy[ch][blk] == EXP_REUSE {
                    continue;
                }
                let end = self.end_freq[ch];
                let i = ch * BLOCKS + blk;
                bitalloc::psd(
                    &self.exp.data[i],
                    0,
                    end,
                    &mut self.psd.data[i],
                    &mut self.band_psd.data[i],
                );
                bitalloc::mask(
                    &self.params,
                    &self.band_psd.data[i],
                    0,
                    end,
                    crate::tables::FAST_GAIN[usize::from(self.fast_gain_code)],
                    self.lfe && ch == self.fbw,
                    &mut self.mask.data[i],
                );
            }
        }

        let bits_left = (8 * self.frame_size) as i64 - (frame_bits + exponent_bits) as i64;
        if bits_left < 0 {
            return Err(Unsupported(format!(
                "{} kb/s leaves no room for the mantissas",
                self.bit_rate
            )));
        }
        let bits_left = bits_left as usize;
        let trial = 1 - self.best;

        let fixed_bits = frame_bits + exponent_bits;

        let mut snr = self.coarse_snr << 4;
        if (snr | self.fine_snr) == 1023 {
            let bits = self.bit_alloc(trial, 1023);
            if bits <= bits_left {
                self.best = trial;
                self.planned_bits = fixed_bits + bits;
                return Ok(());
            }
        }
        let mut bits = usize::MAX;
        while snr >= 0 {
            bits = self.bit_alloc(trial, snr);
            if bits <= bits_left {
                break;
            }
            snr -= 64;
        }
        if snr < 0 {
            return Err(Unsupported(format!(
                "{} kb/s is too low for this programme",
                self.bit_rate
            )));
        }
        self.best = trial;
        let mut step = 64;
        while step > 0 {
            while snr + step <= 1023 {
                let trial_bits = self.bit_alloc(1 - self.best, snr + step);
                if trial_bits > bits_left {
                    break;
                }
                bits = trial_bits;
                snr += step;
                self.best = 1 - self.best;
            }
            step >>= 2;
        }
        self.coarse_snr = snr >> 4;
        self.fine_snr = snr & 0xf;
        self.planned_bits = fixed_bits + bits;
        Ok(())
    }

    /// Pointers at one SNR offset into set `set`, and the mantissa bits they
    /// cost.
    fn bit_alloc(&mut self, set: usize, snr: i32) -> usize {
        let offset = (snr - 240) * 4;
        for blk in 0..BLOCKS {
            for ch in 0..self.channels {
                if self.strategy[ch][blk] == EXP_REUSE {
                    continue;
                }
                let i = ch * BLOCKS + blk;
                bitalloc::bap(
                    &self.mask.data[i],
                    &self.psd.data[i],
                    0,
                    self.end_freq[ch],
                    offset,
                    self.params.floor,
                    &mut self.bap[set].data[i],
                );
            }
        }
        let mut bits = 0;
        for blk in 0..BLOCKS {
            let mut counts = bitalloc::primed_counts();
            for ch in 0..self.channels {
                let bap = self.bap[set].at(ch, self.exp_ref[ch][blk]);
                for value in &bap[..self.end_freq[ch]] {
                    counts[usize::from(*value)] += 1;
                }
            }
            bits += bitalloc::mantissa_bits(&counts);
        }
        bits
    }

    fn group_exponents(&mut self) {
        for blk in 0..BLOCKS {
            for ch in 0..self.channels {
                let strategy = self.strategy[ch][blk];
                if strategy == EXP_REUSE {
                    continue;
                }
                let size = usize::from(strategy + u8::from(strategy == EXP_D45));
                let groups = exponent_groups(strategy, self.end_freq[ch]);
                let exp = *self.exp.at(ch, blk);
                let grouped = self.grouped.at_mut(ch, blk);
                grouped[0] = exp[0];
                let mut at = 0usize;
                for group in grouped.iter_mut().skip(1).take(groups) {
                    let mut code = 0u8;
                    for _ in 0..3 {
                        let delta = i16::from(exp[at + size]) - i16::from(exp[at]) + 2;
                        debug_assert!((0..=4).contains(&delta));
                        code = code * 5 + delta as u8;
                        at += size;
                    }
                    *group = code;
                }
            }
        }
    }

    fn quantize(&mut self) {
        for blk in 0..BLOCKS {
            let mut groups = Groups::default();
            for ch in 0..self.channels {
                let reference = self.exp_ref[ch][blk];
                let exp = *self.exp.at(ch, reference);
                let bap = *self.bap[self.best].at(ch, reference);
                let fixed = *self.fixed.at(ch, blk);
                let i = ch * BLOCKS + blk;
                for bin in 0..self.end_freq[ch] {
                    let (c, e) = (fixed[bin], i32::from(exp[bin]));
                    let v = match bap[bin] {
                        0 => 0,
                        1 => groups
                            .one
                            .add(&mut self.qmant, i, bin, sym_quant(c, e, 3), 3),
                        2 => groups
                            .two
                            .add(&mut self.qmant, i, bin, sym_quant(c, e, 5), 5),
                        3 => sym_quant(c, e, 7),
                        4 => groups
                            .four
                            .add(&mut self.qmant, i, bin, sym_quant(c, e, 11), 11),
                        5 => sym_quant(c, e, 15),
                        14 => asym_quant(c, e, 14),
                        15 => asym_quant(c, e, 16),
                        b => asym_quant(c, e, i32::from(b) - 1),
                    };
                    self.qmant.data[i][bin] = v as i16;
                }
            }
        }
    }

    fn count_frame_bits_fixed(&self) -> usize {
        const MODE_BITS: [usize; 8] = [0, 0, 2, 2, 2, 4, 2, 4];
        let fbw = self.fbw;
        let mut bits = 16 + 49 + MODE_BITS[self.mode as usize];
        for blk in 0..BLOCKS {
            bits += fbw + fbw; // block switch and dither flags
            bits += 1; // dynamic range
            bits += 1; // coupling strategy exists
            bits += 2 * fbw + usize::from(self.lfe); // exponent strategies
            bits += 1; // bit allocation parameters exist
            if blk == 0 {
                bits += 2 + 2 + 2 + 2 + 3;
            }
            bits += 1; // SNR offsets exist
            bits += 1; // delta bit allocation
            bits += 1; // skipped data
        }
        bits + 1 + 1 + 16 // auxiliary data, CRC reserved bit, CRC
    }

    fn count_frame_bits(&self) -> usize {
        let mut bits = 0;
        for blk in 0..BLOCKS {
            if blk == 0 {
                bits += 1; // coupling in use
            }
            if self.mode == ChannelMode::Stereo {
                bits += 1;
                if self.new_rematrix[blk] {
                    bits += 4;
                }
            }
            for ch in 0..self.fbw {
                if self.strategy[ch][blk] != EXP_REUSE {
                    bits += 6 + 2; // bandwidth and gain range
                }
            }
            if blk == 0 {
                bits += 6 + self.channels * (4 + 3);
            }
        }
        bits
    }

    fn write(&mut self, out: &mut Vec<u8>) {
        let mut w = BitWriter::with_capacity(self.frame_size);
        self.write_header(&mut w);
        for blk in 0..BLOCKS {
            self.write_block(&mut w, blk);
        }
        // Everything but the auxiliary data bit, the CRC's reserved bit and
        // the CRC, which sit in the frame's last eighteen bits.
        debug_assert_eq!(
            w.bit_position() + 18,
            self.planned_bits,
            "the writer wrote what the allocation counted"
        );
        debug_assert!(self.planned_bits <= 8 * self.frame_size);
        let mut frame = w.finish();
        frame.resize(self.frame_size, 0);
        self.finish_frame(&mut frame);
        out.extend_from_slice(&frame);
    }

    fn write_header(&self, w: &mut BitWriter) {
        let m = &self.metadata;
        w.put(16, 0x0b77);
        w.put(16, 0); // crc1, filled in last
        w.put(2, self.sr_code as u32);
        w.put(
            6,
            (self.frame_size_code + (self.frame_size - self.frame_size_min) / 2) as u32,
        );
        w.put(5, 8); // bsid
        w.put(3, u32::from(m.bsmod));
        w.put(3, self.mode as u32);
        if self.mode.has_centre() {
            w.put(2, u32::from(m.centre_mix));
        }
        if self.mode.has_surround() {
            w.put(2, u32::from(m.surround_mix));
        }
        if self.mode == ChannelMode::Stereo {
            w.put(2, u32::from(m.surround_mode));
        }
        w.put_bit(self.lfe);
        w.put(5, (-i32::from(m.dialnorm)) as u32);
        w.put(1, 0); // no compression gain word
        w.put(1, 0); // no language code
        w.put(1, 0); // no audio production information
        w.put_bit(m.copyright);
        w.put_bit(m.original);
        w.put(1, 0); // no time code 1
        w.put(1, 0); // no time code 2
        w.put(1, 0); // no additional bitstream information
    }

    fn write_block(&self, w: &mut BitWriter, blk: usize) {
        for _ in 0..self.fbw {
            w.put(1, 0); // no block switching
        }
        for _ in 0..self.fbw {
            w.put(1, 1); // dither
        }
        w.put(1, 0); // no dynamic range word
        w.put_bit(blk == 0); // a coupling strategy follows
        if blk == 0 {
            w.put(1, 0); // and it is: no coupling
        }
        if self.mode == ChannelMode::Stereo {
            w.put_bit(self.new_rematrix[blk]);
            if self.new_rematrix[blk] {
                for flag in self.rematrix[blk] {
                    w.put_bit(flag);
                }
            }
        }
        for ch in 0..self.fbw {
            w.put(2, u32::from(self.strategy[ch][blk]));
        }
        if self.lfe {
            w.put(1, u32::from(self.strategy[self.fbw][blk]));
        }
        for ch in 0..self.fbw {
            if self.strategy[ch][blk] != EXP_REUSE {
                w.put(6, u32::from(self.bandwidth_code));
            }
        }
        for ch in 0..self.channels {
            let strategy = self.strategy[ch][blk];
            if strategy == EXP_REUSE {
                continue;
            }
            let grouped = self.grouped.at(ch, blk);
            w.put(4, u32::from(grouped[0]));
            for group in &grouped[1..=exponent_groups(strategy, self.end_freq[ch])] {
                w.put(7, u32::from(*group));
            }
            if ch < self.fbw {
                w.put(2, 0); // gain range
            }
        }
        w.put_bit(blk == 0); // bit allocation parameters
        if blk == 0 {
            let c = &self.codes;
            w.put(2, u32::from(c.slow_decay));
            w.put(2, u32::from(c.fast_decay));
            w.put(2, u32::from(c.slow_gain));
            w.put(2, u32::from(c.db_per_bit));
            w.put(3, u32::from(c.floor));
        }
        w.put_bit(blk == 0); // SNR offsets
        if blk == 0 {
            w.put(6, self.coarse_snr as u32);
            for _ in 0..self.channels {
                w.put(4, self.fine_snr as u32);
                w.put(3, u32::from(self.fast_gain_code));
            }
        }
        w.put(1, 0); // no delta bit allocation
        w.put(1, 0); // no skipped data

        for ch in 0..self.channels {
            let bap = self.bap[self.best].at(ch, self.exp_ref[ch][blk]);
            let qmant = self.qmant.at(ch, blk);
            for bin in 0..self.end_freq[ch] {
                let q = qmant[bin];
                let bits = match bap[bin] {
                    0 => continue,
                    1 if q != GROUPED => 5,
                    2 | 4 if q != GROUPED => 7,
                    1 | 2 | 4 => continue,
                    3 => 3,
                    14 => 14,
                    15 => 16,
                    b => u32::from(b) - 1,
                };
                w.put(bits, (q as u32) & ((1u32 << bits) - 1));
            }
        }
    }

    /// The two CRCs: the first over the first five-eighths of the frame, and
    /// placed at its start — so it is computed over what follows it and then
    /// multiplied back by the polynomial's inverse power — the second over
    /// the rest.
    fn finish_frame(&self, frame: &mut [u8]) {
        let size = self.frame_size;
        let size_58 = ((size >> 2) + (size >> 4)) << 1;
        let crc1 = crc16(&frame[4..size_58]);
        let inverse = self.crc_inv[usize::from(size > self.frame_size_min)];
        let crc1 = mul_poly(inverse, u32::from(crc1)) as u16;
        frame[2..4].copy_from_slice(&crc1.to_be_bytes());

        let mut crc2 = crc16(&frame[size_58..size - 2]);
        if crc2 == 0x0b77 {
            // A CRC that reads as a sync word: flip the reserved bit ahead of
            // it, which the polynomial absorbs as 0x8005.
            frame[size - 3] ^= 1;
            crc2 ^= 0x8005;
        }
        frame[size - 2..].copy_from_slice(&crc2.to_be_bytes());
    }
}

/// What stands in the place of a mantissa whose value went into an earlier
/// one's group.
const GROUPED: i16 = 128;

/// The running state of one grouped pointer within a block: three (or two)
/// mantissas share a code, accumulated into the first one's slot.
#[derive(Default)]
struct Group {
    count: u8,
    slot: (usize, usize),
}

impl Group {
    fn add(
        &mut self,
        qmant: &mut Planes<i16, COEFS>,
        plane: usize,
        bin: usize,
        v: i32,
        levels: i32,
    ) -> i32 {
        let per = if levels == 11 { 2 } else { 3 };
        let weight = match (per, self.count) {
            (3, 0) => levels * levels,
            (3, 1) | (2, 0) => levels,
            _ => 1,
        };
        if self.count == 0 {
            self.slot = (plane, bin);
            self.count = 1;
            return v * weight;
        }
        qmant.data[self.slot.0][self.slot.1] += (v * weight) as i16;
        self.count = (self.count + 1) % per;
        i32::from(GROUPED)
    }
}

#[derive(Default)]
struct Groups {
    one: Group,
    two: Group,
    four: Group,
}

fn sym_quant(c: i32, e: i32, levels: i32) -> i32 {
    let v = (((levels * c) >> (24 - e)) + levels) >> 1;
    debug_assert!((0..levels).contains(&v));
    v
}

fn asym_quant(c: i32, e: i32, bits: i32) -> i32 {
    let c = (((c << e) >> (24 - bits)) + 1) >> 1;
    let m = 1 << (bits - 1);
    c.min(m - 1)
}

/// The exponents a decoder will reconstruct, from the minimum over each
/// group, the DC exponent capped, and every step held to ±2.
fn encode_exponents(exp: &mut [u8; COEFS], coefs: usize, strategy: u8) {
    let groups = exponent_groups(strategy, coefs) * 3;
    match strategy {
        EXP_D25 => {
            for i in 1..=groups {
                let k = 2 * i - 1;
                exp[i] = exp[k].min(exp[k + 1]);
            }
        }
        EXP_D45 => {
            for i in 1..=groups {
                let k = 4 * i - 3;
                exp[i] = exp[k].min(exp[k + 1]).min(exp[k + 2]).min(exp[k + 3]);
            }
        }
        _ => {}
    }
    exp[0] = exp[0].min(15);
    for i in 1..=groups {
        exp[i] = exp[i].min(exp[i - 1] + 2);
    }
    for i in (0..groups).rev() {
        exp[i] = exp[i].min(exp[i + 1] + 2);
    }
    match strategy {
        EXP_D25 => {
            for i in (1..=groups).rev() {
                let v = exp[i];
                exp[2 * i] = v;
                exp[2 * i - 1] = v;
            }
        }
        EXP_D45 => {
            for i in (1..=groups).rev() {
                let v = exp[i];
                exp[4 * i - 3..=4 * i].fill(v);
            }
        }
        _ => {}
    }
}

/// x¹⁶ + x¹⁵ + x² + 1.
const CRC16_POLY: u32 = (1 << 0) | (1 << 2) | (1 << 15) | (1 << 16);

/// The CRC A/52 §7.10.1 states: that polynomial, most significant bit first,
/// from nought.
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn mul_poly(mut a: u32, mut b: u32) -> u32 {
    let mut c = 0;
    while a != 0 {
        if a & 1 != 0 {
            c ^= b;
        }
        a >>= 1;
        b <<= 1;
        if b & (1 << 16) != 0 {
            b ^= CRC16_POLY;
        }
    }
    c
}

fn pow_poly(mut a: u32, mut n: u32) -> u32 {
    let mut r = 1;
    while n != 0 {
        if n & 1 != 0 {
            r = mul_poly(r, a);
        }
        a = mul_poly(a, a);
        n >>= 1;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f32, amplitude: f32, start: usize, rate: f32) -> Vec<f32> {
        (0..FRAME_SAMPLES)
            .map(|n| {
                amplitude
                    * (2.0 * std::f32::consts::PI * frequency * (start + n) as f32 / rate).sin()
            })
            .collect()
    }

    fn encode(config: Config, frames: usize) -> Vec<u8> {
        let mut encoder = Encoder::new(config).unwrap();
        let mut out = Vec::new();
        for f in 0..frames {
            let channels: Vec<Vec<f32>> = (0..encoder.channels())
                .map(|ch| {
                    sine(
                        220.0 * (ch + 1) as f32,
                        0.3,
                        f * FRAME_SAMPLES,
                        config.sample_rate as f32,
                    )
                })
                .collect();
            let refs: Vec<&[f32]> = channels.iter().map(Vec::as_slice).collect();
            encoder.encode(&refs, &mut out).unwrap();
        }
        out
    }

    /// Both CRCs check the way a decoder checks them: the first five-eighths
    /// past the sync word sum to nought, and so does the whole frame.
    #[test]
    fn every_frame_carries_both_crcs() {
        for (rate, mode, lfe, kbps) in [
            (48_000, ChannelMode::ThreeTwo, true, 640),
            (48_000, ChannelMode::Stereo, false, 192),
            (44_100, ChannelMode::ThreeTwo, true, 448),
            (32_000, ChannelMode::Mono, false, 96),
        ] {
            let config = Config {
                sample_rate: rate,
                mode,
                lfe,
                bit_rate: Some(kbps),
                metadata: Metadata::default(),
            };
            let stream = encode(config, 8);
            let mut at = 0;
            let mut frames = 0;
            while at < stream.len() {
                assert_eq!(&stream[at..at + 2], &[0x0b, 0x77]);
                let fscod = usize::from(stream[at + 4] >> 6);
                let code = usize::from(stream[at + 4] & 0x3f);
                let size = 2 * usize::from(FRAME_WORDS[code][fscod]);
                let frame = &stream[at..at + size];
                let size_58 = ((size >> 2) + (size >> 4)) << 1;
                assert_eq!(crc16(&frame[2..size_58]), 0, "crc1, {rate} Hz {kbps} kb/s");
                assert_eq!(crc16(&frame[2..]), 0, "crc2, {rate} Hz {kbps} kb/s");
                at += size;
                frames += 1;
            }
            assert_eq!(frames, 8);
        }
    }

    /// At 44.1 kHz the two sizes alternate so that the average is the rate.
    #[test]
    fn the_average_rate_holds_at_44_1() {
        let config = Config {
            sample_rate: 44_100,
            mode: ChannelMode::Stereo,
            lfe: false,
            bit_rate: Some(192),
            metadata: Metadata::default(),
        };
        let frames = 200;
        let stream = encode(config, frames);
        let seconds = (frames * FRAME_SAMPLES) as f64 / 44_100.0;
        let rate = stream.len() as f64 * 8.0 / seconds;
        assert!((rate - 192_000.0).abs() < 200.0, "{rate}");
    }

    #[test]
    fn grouped_exponents_decode_to_the_encoded_ones() {
        for strategy in [EXP_D15, EXP_D25, EXP_D45] {
            let mut exp = [0u8; COEFS];
            for (i, e) in exp.iter_mut().enumerate() {
                *e = ((i * 7) % 19) as u8;
            }
            encode_exponents(&mut exp, 253, strategy);
            let size = usize::from(strategy + u8::from(strategy == EXP_D45));
            for i in 1..253 {
                assert!(exp[i].abs_diff(exp[i - 1]) <= 2 || (i - 1) % size != 0 || size == 1);
            }
        }
    }
}
