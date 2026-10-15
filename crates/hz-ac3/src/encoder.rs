// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/ac3enc.c, ac3enc_template.c, ac3enc_float.c —
//     Copyright (c) 2000 Fabrice Bellard, 2006-2010 Justin Ruggles
//   libavcodec/ac3dsp.c — Copyright (c) 2011 Justin Ruggles
// What was taken and what was changed is recorded in docs/provenance.md.

//! One syncframe at a time: transform, coupling, exponents, allocation,
//! mantissas, bitstream.
//!
//! The pipeline is FFmpeg's encoder's, ported: the decisions it makes — when
//! to couple and from which band, when to send new exponents and how
//! coarsely, how far to raise the SNR offset — are the ones a stream decoders
//! have accepted for twenty years was made with, and the first thing this
//! crate has to do is make the same stream. Its own decisions come after, and
//! are measured against this.
//!
//! Channels are indexed as FFmpeg indexes them, because the port is easier to
//! hold against its source that way: nought is the coupling channel, then the
//! coded channels in their coded order, the LFE last.
//!
//! Asked to ([`Encoder::reconstruct`]), the encoder also keeps what a
//! decoder will make of each frame — the mantissas dequantised as A/52 §7.3
//! has them, the coupling undone by the coordinates the stream carries, the
//! stereo pair's rematrixing undone, the inverse transform windowed and
//! overlapped — so that what is fitted to the core, as joint object coding's
//! gains are, can be fitted to the core a decoder plays. It is the decoder's
//! output less the dither a decoder may add where a bin has no bits: the
//! noise is a receiver's own and fits nothing.

use crate::bitalloc::{self, BANDS, COEFS, Codes, Parameters};
use crate::mdct::{BLOCK, Mdct, WINDOW, kbd_window};
use crate::tables::{
    BANDWIDTH, BIT_RATES, CPL_START, DEFAULT_CPL_BAND_STRUCT, FAST_GAIN, FRAME_EXP_STRATEGIES,
    FRAME_WORDS, REMATRIX_BANDS, SAMPLE_RATES,
};
use hz_core::bits::BitWriter;

/// Blocks in a syncframe.
pub const BLOCKS: usize = 6;
/// Samples per channel a syncframe carries.
pub const FRAME_SAMPLES: usize = BLOCKS * BLOCK;
/// Samples a decoder's output lags the input by: the first block is
/// transformed against a block of silence before it.
pub const DELAY: usize = BLOCK;

/// The coupling channel's index.
const CPL: usize = 0;
/// The coupling channel and up to six coded channels.
const MAX_CHANNELS: usize = 7;
const MAX_CPL_BANDS: usize = 18;

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

/// Which bitstream to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// AC-3, `bsid` 8: nineteen fixed rates, two CRCs.
    #[default]
    Ac3,
    /// One independent E-AC-3 substream, `bsid` 16, six blocks a frame: any
    /// rate the frame size can say, exponent strategies per frame where one
    /// fits, and the room for an Evolution frame in a skip field.
    Eac3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub format: Format,
    pub sample_rate: u32,
    pub mode: ChannelMode,
    pub lfe: bool,
    /// kb/s: for AC-3 one of [`BIT_RATES`], for E-AC-3 any rate a six-block
    /// frame of whole 16-bit words can carry. `None` takes FFmpeg's default
    /// for the channel count.
    pub bit_rate: Option<u32>,
    /// Couple the high band where FFmpeg's encoder would, at the band it
    /// would start from. Off never couples.
    pub coupling: bool,
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

/// A block's coupling coordinates are sent again when they moved by more
/// than this on average over the bands.
const NEW_CPL_COORD_THRESHOLD: f32 = 0.03;

/// The largest coefficient 24-bit fixed point holds.
const COEF_MAX: f32 = 16_777_215.0 / 16_777_216.0;

/// Exponent groups a strategy takes to cover `coefs` bins — the coupling
/// channel's in whole groups, everyone else's with the DC exponent apart.
fn exponent_groups(cpl: bool, strategy: u8, coefs: usize) -> usize {
    let size = 3usize << (strategy - 1);
    if cpl {
        coefs / size
    } else if coefs == 7 {
        // The LFE channel's seven bins, which only D15 codes.
        2
    } else {
        (coefs + size - 4) / size
    }
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

    fn index(ch: usize, blk: usize) -> usize {
        ch * BLOCKS + blk
    }

    fn at(&self, ch: usize, blk: usize) -> &[T; N] {
        &self.data[Self::index(ch, blk)]
    }

    fn at_mut(&mut self, ch: usize, blk: usize) -> &mut [T; N] {
        &mut self.data[Self::index(ch, blk)]
    }
}

/// How the high band is coupled, fixed for the stream.
#[derive(Debug, Clone, Copy, Default)]
struct Coupling {
    /// First bin of the coupling channel.
    start: usize,
    /// One past its last.
    end: usize,
    sub_bands: usize,
    bands: usize,
    band_sizes: [usize; MAX_CPL_BANDS],
    /// The range the coupling channel's coefficients are summed over: the
    /// coupled bins and some either side, as FFmpeg lays it out — which
    /// matters because the exponent strategy reads every bin of the channel.
    sum_start: usize,
    sum_len: usize,
}

/// One block's coupling coordinates for one channel, as the stream carries
/// them.
#[derive(Debug, Clone, Copy, Default)]
struct Coordinates {
    master: u8,
    exp: [u8; MAX_CPL_BANDS],
    mant: [u8; MAX_CPL_BANDS],
}

pub struct Encoder {
    mode: ChannelMode,
    lfe: bool,
    /// Coded channels, the LFE included.
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
    start_freq: [usize; MAX_CHANNELS],
    end_freq: [usize; MAX_CHANNELS],
    coupling: Option<Coupling>,
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
    coordinates: [[Coordinates; MAX_CHANNELS]; BLOCKS],
    /// Per block and channel: nought, keep the coordinates; one, new ones
    /// follow; two, new ones follow and the stream need not say so — E-AC-3's
    /// first coordinates.
    new_coordinates: [[u8; MAX_CHANNELS]; BLOCKS],
    /// The same three values for the coupling leak, by block.
    new_cpl_leak: [u8; BLOCKS],
    rematrix: [[bool; 4]; BLOCKS],
    rematrix_bands: usize,
    new_rematrix: [bool; BLOCKS],
    /// Bits the allocation said the frame would take, the two closing fields
    /// included, for the writer to be held to.
    planned_bits: usize,
    /// The order mantissas are quantised and written in: the coded channels,
    /// the coupling channel spliced in after the first coupled one.
    order: [usize; MAX_CHANNELS],
    order_len: usize,
    /// The frame being written, its buffer kept from one frame to the next.
    writer: BitWriter,
    eac3: bool,
    /// E-AC-3: whether this frame's exponent strategies are each one of the
    /// thirty-two the frame-level code names, and which.
    frame_exp: Option<[u8; MAX_CHANNELS]>,
    /// E-AC-3: the bytes the next frame carries in block 2's skip field —
    /// where every reference stream carries its Evolution frame — and
    /// whether it carries any.
    skip_field: Vec<u8>,
    skip_pending: bool,
    /// What a decoder makes of each frame, when asked for.
    reconstruction: Option<Box<Reconstruction>>,
}

/// A decoder's side of the stream, kept as the encoder writes it.
struct Reconstruction {
    /// Per channel and block, the coefficients the stream carries,
    /// dequantised; the coupled channels' high band filled in from the
    /// coupling channel.
    coef: Planes<f32, COEFS>,
    /// Per coded channel, the second half of the last block's inverse
    /// transform, windowed: the next block's first half overlaps it.
    overlap: Vec<[f32; BLOCK]>,
    /// Per input channel, in WAV order, the last frame decoded.
    output: Vec<Vec<f32>>,
    inverse: [f32; WINDOW],
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
        let eac3 = config.format == Format::Eac3;
        let (rate_index, frame_size_min) = if eac3 {
            // Six blocks a frame: whole words, at most 2048 of them.
            let rate = config.sample_rate as u64;
            let bps = u64::from(bit_rate) * 1000;
            let max = 2048 * rate / FRAME_SAMPLES as u64 * 16;
            let min = rate.div_ceil(FRAME_SAMPLES as u64) * 16;
            if !(min..=max).contains(&bps) {
                return Err(Unsupported(format!(
                    "E-AC-3 at {} Hz with six blocks a frame runs from {} to {} kb/s, not {bit_rate}",
                    config.sample_rate,
                    min.div_ceil(1000),
                    max / 1000
                )));
            }
            // The nearest AC-3 rate, for the tables that choose bandwidth
            // and coupling by rate; and the words a frame takes, the frame
            // size alternating above it to keep the average.
            let nearest = (0..BIT_RATES.len())
                .min_by_key(|i| (u64::from(BIT_RATES[*i]) * 1000).abs_diff(bps))
                .expect("rates");
            let mut words = bps / 16 * FRAME_SAMPLES as u64 / rate;
            while words > 1 && words * rate / FRAME_SAMPLES as u64 * 16 > bps {
                words -= 1;
            }
            (nearest, 2 * words as usize)
        } else {
            let index = BIT_RATES
                .iter()
                .position(|rate| u32::from(*rate) == bit_rate)
                .ok_or_else(|| Unsupported(format!("{bit_rate} kb/s is not an AC-3 bit rate")))?;
            (index, 2 * usize::from(FRAME_WORDS[index * 2][sr_code]))
        };
        let m = &config.metadata;
        if !(-31..=-1).contains(&m.dialnorm) {
            return Err(Unsupported(format!(
                "a dialogue level of {} dB is outside −31 to −1",
                m.dialnorm
            )));
        }

        let frame_size_code = rate_index * 2;
        let crc_size = |size: usize| ((size >> 2) + (size >> 4)) << 1;
        let mut crc_inv = [0u32; 2];
        // AC-3's first CRC, placed ahead of what it covers; E-AC-3 has none.
        if !eac3 {
            crc_inv[0] = pow_poly(CRC16_POLY >> 1, 8 * crc_size(frame_size_min) as u32 - 16);
        }
        if !eac3 && sr_code == 1 {
            crc_inv[1] = pow_poly(
                CRC16_POLY >> 1,
                8 * crc_size(frame_size_min + 2) as u32 - 16,
            );
        }

        let bandwidth_code = BANDWIDTH[fbw - 1][sr_code][rate_index];
        let full_end = usize::from(bandwidth_code) * 3 + 73;
        let mut start_freq = [0usize; MAX_CHANNELS];
        let mut end_freq = [0usize; MAX_CHANNELS];
        for end in end_freq.iter_mut().skip(1).take(fbw) {
            *end = full_end;
        }
        if config.lfe {
            end_freq[fbw + 1] = 7;
        }

        let cpl_start = (config.coupling && config.mode as usize >= ChannelMode::Stereo as usize)
            .then(|| CPL_START[config.mode as usize - 2][sr_code][rate_index])
            .filter(|start| *start >= 0);
        let coupling = cpl_start.map(|start| {
            let end_band = usize::from(bandwidth_code) / 4 + 3;
            let start_band = (start as usize).min((end_band - 1).min(15));
            let mut c = Coupling {
                start: start_band * 12 + 37,
                end: end_band * 12 + 37,
                sub_bands: end_band - start_band,
                bands: 1,
                ..Coupling::default()
            };
            c.band_sizes[0] = 12;
            for sub in start_band + 1..end_band {
                if DEFAULT_CPL_BAND_STRUCT[sub] {
                    c.band_sizes[c.bands - 1] += 12;
                } else {
                    c.band_sizes[c.bands] = 12;
                    c.bands += 1;
                }
            }
            // FFmpeg aligns the summed range for its vector code: one bin
            // early, a multiple of 32 long, and inside the block.
            c.sum_len = (c.sub_bands * 12 + 1).next_multiple_of(32);
            c.sum_start = (c.start - 1 + c.sum_len).min(COEFS) - c.sum_len;
            c
        });
        if let Some(c) = &coupling {
            start_freq[CPL] = c.start;
            end_freq[CPL] = c.end;
            for end in end_freq.iter_mut().skip(1).take(fbw) {
                *end = c.start;
            }
        }
        let rematrix_bands = match &coupling {
            Some(c) => 4 - usize::from(c.start <= 61) - usize::from(c.start == 37),
            None => 4,
        };

        let codes = Codes {
            db_per_bit: if eac3 { 2 } else { 3 },
            ..Codes::DEFAULT
        };
        let planes = channels + 1;
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
            start_freq,
            end_freq,
            coupling,
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
            coef: Planes::new(planes),
            fixed: Planes::new(planes),
            exp: Planes::new(planes),
            strategy: [[EXP_REUSE; BLOCKS]; MAX_CHANNELS],
            exp_ref: [[0; BLOCKS]; MAX_CHANNELS],
            grouped: Planes::new(planes),
            psd: Planes::new(planes),
            band_psd: Planes::new(planes),
            mask: Planes::new(planes),
            bap: [Planes::new(planes), Planes::new(planes)],
            best: 0,
            qmant: Planes::new(planes),
            coordinates: [[Coordinates::default(); MAX_CHANNELS]; BLOCKS],
            new_coordinates: [[0; MAX_CHANNELS]; BLOCKS],
            new_cpl_leak: [0; BLOCKS],
            rematrix: [[false; 4]; BLOCKS],
            rematrix_bands,
            new_rematrix: [false; BLOCKS],
            planned_bits: 0,
            order: [0; MAX_CHANNELS],
            order_len: 0,
            writer: BitWriter::with_capacity(4096),
            eac3,
            frame_exp: None,
            skip_field: Vec::with_capacity(512),
            skip_pending: false,
            reconstruction: None,
        };
        encoder.frame_bits_fixed = encoder.count_frame_bits_fixed();
        if encoder.coupling.is_some() {
            // The leak is stated once, and E-AC-3 knows the first time is new.
            encoder.new_cpl_leak[0] = if eac3 { 2 } else { 1 };
        }
        for ch in 1..=channels {
            encoder.order[encoder.order_len] = ch;
            encoder.order_len += 1;
            if ch == 1 && encoder.coupling.is_some() {
                encoder.order[encoder.order_len] = CPL;
                encoder.order_len += 1;
            }
        }
        Ok(encoder)
    }

    /// From the next frame on, keep what a decoder will make of each one:
    /// [`Self::decoded`]. The stream is the same either way.
    pub fn reconstruct(&mut self) {
        let planes = self.coef.data.len() / BLOCKS;
        self.reconstruction = Some(Box::new(Reconstruction {
            coef: Planes::new(planes),
            overlap: vec![[0.0; BLOCK]; self.channels],
            output: vec![vec![0.0; FRAME_SAMPLES]; self.channels],
            inverse: [0.0; WINDOW],
        }));
    }

    /// The last frame [`Self::encode`] wrote, as a decoder plays it: input
    /// channel `ch` in WAV order, [`FRAME_SAMPLES`] samples, [`DELAY`] behind
    /// the input that went in — less the dither a decoder may add where a bin
    /// has no bits. `None` unless [`Self::reconstruct`] was asked.
    pub fn decoded(&self, ch: usize) -> Option<&[f32]> {
        self.reconstruction
            .as_ref()
            .map(|r| r.output[ch].as_slice())
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

    /// E-AC-3: carry these bytes in block 2's skip field of the next frame
    /// encoded — an Evolution frame behind its sync word, as a reference
    /// stream carries its object metadata. At most 511 bytes.
    ///
    /// # Panics
    /// On an AC-3 encoder, which has nowhere to put them that a decoder
    /// reads, or past 511 bytes.
    pub fn carry_skip_field(&mut self, bytes: &[u8]) {
        assert!(self.eac3, "skip fields are carried in E-AC-3");
        assert!(bytes.len() < 512, "a skip field holds at most 511 bytes");
        self.skip_field.clear();
        self.skip_field.extend_from_slice(bytes);
        self.skip_pending = true;
    }

    /// Whether the stream couples its high band.
    pub fn coupled(&self) -> bool {
        self.coupling.is_some()
    }

    /// The order mantissas are quantised and written in.
    fn mantissa_order(&self) -> &[usize] {
        &self.order[..self.order_len]
    }

    /// The channels whose exponents, strategies and offsets the stream
    /// carries, the coupling channel first when there is one.
    fn coded_channels(&self) -> std::ops::RangeInclusive<usize> {
        usize::from(self.coupling.is_none())..=self.channels
    }

    fn in_coupling(&self, ch: usize) -> bool {
        self.coupling.is_some() && (1..=self.fbw).contains(&ch)
    }

    /// Encode one syncframe from [`FRAME_SAMPLES`] samples of each channel,
    /// planar, in WAV order, full scale ±1; append it to `out`.
    ///
    /// # Panics
    /// If the channel count or a channel's length is wrong.
    pub fn encode(&mut self, input: &[&[f32]], out: &mut Vec<u8>) -> Result<(), Unsupported> {
        assert_eq!(input.len(), self.channels, "one slice per channel");
        assert!(input.iter().all(|ch| ch.len() == FRAME_SAMPLES));

        if self.sr_code == 1 || self.eac3 {
            self.adjust_frame_size();
        }
        self.transform(input);
        if self.coupling.is_some() {
            self.couple();
        }
        self.compute_rematrixing();
        self.scale_coefficients();
        self.apply_rematrixing();
        self.process_exponents();
        self.allocate()?;
        self.group_exponents();
        self.quantize();
        self.rebuild();
        self.write(out);
        self.skip_pending = false;
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
        for ch in 1..=self.channels {
            let source = input[self.map[ch - 1]];
            for blk in 0..BLOCKS {
                let current = &source[blk * BLOCK..(blk + 1) * BLOCK];
                let previous: &[f32; BLOCK] = if blk == 0 {
                    &self.previous[ch - 1]
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
            self.previous[ch - 1].copy_from_slice(&source[(BLOCKS - 1) * BLOCK..]);
        }
    }

    /// The coupling channel — the sum of the coupled channels over the high
    /// band — and each channel's coordinates: the square root of its energy
    /// over the sum's, per band, an eighth of it as the stream scales it.
    fn couple(&mut self) {
        let c = self.coupling.expect("coupling");
        for blk in 0..BLOCKS {
            let mut sum = [0f32; COEFS];
            for ch in 1..=self.fbw {
                let coef = self.coef.at(ch, blk);
                for i in c.sum_start..c.sum_start + c.sum_len {
                    sum[i] += coef[i];
                }
            }
            let cpl = self.coef.at_mut(CPL, blk);
            for i in c.sum_start..c.sum_start + c.sum_len {
                cpl[i] = sum[i].clamp(-COEF_MAX, COEF_MAX);
            }
        }

        let mut energy = [[[0f32; MAX_CPL_BANDS]; MAX_CHANNELS]; BLOCKS];
        let mut bin = c.start;
        for band in 0..c.bands {
            let size = c.band_sizes[band];
            for ch in CPL..=self.fbw {
                for (blk, energy) in energy.iter_mut().enumerate() {
                    let coef = self.coef.at(ch, blk);
                    energy[ch][band] = coef[bin..bin + size].iter().map(|v| v * v).sum();
                }
            }
            bin += size;
        }

        let coordinate = |ch: f32, cpl: f32| {
            let mut coord = 0.125;
            if cpl > 0.0 {
                coord *= (ch / cpl).sqrt();
            }
            f32::min(coord, COEF_MAX)
        };
        let mut coords = [[[0f32; MAX_CPL_BANDS]; MAX_CHANNELS]; BLOCKS];
        for blk in 0..BLOCKS {
            for ch in 1..=self.fbw {
                for band in 0..c.bands {
                    coords[blk][ch][band] =
                        coordinate(energy[blk][ch][band], energy[blk][CPL][band]);
                }
            }
        }

        // New coordinates where they moved.
        for blk in 0..BLOCKS {
            self.new_coordinates[blk] = [0; MAX_CHANNELS];
            for ch in 1..=self.fbw {
                // E-AC-3 knows the first block's coordinates are new.
                if blk == 0 {
                    self.new_coordinates[blk][ch] = if self.eac3 { 2 } else { 1 };
                    continue;
                }
                self.new_coordinates[blk][ch] = u8::from({
                    let moved: f32 = (0..c.bands)
                        .map(|band| (coords[blk - 1][ch][band] - coords[blk][ch][band]).abs())
                        .sum();
                    moved / c.bands as f32 > NEW_CPL_COORD_THRESHOLD
                });
            }
        }

        // Coordinates that stand for several blocks are those blocks'
        // energies taken together. FFmpeg steps from run to run by the *last*
        // channel's runs, so another channel's run that starts elsewhere keeps
        // its first block's own coordinate; kept, so that the streams agree,
        // and a thing to measure when this encoder starts deciding for itself.
        for band in 0..c.bands {
            let mut blk = 0;
            while blk < BLOCKS {
                let mut next = blk + 1;
                for ch in 1..=self.fbw {
                    let (mut ch_energy, mut cpl_energy) =
                        (energy[blk][ch][band], energy[blk][CPL][band]);
                    next = blk + 1;
                    while next < BLOCKS && self.new_coordinates[next][ch] == 0 {
                        ch_energy += energy[next][ch][band];
                        cpl_energy += energy[next][CPL][band];
                        next += 1;
                    }
                    coords[blk][ch][band] = coordinate(ch_energy, cpl_energy);
                }
                blk = next;
            }
        }

        // To the stream's exponents and mantissas: a master exponent per
        // channel, three steps of it, and four bits of each.
        for blk in 0..BLOCKS {
            for ch in 1..=self.fbw {
                let mut fixed = [0i32; MAX_CPL_BANDS];
                let mut exp = [0i32; MAX_CPL_BANDS];
                for band in 0..c.bands {
                    fixed[band] = to_fixed24(coords[blk][ch][band]);
                    exp[band] = exponent(fixed[band]);
                }
                if self.new_coordinates[blk][ch] == 0 {
                    continue;
                }
                let bands = &exp[..c.bands];
                let (min, max) = (*bands.iter().min().unwrap(), *bands.iter().max().unwrap());
                let mut master = (((max - 15) + 2) / 3).max(0);
                while min < master * 3 {
                    master -= 1;
                }
                let out = &mut self.coordinates[blk][ch];
                out.master = master as u8;
                for band in 0..c.bands {
                    let e = (exp[band] - master * 3).clamp(0, 15);
                    out.exp[band] = e as u8;
                    let mut mant = (fixed[band] << (5 + e + master * 3)) >> 24;
                    if e == 15 {
                        mant >>= 1;
                    } else {
                        mant -= 16;
                    }
                    out.mant[band] = mant as u8;
                }
            }
        }
    }

    /// Whether to code the stereo pair as sum and difference, per band and
    /// block: wherever one of those has less energy than either side.
    fn compute_rematrixing(&mut self) {
        if self.mode != ChannelMode::Stereo {
            return;
        }
        let coefs = self.end_freq[1].min(self.end_freq[2]);
        for blk in 0..BLOCKS {
            self.new_rematrix[blk] = blk == 0;
            let (left, right) = (self.coef.at(1, blk), self.coef.at(2, blk));
            for band in 0..self.rematrix_bands {
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
        for ch in self.coded_channels() {
            for blk in 0..BLOCKS {
                let i = Planes::<f32, COEFS>::index(ch, blk);
                for (f, c) in self.fixed.data[i].iter_mut().zip(&self.coef.data[i]) {
                    *f = to_fixed24(*c);
                }
            }
        }
    }

    fn apply_rematrixing(&mut self) {
        if self.mode != ChannelMode::Stereo {
            return;
        }
        let coefs = self.end_freq[1].min(self.end_freq[2]);
        for blk in 0..BLOCKS {
            for band in 0..self.rematrix_bands {
                if !self.rematrix[blk][band] {
                    continue;
                }
                let start = REMATRIX_BANDS[band];
                let end = coefs.min(REMATRIX_BANDS[band + 1]);
                for i in start..end {
                    let l = self.fixed.at(1, blk)[i];
                    let r = self.fixed.at(2, blk)[i];
                    self.fixed.at_mut(1, blk)[i] = (l + r) >> 1;
                    self.fixed.at_mut(2, blk)[i] = (l - r) >> 1;
                }
            }
        }
    }

    fn process_exponents(&mut self) {
        // Extract.
        for ch in self.coded_channels() {
            for blk in 0..BLOCKS {
                let i = Planes::<u8, COEFS>::index(ch, blk);
                for (e, c) in self.exp.data[i].iter_mut().zip(&self.fixed.data[i]) {
                    *e = exponent(*c) as u8;
                }
            }
        }

        // Strategy: new exponents where they moved, then a coarser grid the
        // longer they are kept.
        let first = usize::from(self.coupling.is_none());
        for ch in first..=self.fbw {
            let threshold = if ch == CPL {
                EXP_DIFF_THRESHOLD * (self.end_freq[CPL] - self.start_freq[CPL]) as u32
                    / COEFS as u32
            } else {
                EXP_DIFF_THRESHOLD
            };
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
                strategy[blk] = if diff > threshold { EXP_NEW } else { EXP_REUSE };
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
            let strategy = &mut self.strategy[self.channels];
            strategy[0] = EXP_D15;
            strategy[1..].fill(EXP_REUSE);
        }
        if self.eac3 {
            // One five-bit code a channel for the frame, where every channel's
            // six strategies are one of the sets the code can name.
            let mut codes = [0u8; MAX_CHANNELS];
            let named = (first..=self.fbw).all(|ch| {
                FRAME_EXP_STRATEGIES
                    .iter()
                    .position(|set| *set == self.strategy[ch])
                    .map(|code| codes[ch] = code as u8)
                    .is_some()
            });
            self.frame_exp = named.then_some(codes);
        }

        // Encode: the exponents the decoder will see, in the first block of
        // each run.
        for ch in self.coded_channels() {
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
                let (start, end) = (self.start_freq[ch], self.end_freq[ch]);
                let exp = self.exp.at_mut(ch, blk);
                if ch == CPL {
                    encode_cpl_exponents(exp, start, end - start, strategy);
                } else {
                    encode_exponents(exp, end, strategy);
                }
                blk = next;
            }
        }
    }

    fn allocate(&mut self) -> Result<(), Unsupported> {
        let frame_bits = self.frame_bits_fixed + self.count_frame_bits();
        let mut exponent_bits = 0;
        for blk in 0..BLOCKS {
            for ch in self.coded_channels() {
                let strategy = self.strategy[ch][blk];
                if strategy != EXP_REUSE {
                    let coefs = self.end_freq[ch] - self.start_freq[ch];
                    exponent_bits += 4 + 7 * exponent_groups(ch == CPL, strategy, coefs);
                }
            }
        }

        // Masking curves, once: they do not depend on the SNR offset.
        for blk in 0..BLOCKS {
            for ch in self.coded_channels() {
                if self.strategy[ch][blk] == EXP_REUSE {
                    continue;
                }
                let (start, end) = (self.start_freq[ch], self.end_freq[ch]);
                let i = Planes::<u8, COEFS>::index(ch, blk);
                bitalloc::psd(
                    &self.exp.data[i],
                    start,
                    end,
                    &mut self.psd.data[i],
                    &mut self.band_psd.data[i],
                );
                bitalloc::mask(
                    &self.params,
                    &self.band_psd.data[i],
                    start,
                    end,
                    FAST_GAIN[usize::from(self.fast_gain_code)],
                    self.lfe && ch == self.channels,
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
        let fixed_bits = frame_bits + exponent_bits;
        let trial = 1 - self.best;

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
            for ch in self.coded_channels() {
                if self.strategy[ch][blk] == EXP_REUSE {
                    continue;
                }
                let i = Planes::<u8, COEFS>::index(ch, blk);
                bitalloc::bap(
                    &self.mask.data[i],
                    &self.psd.data[i],
                    self.start_freq[ch],
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
            for ch in self.coded_channels() {
                let bap = self.bap[set].at(ch, self.exp_ref[ch][blk]);
                for value in &bap[self.start_freq[ch]..self.end_freq[ch]] {
                    counts[usize::from(*value)] += 1;
                }
            }
            bits += bitalloc::mantissa_bits(&counts);
        }
        bits
    }

    fn group_exponents(&mut self) {
        for blk in 0..BLOCKS {
            for ch in self.coded_channels() {
                let strategy = self.strategy[ch][blk];
                if strategy == EXP_REUSE {
                    continue;
                }
                let cpl = ch == CPL;
                let size = usize::from(strategy + u8::from(strategy == EXP_D45));
                let (start, end) = (self.start_freq[ch], self.end_freq[ch]);
                let groups = exponent_groups(cpl, strategy, end - start);
                let exp = *self.exp.at(ch, blk);
                let grouped = self.grouped.at_mut(ch, blk);
                // The coupling channel's reference exponent sits in the bin
                // before its first.
                let mut at = start - usize::from(cpl);
                let mut previous = exp[at];
                grouped[0] = previous;
                at += 1;
                for group in grouped.iter_mut().skip(1).take(groups) {
                    let mut code = 0u8;
                    for _ in 0..3 {
                        let delta = i16::from(exp[at]) - i16::from(previous) + 2;
                        debug_assert!((0..=4).contains(&delta), "{delta}");
                        code = code * 5 + delta as u8;
                        previous = exp[at];
                        at += size;
                    }
                    *group = code;
                }
            }
        }
    }

    fn quantize(&mut self) {
        if let Some(r) = self.reconstruction.as_mut() {
            for plane in &mut r.coef.data {
                plane.fill(0.0);
            }
        }
        for blk in 0..BLOCKS {
            let mut groups = Groups::default();
            for &ch in &self.order[..self.order_len] {
                let reference = self.exp_ref[ch][blk];
                let exp = *self.exp.at(ch, reference);
                let bap = *self.bap[self.best].at(ch, reference);
                let fixed = *self.fixed.at(ch, blk);
                let i = Planes::<u8, COEFS>::index(ch, blk);
                for bin in self.start_freq[ch]..self.end_freq[ch] {
                    let (c, e) = (fixed[bin], i32::from(exp[bin]));
                    let level = match bap[bin] {
                        0 => 0,
                        1 => sym_quant(c, e, 3),
                        2 => sym_quant(c, e, 5),
                        3 => sym_quant(c, e, 7),
                        4 => sym_quant(c, e, 11),
                        5 => sym_quant(c, e, 15),
                        14 => asym_quant(c, e, 14),
                        15 => asym_quant(c, e, 16),
                        b => asym_quant(c, e, i32::from(b) - 1),
                    };
                    let v = match bap[bin] {
                        1 => groups.one.add(&mut self.qmant, i, bin, level, 3),
                        2 => groups.two.add(&mut self.qmant, i, bin, level, 5),
                        4 => groups.four.add(&mut self.qmant, i, bin, level, 11),
                        _ => level,
                    };
                    self.qmant.data[i][bin] = v as i16;
                    if let Some(r) = self.reconstruction.as_mut() {
                        r.coef.data[i][bin] = dequantise(bap[bin], level) * (-e as f32).exp2();
                    }
                }
            }
        }
    }

    /// The frame as a decoder rebuilds it, into the reconstruction: coupling
    /// undone, rematrixing undone, each block's inverse transform windowed
    /// and overlapped with the last one's.
    fn rebuild(&mut self) {
        let Some(mut r) = self.reconstruction.take() else {
            return;
        };
        let mut coordinates = [[0f32; MAX_CPL_BANDS]; MAX_CHANNELS];
        for blk in 0..BLOCKS {
            if let Some(c) = self.coupling {
                let cpl = *r.coef.at(CPL, blk);
                for (ch, coordinates) in coordinates
                    .iter_mut()
                    .enumerate()
                    .take(self.fbw + 1)
                    .skip(1)
                {
                    // A block without new coordinates keeps the last ones;
                    // the first block always has new ones.
                    if self.new_coordinates[blk][ch] != 0 {
                        let coded = &self.coordinates[blk][ch];
                        for (band, value) in coordinates.iter_mut().enumerate().take(c.bands) {
                            *value = coupling_coordinate(
                                coded.master,
                                coded.exp[band],
                                coded.mant[band],
                            );
                        }
                    }
                    let out = r.coef.at_mut(ch, blk);
                    let mut bin = c.start;
                    for (band, &size) in c.band_sizes[..c.bands].iter().enumerate() {
                        for _ in 0..size {
                            out[bin] = cpl[bin] * coordinates[band];
                            bin += 1;
                        }
                    }
                }
            }
            if self.mode == ChannelMode::Stereo {
                let coefs = self.end_freq[1].min(self.end_freq[2]);
                for band in 0..self.rematrix_bands {
                    if !self.rematrix[blk][band] {
                        continue;
                    }
                    for i in REMATRIX_BANDS[band]..coefs.min(REMATRIX_BANDS[band + 1]) {
                        let (sum, difference) = (r.coef.at(1, blk)[i], r.coef.at(2, blk)[i]);
                        r.coef.at_mut(1, blk)[i] = sum + difference;
                        r.coef.at_mut(2, blk)[i] = sum - difference;
                    }
                }
            }
            for ch in 1..=self.channels {
                let mut inverse = r.inverse;
                self.mdct.inverse(r.coef.at(ch, blk), &mut inverse);
                let overlap = &mut r.overlap[ch - 1];
                let out = &mut r.output[self.map[ch - 1]][blk * BLOCK..(blk + 1) * BLOCK];
                for i in 0..BLOCK {
                    out[i] = overlap[i] + inverse[i] * self.window[i];
                    overlap[i] = inverse[BLOCK + i] * self.window[BLOCK - 1 - i];
                }
            }
        }
        self.reconstruction = Some(r);
    }

    fn count_frame_bits_fixed(&self) -> usize {
        let fbw = self.fbw;
        if self.eac3 {
            // FFmpeg's count, which assumes per-block exponent strategies:
            // a frame that takes the frame-level code spends less than this,
            // and the allocation is conservative by the difference.
            let mut bits = 16 + 35 + 1 + 1 + 1; // sync, BSI, metadata flags, addbsie
            bits += 2 + 10; // exponent strategy syntax, AHT, the frame's flags
            bits += BLOCKS * 2 * fbw; // exponent strategies
            if self.lfe {
                bits += BLOCKS;
            }
            bits += fbw * 5; // converter exponent strategies
            bits += 10; // SNR offsets
            bits += 1; // block start information
            bits += BLOCKS * (1 + 1 + 1); // dynamic range, spectral extension, SNR
            return bits + 1 + 1 + 16;
        }
        const MODE_BITS: [usize; 8] = [0, 0, 2, 2, 2, 4, 2, 4];
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

    /// What the fixed count overstates by this frame: the per-block exponent
    /// strategies it assumed, where the frame-level code was written.
    fn fixed_overcount(&self) -> usize {
        match self.frame_exp {
            Some(_) => (2 * BLOCKS - 5) * self.fbw,
            None => 0,
        }
    }

    fn count_frame_bits(&self) -> usize {
        let cpl = self.coupling;
        let eac3 = self.eac3;
        let mut bits = 0;
        if eac3 {
            if self.mode != ChannelMode::Mono {
                bits += BLOCKS; // coupling in use, then strategy exists
            }
            if cpl.is_some() {
                bits += if self.frame_exp.is_some() {
                    5
                } else {
                    2 * BLOCKS
                };
            }
            if self.skip_pending {
                bits += BLOCKS + 9 + 8 * self.skip_field.len();
            }
        }
        for blk in 0..BLOCKS {
            if blk == 0 {
                if !eac3 {
                    bits += 1; // coupling in use
                }
                if let Some(c) = &cpl {
                    if eac3 {
                        bits += 1; // enhanced coupling
                    }
                    if !eac3 || self.mode != ChannelMode::Stereo {
                        bits += self.fbw; // channels in coupling
                    }
                    if self.mode == ChannelMode::Stereo {
                        bits += 1; // phase flags in use
                    }
                    bits += 4 + 4;
                    bits += if eac3 { 1 } else { c.sub_bands - 1 }; // band structure
                }
            }
            if let Some(c) = &cpl {
                for ch in 1..=self.fbw {
                    let new = self.new_coordinates[blk][ch];
                    if !eac3 || new != 2 {
                        bits += 1;
                    }
                    if new != 0 {
                        bits += 2 + (4 + 4) * c.bands;
                    }
                }
            }
            if self.mode == ChannelMode::Stereo {
                if !eac3 || blk > 0 {
                    bits += 1;
                }
                if self.new_rematrix[blk] {
                    bits += self.rematrix_bands;
                }
            }
            for ch in 1..=self.fbw {
                if self.strategy[ch][blk] != EXP_REUSE {
                    if !self.in_coupling(ch) {
                        bits += 6; // bandwidth
                    }
                    bits += 2; // gain range
                }
            }
            if !eac3 {
                if cpl.is_some() {
                    bits += 2; // coupling exponent strategy
                }
                if blk == 0 {
                    bits += 6 + (self.channels + usize::from(cpl.is_some())) * (4 + 3);
                }
            }
            if cpl.is_some() {
                let new = self.new_cpl_leak[blk];
                if !eac3 || new != 2 {
                    bits += 1; // coupling leak exists
                }
                if new != 0 {
                    bits += 3 + 3;
                }
            }
        }
        bits
    }

    fn write(&mut self, out: &mut Vec<u8>) {
        let mut w = std::mem::take(&mut self.writer);
        w.clear();
        if self.eac3 {
            self.write_eac3_header(&mut w);
        } else {
            self.write_header(&mut w);
        }
        for blk in 0..BLOCKS {
            self.write_block(&mut w, blk);
        }
        // Everything but the auxiliary data bit, the CRC's reserved bit and
        // the CRC, which sit in the frame's last eighteen bits.
        debug_assert_eq!(
            w.bit_position() + 18 + self.fixed_overcount(),
            self.planned_bits,
            "the writer wrote what the allocation counted"
        );
        debug_assert!(self.planned_bits <= 8 * self.frame_size);
        w.align_to(8);
        let start = out.len();
        out.extend_from_slice(w.bytes());
        out.resize(start + self.frame_size, 0);
        if self.eac3 {
            self.finish_eac3_frame(&mut out[start..]);
        } else {
            self.finish_frame(&mut out[start..]);
        }
        self.writer = w;
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

    /// The E-AC-3 syncframe's header: the bitstream information of one
    /// independent substream, and the audio frame's — the flags saying which
    /// tools the blocks use, the coupling and exponent strategies, the SNR
    /// offsets for the whole frame.
    fn write_eac3_header(&self, w: &mut BitWriter) {
        let m = &self.metadata;
        w.put(16, 0x0b77);
        w.put(2, 0); // independent
        w.put(3, 0); // substream 0
        w.put(11, (self.frame_size / 2 - 1) as u32);
        w.put(2, self.sr_code as u32);
        w.put(2, 3); // six blocks
        w.put(3, self.mode as u32);
        w.put_bit(self.lfe);
        w.put(5, 16); // bsid
        w.put(5, (-i32::from(m.dialnorm)) as u32);
        w.put(1, 0); // no compression gain word
        w.put(1, 0); // no mixing metadata
        w.put(1, 0); // no informational metadata
        w.put(1, 0); // no additional bitstream information

        w.put_bit(self.frame_exp.is_none()); // exponent strategies per block
        w.put(1, 0); // no adaptive hybrid transform
        w.put(2, 0); // one SNR offset for the frame
        w.put(1, 0); // no transient pre-noise processing
        w.put(1, 0); // no block switching
        w.put(1, 0); // no dither flags
        w.put(1, 0); // default bit allocation model
        w.put(1, 0); // default fast gain
        w.put(1, 0); // no delta bit allocation
        w.put_bit(self.skip_pending); // skip fields
        w.put(1, 0); // no spectral extension attenuation
        if self.mode != ChannelMode::Mono {
            w.put_bit(self.coupling.is_some());
            for _ in 1..BLOCKS {
                w.put(1, 0); // the coupling strategy stands
            }
        }
        match &self.frame_exp {
            Some(codes) => {
                for code in &codes[usize::from(self.coupling.is_none())..=self.fbw] {
                    w.put(5, u32::from(*code));
                }
            }
            None => {
                for blk in 0..BLOCKS {
                    for ch in usize::from(self.coupling.is_none())..=self.fbw {
                        w.put(2, u32::from(self.strategy[ch][blk]));
                    }
                }
            }
        }
        if self.lfe {
            for blk in 0..BLOCKS {
                w.put(1, u32::from(self.strategy[self.channels][blk]));
            }
        }
        // What an E-AC-3 to AC-3 converter is to use, which is required with
        // six blocks: the frame-level code where there is one.
        for ch in 1..=self.fbw {
            w.put(5, self.frame_exp.map_or(0, |codes| u32::from(codes[ch])));
        }
        w.put(6, self.coarse_snr as u32);
        w.put(4, self.fine_snr as u32);
        w.put(1, 0); // no block start information
    }

    fn write_block(&self, w: &mut BitWriter, blk: usize) {
        let eac3 = self.eac3;
        if !eac3 {
            for _ in 0..self.fbw {
                w.put(1, 0); // no block switching
            }
            for _ in 0..self.fbw {
                w.put(1, 1); // dither
            }
        }
        w.put(1, 0); // no dynamic range word
        if eac3 {
            w.put(1, 0); // no spectral extension
        }

        // Coupling strategy, stated once: every full-bandwidth channel in,
        // over the band FFmpeg would choose, in the default banding.
        if !eac3 {
            w.put_bit(blk == 0);
        }
        if blk == 0 {
            if !eac3 {
                w.put_bit(self.coupling.is_some());
            }
            if let Some(c) = &self.coupling {
                if eac3 {
                    w.put(1, 0); // not enhanced coupling
                }
                if !eac3 || self.mode != ChannelMode::Stereo {
                    for _ in 0..self.fbw {
                        w.put(1, 1);
                    }
                }
                if self.mode == ChannelMode::Stereo {
                    w.put(1, 0); // no phase flags
                }
                let start = (c.start - 37) / 12;
                let end = (c.end - 37) / 12;
                w.put(4, start as u32);
                w.put(4, (end - 3) as u32);
                if eac3 {
                    w.put(1, 0); // the default band structure
                } else {
                    for sub in start + 1..end {
                        w.put_bit(DEFAULT_CPL_BAND_STRUCT[sub]);
                    }
                }
            }
        }
        if let Some(c) = &self.coupling {
            for ch in 1..=self.fbw {
                let new = self.new_coordinates[blk][ch];
                if !eac3 || new != 2 {
                    w.put_bit(new != 0);
                }
                if new != 0 {
                    let coords = &self.coordinates[blk][ch];
                    w.put(2, u32::from(coords.master));
                    for band in 0..c.bands {
                        w.put(4, u32::from(coords.exp[band]));
                        w.put(4, u32::from(coords.mant[band]));
                    }
                }
            }
        }

        if self.mode == ChannelMode::Stereo {
            if !eac3 || blk > 0 {
                w.put_bit(self.new_rematrix[blk]);
            }
            if self.new_rematrix[blk] {
                for flag in &self.rematrix[blk][..self.rematrix_bands] {
                    w.put_bit(*flag);
                }
            }
        }
        if !eac3 {
            for ch in usize::from(self.coupling.is_none())..=self.fbw {
                w.put(2, u32::from(self.strategy[ch][blk]));
            }
            if self.lfe {
                w.put(1, u32::from(self.strategy[self.channels][blk]));
            }
        }
        for ch in 1..=self.fbw {
            if self.strategy[ch][blk] != EXP_REUSE && !self.in_coupling(ch) {
                w.put(6, u32::from(self.bandwidth_code));
            }
        }
        for ch in self.coded_channels() {
            let strategy = self.strategy[ch][blk];
            if strategy == EXP_REUSE {
                continue;
            }
            let cpl = ch == CPL;
            let grouped = self.grouped.at(ch, blk);
            w.put(4, u32::from(grouped[0] >> u8::from(cpl)));
            let coefs = self.end_freq[ch] - self.start_freq[ch];
            for group in &grouped[1..=exponent_groups(cpl, strategy, coefs)] {
                w.put(7, u32::from(*group));
            }
            if (1..=self.fbw).contains(&ch) {
                w.put(2, 0); // gain range
            }
        }
        if eac3 {
            w.put(1, 0); // no converter SNR offset
        } else {
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
                for _ in self.coded_channels() {
                    w.put(4, self.fine_snr as u32);
                    w.put(3, u32::from(self.fast_gain_code));
                }
            }
        }
        if self.coupling.is_some() {
            let new = self.new_cpl_leak[blk];
            if !eac3 || new != 2 {
                w.put_bit(new != 0); // coupling leak
            }
            if new != 0 {
                w.put(3, self.params.cpl_fast_leak as u32);
                w.put(3, self.params.cpl_slow_leak as u32);
            }
        }
        if !eac3 {
            w.put(1, 0); // no delta bit allocation
            w.put(1, 0); // no skipped data
        } else if self.skip_pending {
            // The one skip field, in block 2, where the reference streams
            // carry theirs.
            let here = blk == 2;
            w.put_bit(here);
            if here {
                w.put(9, self.skip_field.len() as u32);
                for byte in &self.skip_field {
                    w.put(8, u32::from(*byte));
                }
            }
        }

        for &ch in self.mantissa_order() {
            let bap = self.bap[self.best].at(ch, self.exp_ref[ch][blk]);
            let qmant = self.qmant.at(ch, blk);
            for bin in self.start_freq[ch]..self.end_freq[ch] {
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

    /// E-AC-3 has one CRC, over the whole frame past the sync word.
    fn finish_eac3_frame(&self, frame: &mut [u8]) {
        let size = self.frame_size;
        let mut crc2 = crc16(&frame[2..size - 2]);
        if crc2 == 0x0b77 {
            frame[size - 3] ^= 1;
            crc2 ^= 0x8005;
        }
        frame[size - 2..].copy_from_slice(&crc2.to_be_bytes());
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

/// A coefficient in 24-bit fixed point, rounded to nearest even as FFmpeg's
/// `lrintf` rounds, and clamped where FFmpeg does not clamp: a coefficient at
/// full scale has no exponent to be said with.
fn to_fixed24(value: f32) -> i32 {
    const LIMIT: f32 = 16_777_215.0;
    (value * 16_777_216.0)
        .round_ties_even()
        .clamp(-LIMIT, LIMIT) as i32
}

/// A fixed-point value's exponent: its leading zeros below bit 23, 24 for
/// nought.
fn exponent(value: i32) -> i32 {
    let v = value.unsigned_abs();
    if v == 0 { 24 } else { 23 - v.ilog2() as i32 }
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

/// A mantissa as a decoder reads it back, before its exponent: a symmetric
/// quantiser's level to the middle of its step, an asymmetric one's two's
/// complement to a fraction (A/52 §7.3.3).
fn dequantise(bap: u8, level: i32) -> f32 {
    let symmetric = |levels: i32| (2 * level - (levels - 1)) as f32 / levels as f32;
    match bap {
        0 => 0.0,
        1 => symmetric(3),
        2 => symmetric(5),
        3 => symmetric(7),
        4 => symmetric(11),
        5 => symmetric(15),
        14 => level as f32 / (1 << 13) as f32,
        15 => level as f32 / (1 << 15) as f32,
        b => level as f32 / (1 << (b - 2)) as f32,
    }
}

/// A coupling coordinate as a decoder reads it back (A/52 §7.4.3): its
/// mantissa a fraction, with an implied leading bit unless its exponent is
/// the largest, scaled down by the exponent and three steps of the master's
/// — and up by eight, the factor the encoder took out to keep it in range.
fn coupling_coordinate(master: u8, exp: u8, mant: u8) -> f32 {
    let mantissa = if exp == 15 {
        f32::from(mant) / 16.0
    } else {
        (f32::from(mant) + 16.0) / 32.0
    };
    8.0 * mantissa * (-(f32::from(exp) + 3.0 * f32::from(master))).exp2()
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
    let groups = exponent_groups(false, strategy, coefs) * 3;
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
    limit_steps(exp, 0, groups);
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

/// The same for the coupling channel, whose exponents start at `start` and
/// whose reference exponent — even, and the first group's — sits in the bin
/// before.
fn encode_cpl_exponents(exp: &mut [u8; COEFS], start: usize, coefs: usize, strategy: u8) {
    let groups = exponent_groups(true, strategy, coefs) * 3;
    let at = |i: usize| start + i;
    match strategy {
        EXP_D25 => {
            for i in 1..=groups {
                let k = 2 * (i - 1);
                exp[at(i - 1)] = exp[at(k)].min(exp[at(k + 1)]);
            }
        }
        EXP_D45 => {
            for i in 1..=groups {
                let k = 4 * (i - 1);
                exp[at(i - 1)] = exp[at(k)]
                    .min(exp[at(k + 1)])
                    .min(exp[at(k + 2)])
                    .min(exp[at(k + 3)]);
            }
        }
        _ => {}
    }
    // FFmpeg's steps run over one value past the groups, which is the bin
    // after the channel's last; kept, so that the streams agree.
    limit_steps(exp, start, groups);
    exp[start - 1] = exp[start] & !1;
    match strategy {
        EXP_D25 => {
            for i in (1..=groups).rev() {
                let v = exp[at(i - 1)];
                exp[at(2 * i - 1)] = v;
                exp[at(2 * i - 2)] = v;
            }
        }
        EXP_D45 => {
            for i in (1..=groups).rev() {
                let v = exp[at(i - 1)];
                exp[at(4 * i - 4)..=at(4 * i - 1)].fill(v);
            }
        }
        _ => {}
    }
}

/// Hold every step between `exp[base]` and `exp[base + groups]` to ±2, up
/// then down.
fn limit_steps(exp: &mut [u8; COEFS], base: usize, groups: usize) {
    for i in base + 1..=base + groups {
        exp[i] = exp[i].min(exp[i - 1] + 2);
    }
    for i in (base..base + groups).rev() {
        exp[i] = exp[i].min(exp[i + 1] + 2);
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
                    let mut s = sine(
                        220.0 * (ch + 1) as f32,
                        0.3,
                        f * FRAME_SAMPLES,
                        config.sample_rate as f32,
                    );
                    // And something in the high band, for coupling to carry.
                    for (n, v) in sine(
                        9000.0 + 500.0 * ch as f32,
                        0.05,
                        f * FRAME_SAMPLES,
                        config.sample_rate as f32,
                    )
                    .iter()
                    .enumerate()
                    {
                        s[n] += v;
                    }
                    s
                })
                .collect();
            let refs: Vec<&[f32]> = channels.iter().map(Vec::as_slice).collect();
            encoder.encode(&refs, &mut out).unwrap();
        }
        out
    }

    fn config(sample_rate: u32, mode: ChannelMode, lfe: bool, kbps: u32, coupling: bool) -> Config {
        Config {
            format: Format::Ac3,
            sample_rate,
            mode,
            lfe,
            bit_rate: Some(kbps),
            coupling,
            metadata: Metadata::default(),
        }
    }

    /// Asking for the reconstruction leaves the stream as it was, byte for
    /// byte, and what it reconstructs follows the input, [`DELAY`] late. That
    /// it is what a decoder plays is held to the workspace's decoder in
    /// `xtask` (`ac3::tests`).
    #[test]
    fn the_reconstruction_follows_the_input_and_leaves_the_stream_alone() {
        for format in [Format::Ac3, Format::Eac3] {
            let config = Config {
                format,
                ..config(48_000, ChannelMode::ThreeTwo, true, 640, false)
            };
            let mut encoder = Encoder::new(config).unwrap();
            encoder.reconstruct();
            let (mut stream, mut input, mut output) = (Vec::new(), Vec::new(), Vec::new());
            for f in 0..8 {
                let channels: Vec<Vec<f32>> = (0..encoder.channels())
                    .map(|ch| sine(220.0 * (ch + 1) as f32, 0.3, f * FRAME_SAMPLES, 48_000.0))
                    .collect();
                let refs: Vec<&[f32]> = channels.iter().map(Vec::as_slice).collect();
                encoder.encode(&refs, &mut stream).unwrap();
                input.extend_from_slice(&channels[0]);
                output.extend_from_slice(encoder.decoded(0).unwrap());
            }
            let mut again = Encoder::new(config).unwrap();
            let mut same = Vec::new();
            for f in 0..8 {
                let channels: Vec<Vec<f32>> = (0..again.channels())
                    .map(|ch| sine(220.0 * (ch + 1) as f32, 0.3, f * FRAME_SAMPLES, 48_000.0))
                    .collect();
                let refs: Vec<&[f32]> = channels.iter().map(Vec::as_slice).collect();
                again.encode(&refs, &mut same).unwrap();
            }
            assert_eq!(
                stream, same,
                "{format:?}: reconstructing changed the stream"
            );
            let (mut signal, mut error) = (0f64, 0f64);
            for n in FRAME_SAMPLES..input.len() - DELAY {
                let (x, y) = (f64::from(input[n]), f64::from(output[n + DELAY]));
                signal += x * x;
                error += (y - x) * (y - x);
            }
            let snr = 10.0 * (signal / error).log10();
            assert!(
                snr > 40.0,
                "{format:?}: the reconstruction follows at {snr:.1} dB"
            );
        }
    }

    /// Both CRCs check the way a decoder checks them: the first five-eighths
    /// past the sync word sum to nought, and so does the whole frame. In
    /// debug, every frame also holds the writer to the allocation's count.
    #[test]
    fn every_frame_carries_both_crcs() {
        for (rate, mode, lfe, kbps, coupling) in [
            (48_000, ChannelMode::ThreeTwo, true, 640, true),
            (48_000, ChannelMode::ThreeTwo, true, 384, true),
            (48_000, ChannelMode::Stereo, false, 192, true),
            (48_000, ChannelMode::Stereo, false, 192, false),
            (44_100, ChannelMode::ThreeTwo, true, 448, true),
            (44_100, ChannelMode::TwoOne, false, 320, true),
            (32_000, ChannelMode::Mono, false, 96, true),
        ] {
            let stream = encode(config(rate, mode, lfe, kbps, coupling), 8);
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

    /// Coupling is FFmpeg's choice: on for five channels at 448 kb/s, off at
    /// 640, off when asked.
    #[test]
    fn coupling_follows_the_rate() {
        let on = Encoder::new(config(48_000, ChannelMode::ThreeTwo, true, 448, true)).unwrap();
        assert!(on.coupled());
        let off = Encoder::new(config(48_000, ChannelMode::ThreeTwo, true, 640, true)).unwrap();
        assert!(!off.coupled());
        let asked = Encoder::new(config(48_000, ChannelMode::ThreeTwo, true, 448, false)).unwrap();
        assert!(!asked.coupled());
        let mono = Encoder::new(config(48_000, ChannelMode::Mono, false, 96, true)).unwrap();
        assert!(!mono.coupled());
    }

    /// E-AC-3 frames carry their one CRC, say their own size, come to the
    /// rate asked for on average whatever it is, and carry a skip field where
    /// they are given one — and in debug every frame holds the writer to the
    /// allocation's count, which differs from AC-3's in nearly every field.
    #[test]
    fn eac3_frames_check_and_keep_the_rate() {
        for (rate, mode, lfe, kbps, coupling, skip) in [
            (48_000, ChannelMode::ThreeTwo, true, 768, true, true),
            (48_000, ChannelMode::ThreeTwo, true, 384, true, true),
            (48_000, ChannelMode::ThreeTwo, true, 640, false, false),
            (48_000, ChannelMode::Stereo, false, 200, true, true),
            (44_100, ChannelMode::Stereo, false, 192, true, false),
            (32_000, ChannelMode::Mono, false, 64, true, true),
        ] {
            let mut encoder = Encoder::new(Config {
                format: Format::Eac3,
                ..config(rate, mode, lfe, kbps, coupling)
            })
            .unwrap();
            let frames = 60;
            let mut stream = Vec::new();
            for f in 0..frames {
                if skip {
                    encoder.carry_skip_field(&[0x58, 0x38, 0, 2, 0xab, f as u8]);
                }
                let planes: Vec<Vec<f32>> = (0..encoder.channels())
                    .map(|ch| sine(330.0 * (ch + 1) as f32, 0.3, f * FRAME_SAMPLES, rate as f32))
                    .collect();
                let refs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
                encoder.encode(&refs, &mut stream).unwrap();
            }
            let mut at = 0;
            let mut count = 0;
            while at < stream.len() {
                assert_eq!(&stream[at..at + 2], &[0x0b, 0x77]);
                let words =
                    (usize::from(stream[at + 2] & 7) << 8 | usize::from(stream[at + 3])) + 1;
                let frame = &stream[at..at + 2 * words];
                assert_eq!(crc16(&frame[2..]), 0, "{rate} Hz {kbps} kb/s");
                assert_eq!(frame[5] >> 3, 16, "bsid");
                at += 2 * words;
                count += 1;
            }
            assert_eq!(count, frames);
            let seconds = (frames * FRAME_SAMPLES) as f64 / f64::from(rate);
            let measured = stream.len() as f64 * 8.0 / seconds / 1000.0;
            assert!(
                (measured - f64::from(kbps)).abs() < 1.0,
                "{measured} kb/s for {kbps}"
            );
        }
    }

    #[test]
    fn eac3_rates_are_bounded_by_the_frame_size() {
        let at = |kbps| {
            Encoder::new(Config {
                format: Format::Eac3,
                ..config(48_000, ChannelMode::ThreeTwo, true, kbps, true)
            })
        };
        assert!(at(1024).is_ok());
        assert!(at(1025).is_err());
        // Below, a frame is a word or two: 512 bit/s at 48 kHz.
        assert!(at(1).is_ok());
        assert!(at(0).is_err());
    }

    /// At 44.1 kHz the two sizes alternate so that the average is the rate.
    #[test]
    fn the_average_rate_holds_at_44_1() {
        let frames = 200;
        let stream = encode(
            config(44_100, ChannelMode::Stereo, false, 192, true),
            frames,
        );
        let seconds = (frames * FRAME_SAMPLES) as f64 / 44_100.0;
        let rate = stream.len() as f64 * 8.0 / seconds;
        assert!((rate - 192_000.0).abs() < 200.0, "{rate}");
    }
}
