// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/dcaenc.c — Copyright (C) 2008-2012 Alexander E. Patrakov,
//     2010 Benjamin Larsson, 2011 Xiang Wang
//   libavcodec/dcaenc.h — Copyright (C) 2008-2012 Alexander E. Patrakov
// What was taken and what was changed is recorded in docs/provenance.md.

//! The core encoder: PCM in, one core frame per 512 samples out.
//!
//! FFmpeg's `dcaenc.c`, ported stage for stage: a 32-band polyphase
//! analysis, an LFE decimated 64 times, a masking curve from a 512-point
//! MDCT through 25 auditory filters, an SNR offset searched for until the
//! frame's bits fit, and a bitstream of one subframe of two subsubframes.
//! What it can do is what that encoder can — up to 5.1, the sample rates from
//! 8 to 48 kHz, every transmission rate, ADPCM by choice — and the stream is
//! that encoder's, byte for byte, except where the original has a defect: the
//! LFE history, which is optional here ([`Config::lfe_history_quirk`]), and
//! the step sizes at 96 kb/s with ADPCM, which are the decoders' here and
//! not FFmpeg's (see `State::step_size`).
//!
//! The arithmetic is FFmpeg's throughout, rounding for rounding, including the
//! fixed-point MDCT the masking model sees the signal through ([`crate::mdct`]):
//! the bit allocation follows from integer comparisons against thresholds,
//! and a model that is merely close moves the allocation of an occasional
//! band, which makes a stream that can no longer be compared byte for byte.
//! The tables built at construction come from `f64` arithmetic in the same
//! order FFmpeg's C evaluates it, through the platform's `libm`.
//!
//! Channels are indexed as FFmpeg indexes them: coded channels in their coded
//! order (centre, left, right, left surround, right surround for 5.1), then
//! the LFE. The input is interleaved 32-bit PCM in FFmpeg's — and WAV's —
//! order: L, R, C, LFE, Ls, Rs.
//!
//! Nothing allocates per frame. The state is set up once: about 120 kB, and
//! 160 kB more for the ADPCM predictor search when it is on.

use crate::adpcm::{self, Adpcm, mul32, norm, quantize_value};
use crate::core::{BIT_RATES, BITS_PER_SAMPLE, CoreHeader};
use crate::mdct::{self, Mdct};
use crate::tables::{
    ADPCM_COEFFS, BIT_CONSUMPTION, BITALLOC_12_COUNT, BITALLOC_OFFSETS, BITALLOC_SIZES,
    CHANNEL_REORDER_LFE, CHANNEL_REORDER_NOLFE, CODE_BOOKS, ERB, FC, FIR_32BANDS_NONPERFECT,
    LFE_FIR_64, LFE_INDEX, LOSSLESS_QUANT, LOSSY_QUANT, QUANT_INDEX_GROUP_SIZE,
    QUANT_INDEX_SEL_NBITS, QUANT_LEVELS, SAMPLE_RATES, SCALE_FACTOR_QUANT7, SCALEFACTOR_INV,
    STEPSIZE_INV, SoftFloat, VLC_SOURCE,
};
use hz_core::bits::BitWriter;

/// Samples per channel in a frame.
pub const FRAME_SAMPLES: usize = 512;

const MAX_CHANNELS: usize = 6;
const MAX_FRAME_SIZE: i64 = 16384;
const LFE_SAMPLES: usize = 8;
const SUBBANDS: usize = 32;
const SUBFRAMES: usize = 1;
const SUBSUBFRAMES: usize = 2;
const SUBBAND_SAMPLES: usize = SUBFRAMES * SUBSUBFRAMES * 8;
const AUBANDS: usize = 25;

/// The SNR offset's coarse step, in centibels.
const SNR_FUDGE: i32 = 128;
const USED_1ABITS: i32 = 1;
const USED_26ABITS: i32 = 4;

/// The channel layouts FFmpeg's encoder takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Mono,
    Stereo,
    /// L, R, Ls, Rs.
    TwoTwo,
    /// L, R, C, Ls, Rs.
    FivePointZero,
    /// L, R, C, LFE, Ls, Rs.
    FivePointOne,
}

impl Layout {
    /// Interleaved input channels, the LFE included.
    pub fn channels(self) -> usize {
        match self {
            Layout::Mono => 1,
            Layout::Stereo => 2,
            Layout::TwoTwo => 4,
            Layout::FivePointZero => 5,
            Layout::FivePointOne => 6,
        }
    }

    /// The core header's channel arrangement (`amode`).
    pub fn audio_mode(self) -> u8 {
        match self {
            Layout::Mono => 0,
            Layout::Stereo => 2,
            Layout::TwoTwo => 8,
            Layout::FivePointZero | Layout::FivePointOne => 9,
        }
    }

    pub fn has_lfe(self) -> bool {
        self == Layout::FivePointOne
    }
}

/// What to encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub layout: Layout,
    /// In hertz: 8, 11.025, 12, 16, 22.05, 24, 32, 44.1 or 48 kHz.
    pub sample_rate: u32,
    /// In bits a second. The frame is this rounded up to a whole number of
    /// 32-bit words; the header names the first standard rate at or above it.
    pub bit_rate: u32,
    /// Code subbands as ADPCM where prediction gains 10 dB: FFmpeg's
    /// `dca_adpcm` option, off by default there too.
    pub adpcm: bool,
    /// The header's source word-length code (`pcmr`): what the PCM was
    /// before it was coded, which the core does not depend on. FFmpeg writes
    /// 0, sixteen bits.
    pub pcm_resolution_code: u8,
    /// Reproduce FFmpeg's LFE history. FFmpeg's encoder keeps the LFE's
    /// history in the slot after the coded channels, but fills that slot from
    /// a channel-order entry that is −1 — the previous sample's *last input
    /// channel*, the right surround, rather than the LFE. So the first half
    /// of each LFE output's 512-tap window sees the right surround of the
    /// previous frame. Off, the history is the LFE's own; on, the stream is
    /// FFmpeg's byte for byte, which is what the comparison needs and nothing
    /// else does. (FFmpeg also reads one sample before its input buffer for
    /// this; that sample is taken as zero.)
    pub lfe_history_quirk: bool,
    /// The dialogue normalisation the header states, in dB of attenuation,
    /// 0 to 31: a decoder that applies it scales its output by `-dialnorm`
    /// dB. Neither FFmpeg nor `dca` applies it. 0, as the reference streams
    /// carry it, states none.
    pub dialnorm: u8,
}

impl Config {
    /// The reference streams' core: 5.1 at 48 kHz in 2012-byte frames —
    /// 1509 kb/s, under the 1536 kb/s its rate code names.
    pub fn reference_five_one() -> Self {
        Self {
            layout: Layout::FivePointOne,
            sample_rate: 48000,
            bit_rate: 1_509_000,
            adpcm: false,
            pcm_resolution_code: 0,
            lfe_history_quirk: false,
            dialnorm: 0,
        }
    }
}

/// The header's encoder revision and dialogue normalisation code for
/// `dialnorm` dB, 0 to 31 (ETSI TS 102 114 table 5-20): revision 7 states
/// 0 to 15 dB, revision 6 states 16 to 31. Revision 7 is also what the
/// header says with none, so a stream without dialnorm is unchanged.
pub fn dialnorm_fields(dialnorm: u8) -> Option<(u8, u8)> {
    match dialnorm {
        0..=15 => Some((7, dialnorm)),
        16..=31 => Some((6, dialnorm - 16)),
        _ => None,
    }
}

/// A configuration the encoder cannot make a stream for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub String);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

/// The Huffman codes the encoder writes, built from the decoder's source
/// tables as FFmpeg's `create_enc_table` builds them: each code is its rank
/// in the table, left-aligned in sixteen bits.
struct Codes {
    /// (code, length) by symbol, for every quantisation index codebook.
    quant: [(u16, u8); 1988],
    /// Where each (allocation code − 1, codebook) starts in `quant`.
    quant_start: [[u16; 7]; CODE_BOOKS],
    /// (code, length) by allocation code 1 to 12, for each of the five bit
    /// allocation codebooks; index 0 unused.
    bitalloc: [[(u16, u8); 13]; BITALLOC_12_COUNT],
}

const CODES: Codes = build_codes();

const fn build_codes() -> Codes {
    let mut codes = Codes {
        quant: [(0, 0); 1988],
        quant_start: [[0; 7]; CODE_BOOKS],
        bitalloc: [[(0, 0); 13]; BITALLOC_12_COUNT],
    };
    let mut src = 0;
    let mut dst = 0;
    let mut i = 0;
    while i < CODE_BOOKS {
        let mut j = 0;
        while j < QUANT_INDEX_GROUP_SIZE[i] as usize {
            codes.quant_start[i][j] = dst as u16;
            let count = BITALLOC_SIZES[i] as usize;
            let mut code = 0u32;
            let mut n = 0;
            while n < count {
                let (symbol, len) = VLC_SOURCE[src + n];
                codes.quant[dst + symbol as usize] = ((code >> (16 - len)) as u16, len);
                code += 1 << (16 - len);
                n += 1;
            }
            src += count;
            dst += count;
            j += 1;
        }
        i += 1;
    }
    let mut i = 0;
    while i < BITALLOC_12_COUNT {
        let mut code = 0u32;
        let mut n = 0;
        while n < 12 {
            let (symbol, len) = VLC_SOURCE[src + n];
            codes.bitalloc[i][1 + symbol as usize] = ((code >> (16 - len)) as u16, len);
            code += 1 << (16 - len);
            n += 1;
        }
        src += 12;
        i += 1;
    }
    codes
}

/// The code for quantised `value` in codebook `sel` of allocation code
/// `table + 1`.
#[inline(always)]
fn quant_code(table: usize, sel: usize, value: i32) -> (u16, u8) {
    let at = i32::from(CODES.quant_start[table][sel]) - i32::from(BITALLOC_OFFSETS[table]) + value;
    CODES.quant[at as usize]
}

/// The core encoder.
pub struct Encoder {
    config: Config,
    state: Box<State>,
    mdct: Mdct,
    adpcm: Option<Adpcm>,
    writer: BitWriter,
}

/// Everything per stream and per frame, in one allocation.
struct State {
    frame_size: usize,
    frame_bits: i32,
    fullband_channels: usize,
    channels: usize,
    lfe_channel: bool,
    sample_rate_code: u8,
    bit_rate_code: u8,
    audio_mode: u8,
    channel_order: [i8; 9],
    lfe_index: usize,
    lfe_scale_factor: i32,
    lfe_quant: SoftFloat,
    lfe_peak_cb: i32,

    prediction_mode: [[i32; SUBBANDS]; MAX_CHANNELS],
    /// Per subband: the four reconstructed samples the frame's prediction
    /// starts from, then the four it ends on.
    adpcm_history: [[[i32; 2 * ADPCM_COEFFS]; SUBBANDS]; MAX_CHANNELS],
    /// The last frame's input, per coded channel, then the LFE's.
    history: [[i32; FRAME_SAMPLES]; MAX_CHANNELS],
    /// Per subband: four samples of the previous frame, as the decoder will
    /// have reconstructed them, then this frame's sixteen.
    subband: [[[i32; ADPCM_COEFFS + SUBBAND_SAMPLES]; SUBBANDS]; MAX_CHANNELS],
    quantized: [[[i32; SUBBAND_SAMPLES]; SUBBANDS]; MAX_CHANNELS],
    peak_cb: [[i32; SUBBANDS]; MAX_CHANNELS],
    /// The expected peak of the prediction residual.
    diff_peak_cb: [[i32; SUBBANDS]; MAX_CHANNELS],
    downsampled_lfe: [i32; LFE_SAMPLES],
    /// The LFE samples quantised, of which the stream carries the low eight
    /// bits.
    lfe_codes: [i32; LFE_SAMPLES],
    masking_curve_cb: [[i32; 256]; SUBSUBFRAMES],
    bit_allocation_sel: [i32; MAX_CHANNELS],
    abits: [[i32; SUBBANDS]; MAX_CHANNELS],
    scale_factor: [[i32; SUBBANDS]; MAX_CHANNELS],
    quant: [[SoftFloat; SUBBANDS]; MAX_CHANNELS],
    quant_index_sel: [[i32; CODE_BOOKS]; MAX_CHANNELS],
    eff_masking_curve_cb: [i32; 256],
    band_masking_cb: [i32; 32],
    worst_quantization_noise: i32,
    worst_noise_ever: i32,
    consumed_bits: i32,
    /// Bits that carry the ADPCM predictor indices.
    consumed_adpcm_bits: i32,

    cos_table: [i32; 2048],
    band_interpolation: [i32; 512],
    band_spectrum: [i32; 8],
    /// The auditory filters' response, outer and middle ear included, at
    /// the stream's sample rate: centibels by filter and MDCT bin.
    auf: [[i32; 256]; AUBANDS],
    cb_to_add: [i32; 256],
    cb_to_level: [i32; 2048],
    lfe_fir_64i: [i32; 512],
}

/// `ff_exp10`, which FFmpeg computes as a power of two.
fn exp10(x: f64) -> f64 {
    (std::f64::consts::LOG2_10 * x).exp2()
}

/// Transfer function of the outer and middle ear, hertz to decibels (`hom`).
fn hom(f: f64) -> f64 {
    let f1 = f / 1000.0;
    -3.64 * f1.powf(-0.8) + 6.8 * (-0.6 * (f1 - 3.4) * (f1 - 3.4)).exp()
        - 6.0 * (-0.15 * (f1 - 8.7) * (f1 - 8.7)).exp()
        - 0.0006 * (f1 * f1) * (f1 * f1)
}

/// One auditory filter's response, in decibels (`gammafilter`).
fn gammafilter(i: usize, f: f64) -> f64 {
    let h = (f - f64::from(FC[i])) / f64::from(ERB[i]);
    let h = 1.0 + h * h;
    let h = 1.0 / (h * h);
    20.0 * h.log10()
}

impl Encoder {
    pub fn new(config: Config) -> Result<Self, Unsupported> {
        let channels = config.layout.channels();
        let lfe_channel = config.layout.has_lfe();
        let fullband_channels = channels - usize::from(lfe_channel);
        let audio_mode = config.layout.audio_mode();
        let channel_order = if lfe_channel {
            CHANNEL_REORDER_LFE[usize::from(audio_mode)]
        } else {
            CHANNEL_REORDER_NOLFE[usize::from(audio_mode)]
        };
        let Some(&(_, sample_rate_code)) = SAMPLE_RATES
            .iter()
            .find(|(rate, _)| *rate == config.sample_rate)
        else {
            return Err(Unsupported(format!(
                "{} Hz is not a core sample rate the encoder takes",
                config.sample_rate
            )));
        };
        if !(32_000..=3_840_000).contains(&config.bit_rate) {
            return Err(Unsupported(format!(
                "{} b/s is outside 32 kb/s to 3840 kb/s",
                config.bit_rate
            )));
        }
        if BITS_PER_SAMPLE
            .get(usize::from(config.pcm_resolution_code))
            .is_none_or(|&bits| bits == 0)
        {
            return Err(Unsupported(format!(
                "word-length code {} is not a defined one",
                config.pcm_resolution_code
            )));
        }
        if dialnorm_fields(config.dialnorm).is_none() {
            return Err(Unsupported(format!(
                "a dialnorm of {} dB is outside the 0 to 31 dB a core header states",
                config.dialnorm
            )));
        }
        let bit_rate_code = BIT_RATES
            .iter()
            .position(|&rate| rate >= config.bit_rate)
            .expect("the last named rate is 3840 kb/s") as u8;
        let bit_rate = i64::from(config.bit_rate);
        let sample_rate = i64::from(config.sample_rate);
        let frame_bits =
            ((bit_rate * FRAME_SAMPLES as i64 + sample_rate - 1) / sample_rate + 31) & !31;
        let min_frame_bits =
            132 + (493 + 28 * 32) * fullband_channels as i64 + i64::from(lfe_channel) * 72;
        if frame_bits < min_frame_bits || frame_bits > MAX_FRAME_SIZE * 8 {
            return Err(Unsupported(format!(
                "{} b/s makes a {frame_bits}-bit frame, outside {min_frame_bits} to {} bits",
                config.bit_rate,
                MAX_FRAME_SIZE * 8
            )));
        }

        let mut state = Box::new(State {
            frame_size: ((frame_bits + 7) / 8) as usize,
            frame_bits: frame_bits as i32,
            fullband_channels,
            channels,
            lfe_channel,
            sample_rate_code,
            bit_rate_code,
            audio_mode,
            channel_order,
            lfe_index: LFE_INDEX[usize::from(audio_mode)] as usize,
            lfe_scale_factor: 0,
            lfe_quant: SoftFloat::default(),
            lfe_peak_cb: 0,
            prediction_mode: [[-1; SUBBANDS]; MAX_CHANNELS],
            adpcm_history: [[[0; 2 * ADPCM_COEFFS]; SUBBANDS]; MAX_CHANNELS],
            history: [[0; FRAME_SAMPLES]; MAX_CHANNELS],
            subband: [[[0; ADPCM_COEFFS + SUBBAND_SAMPLES]; SUBBANDS]; MAX_CHANNELS],
            quantized: [[[0; SUBBAND_SAMPLES]; SUBBANDS]; MAX_CHANNELS],
            peak_cb: [[0; SUBBANDS]; MAX_CHANNELS],
            diff_peak_cb: [[0; SUBBANDS]; MAX_CHANNELS],
            downsampled_lfe: [0; LFE_SAMPLES],
            lfe_codes: [0; LFE_SAMPLES],
            masking_curve_cb: [[0; 256]; SUBSUBFRAMES],
            // Six: no Huffman code for the allocation.
            bit_allocation_sel: [6; MAX_CHANNELS],
            abits: [[0; SUBBANDS]; MAX_CHANNELS],
            scale_factor: [[0; SUBBANDS]; MAX_CHANNELS],
            quant: [[SoftFloat::default(); SUBBANDS]; MAX_CHANNELS],
            quant_index_sel: [QUANT_INDEX_GROUP_SIZE.map(i32::from); MAX_CHANNELS],
            eff_masking_curve_cb: [0; 256],
            band_masking_cb: [0; 32],
            worst_quantization_noise: -2047,
            worst_noise_ever: -2047,
            consumed_bits: 0,
            consumed_adpcm_bits: 0,
            cos_table: [0; 2048],
            band_interpolation: [0; 512],
            band_spectrum: [0; 8],
            auf: [[0; 256]; AUBANDS],
            cb_to_add: [0; 256],
            cb_to_level: [0; 2048],
            lfe_fir_64i: [0; 512],
        });
        state.init_tables(config.sample_rate);

        Ok(Self {
            adpcm: config.adpcm.then(Adpcm::new),
            config,
            state,
            mdct: Mdct::new(),
            writer: BitWriter::with_capacity(MAX_FRAME_SIZE as usize),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Bytes in every frame.
    pub fn frame_size(&self) -> usize {
        self.state.frame_size
    }

    /// Interleaved input channels, the LFE included.
    pub fn channels(&self) -> usize {
        self.state.channels
    }

    /// The header every frame carries.
    pub fn header(&self) -> CoreHeader {
        let s = &self.state;
        let (revision, dialnorm_code) =
            dialnorm_fields(self.config.dialnorm).expect("the dialnorm is checked on creation");
        CoreHeader {
            normal_frame: true,
            crc_present: false,
            pcm_blocks: SUBBAND_SAMPLES as u32,
            frame_size: s.frame_size as u32,
            audio_mode: s.audio_mode,
            sample_rate_code: s.sample_rate_code,
            bit_rate_code: s.bit_rate_code,
            drc_present: false,
            timestamp_present: false,
            aux_present: false,
            hdcd_master: false,
            ext_audio_type: 0,
            ext_audio_present: false,
            sync_ssf: false,
            lfe: if s.lfe_channel { 2 } else { 0 },
            predictor_history: true,
            header_crc: None,
            filter_perfect: false,
            encoder_revision: revision,
            copy_history: 0,
            pcm_resolution_code: self.config.pcm_resolution_code,
            sumdiff_front: false,
            sumdiff_surround: false,
            dialnorm_code,
        }
    }

    /// Encode one frame — [`FRAME_SAMPLES`] samples of every channel,
    /// interleaved — and append it to `out`.
    ///
    /// # Panics
    /// If `pcm` is not exactly one frame.
    pub fn encode(&mut self, pcm: &[i32], out: &mut Vec<u8>) {
        assert_eq!(
            pcm.len(),
            FRAME_SAMPLES * self.state.channels,
            "a frame is {FRAME_SAMPLES} samples of each channel"
        );
        let s = &mut *self.state;
        s.subband_transform(pcm);
        if s.lfe_channel {
            s.lfe_downsample(pcm);
        }
        s.calc_masking(&mut self.mdct, pcm);
        if let Some(adpcm) = &self.adpcm {
            s.adpcm_analysis(adpcm);
        }
        s.find_peaks();
        s.assign_bits();
        if s.lfe_channel {
            let (scale, quant) = s.calc_one_scale(s.lfe_peak_cb, 11);
            s.lfe_scale_factor = scale;
            s.lfe_quant = quant;
            for (code, &sample) in s.lfe_codes.iter_mut().zip(&s.downsampled_lfe) {
                *code = quantize_value(sample, quant);
            }
        }
        s.shift_history(pcm, self.config.lfe_history_quirk);
        s.fill_in_adpcm_buffer();

        let header = self.header();
        let s = &*self.state;
        let w = &mut self.writer;
        w.clear();
        header.write(w);
        s.put_primary_audio_header(w);
        for _ in 0..SUBFRAMES {
            s.put_subframe(w);
        }
        debug_assert!(
            w.bit_position() <= s.frame_bits as usize,
            "{} bits written into a {}-bit frame",
            w.bit_position(),
            s.frame_bits
        );
        w.align_to(8);
        let start = out.len();
        out.extend_from_slice(w.bytes());
        out.resize(start + s.frame_size, 0);
    }

    /// Forget the subband prediction history, as though the frame just
    /// written had not been: what a stream that leaves it out needs, since
    /// its decoder starts the next frame's ADPCM from silence.
    pub(crate) fn restart_prediction(&mut self) {
        let s = &mut *self.state;
        for ch in 0..s.fullband_channels {
            for band in 0..SUBBANDS {
                s.adpcm_history[ch][band] = [0; 2 * ADPCM_COEFFS];
                s.subband[ch][band][..ADPCM_COEFFS].fill(0);
            }
        }
    }

    /// Coded channels, the LFE apart.
    pub(crate) fn coded_channels(&self) -> usize {
        self.state.fullband_channels
    }

    /// The input channel coded channel `ch` is made from.
    pub(crate) fn input_channel(&self, ch: usize) -> usize {
        self.state.channel_order[ch] as usize
    }

    /// Whether the steps are the lossless table's: never at a rate this
    /// encoder writes (see `State::step_size`).
    pub(crate) fn lossless_steps(&self) -> bool {
        BIT_RATES[usize::from(self.state.bit_rate_code)] == 3
    }

    /// Subband `band` of coded channel `ch` as the frame last written sends
    /// it.
    pub(crate) fn subband(&self, ch: usize, band: usize) -> Coded<'_> {
        let s = &self.state;
        let abits = s.abits[ch][band];
        let huffman = (1..=CODE_BOOKS as i32).contains(&abits)
            && s.quant_index_sel[ch][abits as usize - 1]
                < i32::from(QUANT_INDEX_GROUP_SIZE[abits as usize - 1]);
        let mode = s.prediction_mode[ch][band];
        Coded {
            abits,
            scale_index: s.scale_factor[ch][band] as usize,
            predictor: (mode >= 0).then_some(mode as usize),
            huffman,
            // Neither Huffman nor block codes: plain words.
            linear: !huffman && abits > 7,
            samples: &s.quantized[ch][band],
        }
    }

    /// The input channel of the LFE.
    pub(crate) fn lfe_channel(&self) -> usize {
        self.state.lfe_index
    }

    /// The LFE of the frame last written — its quantised samples, of which
    /// the stream carries the low eight bits, and its scale factor index —
    /// when there is one.
    pub(crate) fn lfe(&self) -> Option<(&[i32; LFE_SAMPLES], usize)> {
        let s = &self.state;
        s.lfe_channel
            .then_some((&s.lfe_codes, s.lfe_scale_factor as usize))
    }
}

/// One subband of a frame, as the stream carries it.
pub(crate) struct Coded<'a> {
    /// The allocation code; nought for a subband without samples.
    pub abits: i32,
    pub scale_index: usize,
    /// The ADPCM predictor's index, when the subband is predicted.
    pub predictor: Option<usize>,
    /// Huffman codes, for which the decoder adjusts the scale factor.
    pub huffman: bool,
    /// Plain words of `abits − 3` bits, which a decoder reads as signed.
    pub linear: bool,
    pub samples: &'a [i32; SUBBAND_SAMPLES],
}

impl State {
    fn init_tables(&mut self, sample_rate: u32) {
        use std::f64::consts::PI;

        self.cos_table[0] = 0x7fff_ffff;
        self.cos_table[512] = 0;
        self.cos_table[1024] = -self.cos_table[0];
        for i in 1..512 {
            let c = (f64::from(0x7fff_ffff) * (PI * i as f64 / 1024.0).cos()) as i32;
            self.cos_table[i] = c;
            self.cos_table[1024 - i] = -c;
            self.cos_table[1024 + i] = -c;
            self.cos_table[2048 - i] = c;
        }

        for (i, level) in self.cb_to_level.iter_mut().enumerate() {
            *level = (f64::from(0x7fff_ffff) * exp10(-0.005 * i as f64)) as i32;
        }

        // `0xffffff800000ULL * float` is a `float` product: the integer
        // rounds to 2^48 first.
        let lfe_scale = 0xffff_ff80_0000u64 as f32;
        for k in 0..32 {
            for j in 0..8 {
                let v = (lfe_scale * LFE_FIR_64[8 * k + j]) as i32;
                self.lfe_fir_64i[64 * j + k] = v;
                self.lfe_fir_64i[64 * (7 - j) + (63 - k)] = v;
            }
        }

        let interpolation_scale = 0x10_0000_0000u64 as f32;
        for (dst, &src) in self
            .band_interpolation
            .iter_mut()
            .zip(FIR_32BANDS_NONPERFECT.iter())
        {
            *dst = (interpolation_scale * src) as i32;
        }

        for (j, row) in self.auf.iter_mut().enumerate() {
            for (k, v) in row.iter_mut().enumerate() {
                let freq = f64::from(sample_rate) * (k as f64 + 0.5) / 512.0;
                *v = (10.0 * (hom(freq) + gammafilter(j, freq))) as i32;
            }
        }

        for (i, add) in self.cb_to_add.iter_mut().enumerate() {
            let a = 1.0 + exp10(-0.01 * i as f64);
            *add = (100.0 * a.log10()) as i32;
        }

        for (j, spectrum) in self.band_spectrum.iter_mut().enumerate() {
            let mut accum = 0.0f64;
            for (i, &fir) in FIR_32BANDS_NONPERFECT.iter().enumerate() {
                let reconst = f64::from(if i & 64 != 0 { -fir } else { fir });
                accum += reconst
                    * (2.0 * PI * (i as f64 + 0.5 - 256.0) * (j as f64 + 0.5) / 512.0).cos();
            }
            *spectrum = (200.0 * accum.log10()) as i32;
        }
    }

    #[inline(always)]
    fn cos_t(&self, x: usize) -> i32 {
        self.cos_table[x & 2047]
    }

    /// The 32-band polyphase analysis of each coded channel.
    fn subband_transform(&mut self, input: &[i32]) {
        let channels = self.channels;
        for ch in 0..self.fullband_channels {
            // The history is copied because the masking model needs it too.
            let mut hist = self.history[ch];
            let mut hist_start = 0;
            let chi = self.channel_order[ch] as usize;
            for subs in 0..SUBBAND_SAMPLES {
                // All the convolutions at once.
                let mut accum = [0i32; 64];
                let mut k = 0;
                for (j, i) in (hist_start..512).chain(0..hist_start).enumerate() {
                    accum[k] = accum[k].wrapping_add(mul32(hist[i], self.band_interpolation[j]));
                    k = (k + 1) & 63;
                }
                for k in 16..32 {
                    accum[k] = accum[k].wrapping_sub(accum[31 - k]);
                }
                for k in 32..48 {
                    accum[k] = accum[k].wrapping_add(accum[95 - k]);
                }
                for band in 0..SUBBANDS {
                    let mut resp = 0i32;
                    for (i, &a) in accum.iter().enumerate().take(48).skip(16) {
                        let s = (2 * band + 1) * (2 * (i + 16) + 1);
                        resp = resp.wrapping_add(mul32(a, self.cos_t(s << 3)) >> 3);
                    }
                    self.subband[ch][band][ADPCM_COEFFS + subs] = if (band + 1) & 2 != 0 {
                        resp.wrapping_neg()
                    } else {
                        resp
                    };
                }
                for i in 0..32 {
                    hist[i + hist_start] = input[(subs * 32 + i) * channels + chi];
                }
                hist_start = (hist_start + 32) & 511;
            }
        }
    }

    /// The LFE decimated 64 times, through the interpolator's filter.
    fn lfe_downsample(&mut self, input: &[i32]) {
        let channels = self.channels;
        let lfech = self.lfe_index;
        let mut hist = self.history[channels - 1];
        let mut hist_start = 0;
        for lfes in 0..LFE_SAMPLES {
            let mut accum = 0i32;
            for (j, i) in (hist_start..512).chain(0..hist_start).enumerate() {
                accum = accum.wrapping_add(mul32(hist[i], self.lfe_fir_64i[j]));
            }
            self.downsampled_lfe[lfes] = accum;
            for i in 0..64 {
                hist[i + hist_start] = input[(lfes * 64 + i) * channels + lfech];
            }
            hist_start = (hist_start + 64) & 511;
        }
    }

    /// `get_cb`: a level as negative centibels below full scale.
    #[inline(always)]
    fn get_cb(&self, input: i32) -> i32 {
        let input = input.wrapping_abs();
        let mut res = 0usize;
        let mut i = 1024;
        while i > 0 {
            if self.cb_to_level[i + res] >= input {
                res += i;
            }
            i >>= 1;
        }
        -(res as i32)
    }

    /// `add_cb`: the sum of two levels in centibels.
    #[inline(always)]
    fn add_cb(&self, a: i32, b: i32) -> i32 {
        let (a, b) = if a < b { (b, a) } else { (a, b) };
        if a - b >= 256 {
            return a;
        }
        a + self.cb_to_add[(a - b) as usize]
    }

    fn calc_power(&self, mdct: &mut Mdct, input: &[i32; 512], power: &mut [i32; 256]) {
        let mut data = [0i32; 2 * mdct::LEN];
        for (i, d) in data.iter_mut().enumerate() {
            let window = 0x3fff_ffff - (self.cos_t(4 * i + 2) >> 1);
            *d = norm(i64::from(mul32(input[i], window)), 4);
        }
        let mut coeff = [0i32; mdct::LEN];
        mdct.forward(&data, &mut coeff);
        for (p, &c) in power.iter_mut().zip(&coeff) {
            let cb = self.get_cb(c);
            *p = self.add_cb(cb, cb);
        }
    }

    /// The just-noticeable difference of one block of one channel, folded
    /// into `out_cb`.
    fn adjust_jnd(&self, mdct: &mut Mdct, input: &[i32; 512], out_cb: &mut [i32; 256]) {
        const CA_CB: i32 = -1114;
        const CS_CB: i32 = 928;
        let mut power = [0i32; 256];
        self.calc_power(mdct, input, &mut power);
        // Can only grow.
        let mut out_cb_unnorm = [-2047i32; 256];
        for auf in &self.auf {
            let mut denom = CA_CB;
            for (&p, &a) in power.iter().zip(auf) {
                denom = self.add_cb(denom, p + a);
            }
            for (u, &a) in out_cb_unnorm.iter_mut().zip(auf) {
                *u = self.add_cb(*u, -denom + a);
            }
        }
        for (o, &u) in out_cb.iter_mut().zip(&out_cb_unnorm) {
            *o = self.add_cb(*o, -u - CA_CB - CS_CB);
        }
    }

    fn calc_masking(&mut self, mdct: &mut Mdct, input: &[i32]) {
        let mut masking = [[-2047i32; 256]; SUBSUBFRAMES];
        let mut data = [0i32; 512];
        for (ssf, curve) in masking.iter_mut().enumerate() {
            for ch in 0..self.fullband_channels {
                let chi = self.channel_order[ch] as usize;
                let first = 128 + 256 * ssf;
                let from_history = 512 - first;
                data[..from_history].copy_from_slice(&self.history[ch][first..]);
                for (k, d) in data[from_history..].iter_mut().enumerate() {
                    *d = input[k * self.channels + chi];
                }
                self.adjust_jnd(mdct, &data, curve);
            }
        }
        self.masking_curve_cb = masking;
        for i in 0..256 {
            let mut m = 2048;
            for curve in &self.masking_curve_cb {
                m = m.min(curve[i]);
            }
            self.eff_masking_curve_cb[i] = m;
        }

        for band in 0..32 {
            self.band_masking_cb[band] = 2048;
            // `walk_band_low` and `walk_band_high` with
            // `update_band_masking`: only the first band and the first
            // spectrum value are used.
            let mut update = |f: usize, spectrum: i32| {
                let value = self.eff_masking_curve_cb[f] - spectrum;
                if value < self.band_masking_cb[band] {
                    self.band_masking_cb[band] = value;
                }
            };
            if band == 0 {
                for f in 0..4 {
                    update(f, 0);
                }
            } else {
                for f in 0..8 {
                    update(8 * band - 4 + f, self.band_spectrum[7 - f]);
                }
            }
            if band == 31 {
                for f in 0..4 {
                    update(256 - 4 + f, 0);
                }
            } else {
                for f in 0..8 {
                    update(8 * band + 4 + f, self.band_spectrum[f]);
                }
            }
        }
    }

    fn find_peak(&self, input: &[i32]) -> i32 {
        let m = input.iter().fold(0i32, |m, &s| m.max(s.wrapping_abs()));
        self.get_cb(m)
    }

    fn find_peaks(&mut self) {
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                self.peak_cb[ch][band] = self.find_peak(&self.subband[ch][band][ADPCM_COEFFS..]);
            }
        }
        if self.lfe_channel {
            self.lfe_peak_cb = self.find_peak(&self.downsampled_lfe);
        }
    }

    fn adpcm_analysis(&mut self, adpcm: &Adpcm) {
        self.consumed_adpcm_bits = 0;
        let mut estimated_diff = [0i32; SUBBAND_SAMPLES];
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                match adpcm.subband_analysis(&self.subband[ch][band], &mut estimated_diff) {
                    Some(vq) => {
                        self.prediction_mode[ch][band] = i32::from(vq);
                        // Twelve bits for the predictor's index.
                        self.consumed_adpcm_bits += 12;
                        self.diff_peak_cb[ch][band] = self.find_peak(&estimated_diff);
                    }
                    None => self.prediction_mode[ch][band] = -1,
                }
            }
        }
    }

    /// The quantiser step a decoder dequantises with: the lossless table
    /// only when the rate *named by the code* is the lossless sentinel.
    /// FFmpeg's encoder compares the code itself with 3 — which is 96 kb/s —
    /// while its decoder, and every other, compares the rate; at 96 kb/s with
    /// ADPCM its predictor history then follows the wrong steps and the
    /// stream decodes to noise. Here the encoder agrees with the decoders,
    /// which leaves it on the lossy table at every rate it can write.
    fn step_size(&self, ch: usize, band: usize) -> i32 {
        let abits = self.abits[ch][band] as usize;
        if BIT_RATES[usize::from(self.bit_rate_code)] == 3 {
            LOSSLESS_QUANT[abits] as i32
        } else {
            LOSSY_QUANT[abits] as i32
        }
    }

    /// `calc_one_scale`: the smallest scale factor at which a peak of
    /// `peak_cb` still fits the quantiser, and the multiplier that goes with
    /// it.
    fn calc_one_scale(&self, peak_cb: i32, abits: i32) -> (i32, SoftFloat) {
        debug_assert!((-2047..=0).contains(&peak_cb));
        let abits = abits as usize;
        let peak = self.cb_to_level[(-peak_cb) as usize];
        let limit = (QUANT_LEVELS[abits] as i32 - 1) / 2;
        let mut nscale = 127usize;
        let mut try_remove = 64;
        while try_remove > 0 {
            let sf = SCALEFACTOR_INV[nscale - try_remove];
            let step = STEPSIZE_INV[abits];
            if sf.e + step.e > 17 {
                let quant = SoftFloat {
                    m: mul32(sf.m, step.m),
                    e: sf.e + step.e - 17,
                };
                if limit >= quantize_value(peak, quant) {
                    nscale -= try_remove;
                }
            }
            try_remove >>= 1;
        }
        if nscale >= 125 {
            nscale = 124;
        }
        let quant = SoftFloat {
            m: mul32(SCALEFACTOR_INV[nscale].m, STEPSIZE_INV[abits].m),
            e: SCALEFACTOR_INV[nscale].e + STEPSIZE_INV[abits].e - 17,
        };
        debug_assert!(limit >= quantize_value(peak, quant));
        (nscale as i32, quant)
    }

    fn quantize_adpcm(&mut self) {
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                let vq = self.prediction_mode[ch][band];
                if vq < 0 {
                    continue;
                }
                let diff_peak_cb = self.diff_peak_cb[ch][band];
                let (scale, quant) = self.calc_one_scale(diff_peak_cb, self.abits[ch][band]);
                self.scale_factor[ch][band] = scale;
                self.quant[ch][band] = quant;
                let step_size = self.step_size(ch, band);
                let peak = self.cb_to_level[(-diff_peak_cb) as usize];
                let history = &mut self.adpcm_history[ch][band];
                let prev: [i32; ADPCM_COEFFS] = history[..ADPCM_COEFFS].try_into().unwrap();
                let next: &mut [i32; ADPCM_COEFFS] =
                    (&mut history[ADPCM_COEFFS..]).try_into().unwrap();
                adpcm::code_subband(
                    vq as usize,
                    quant,
                    SCALE_FACTOR_QUANT7[scale as usize] as i32,
                    step_size,
                    &prev,
                    &self.subband[ch][band][ADPCM_COEFFS..],
                    next,
                    &mut self.quantized[ch][band],
                    peak,
                );
            }
        }
    }

    fn quantize_pcm(&mut self) {
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                if self.prediction_mode[ch][band] != -1 {
                    continue;
                }
                let quant = self.quant[ch][band];
                for (q, &s) in self.quantized[ch][band]
                    .iter_mut()
                    .zip(&self.subband[ch][band][ADPCM_COEFFS..])
                {
                    *q = quantize_value(s, quant);
                }
            }
        }
    }

    /// `set_best_abits_code`: the cheapest way to send a channel's
    /// allocation, and what it costs.
    fn best_abits_code(abits: &[i32; SUBBANDS]) -> (i32, i32) {
        let mut best_sel = 6;
        let mut best_bits = SUBBANDS as i32 * 5;
        // An allocation the Huffman codebooks cannot say goes linear.
        if abits.iter().any(|&a| a > 12 || a == 0) {
            return (best_sel, best_bits);
        }
        for (sel, book) in CODES.bitalloc.iter().enumerate() {
            let bits: i32 = abits.iter().map(|&a| i32::from(book[a as usize].1)).sum();
            if bits < best_bits {
                best_bits = bits;
                best_sel = sel as i32;
            }
        }
        (best_sel, best_bits)
    }

    /// `set_best_code`: per allocation code, Huffman or not, and with which
    /// codebook; the bits that costs.
    fn best_code(
        vlc_bits: &[[u32; 7]; CODE_BOOKS],
        clc_bits: &[u32; CODE_BOOKS],
        res: &mut [i32; CODE_BOOKS],
    ) -> u32 {
        let mut bits = 0u32;
        for i in 0..CODE_BOOKS {
            debug_assert_eq!(vlc_bits[i][0] != 0, clc_bits[i] != 0);
            let group = i32::from(QUANT_INDEX_GROUP_SIZE[i]);
            if vlc_bits[i][0] == 0 {
                // No adjustment index for a codebook nothing uses.
                res[i] = group;
                continue;
            }
            let mut best_bits = vlc_bits[i][0];
            let mut best_id = 0;
            for (sel, &sel_bits) in vlc_bits[i].iter().enumerate().take(group as usize) {
                if best_bits > sel_bits && sel_bits != 0 {
                    best_bits = sel_bits;
                    best_id = sel as i32;
                }
            }
            // Two bits for the scale factor adjustment index.
            let t = best_bits + 2;
            if t < clc_bits[i] {
                res[i] = best_id;
                bits += t;
            } else {
                res[i] = group;
                bits += clc_bits[i];
            }
        }
        bits
    }

    /// `init_quantization_noise`: allocate, scale and quantise at SNR offset
    /// `noise`, and count the bits. Returns which of the extreme allocations
    /// every band got.
    fn init_quantization_noise(&mut self, noise: i32, forbid_zero: bool) -> i32 {
        let mut ret = USED_26ABITS | USED_1ABITS;
        self.consumed_bits = 132 + 333 * self.fullband_channels as i32;
        self.consumed_bits += self.consumed_adpcm_bits;
        if self.lfe_channel {
            self.consumed_bits += 72;
        }

        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                let snr_cb = self.peak_cb[ch][band] - self.band_masking_cb[band] - noise;
                let abits = if snr_cb >= 1312 {
                    ret &= !USED_1ABITS;
                    26
                } else if snr_cb >= 222 {
                    ret &= !(USED_26ABITS | USED_1ABITS);
                    8 + mul32(snr_cb - 222, 69_000_000)
                } else if snr_cb >= 0 {
                    ret &= !(USED_26ABITS | USED_1ABITS);
                    2 + mul32(snr_cb, 106_000_000)
                } else if forbid_zero || snr_cb >= -140 {
                    ret &= !USED_26ABITS;
                    1
                } else {
                    ret &= !(USED_26ABITS | USED_1ABITS);
                    0
                };
                self.abits[ch][band] = abits;
            }
            let (sel, bits) = Self::best_abits_code(&self.abits[ch]);
            self.bit_allocation_sel[ch] = sel;
            self.consumed_bits += bits;
        }

        // Scale factors again on every pass, to count the Huffman codes.
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                if self.prediction_mode[ch][band] == -1 {
                    let (scale, quant) =
                        self.calc_one_scale(self.peak_cb[ch][band], self.abits[ch][band]);
                    self.scale_factor[ch][band] = scale;
                    self.quant[ch][band] = quant;
                }
            }
        }
        self.quantize_adpcm();
        self.quantize_pcm();

        let mut huff_bits = [[[0u32; 7]; CODE_BOOKS]; MAX_CHANNELS];
        let mut clc_bits = [[0u32; CODE_BOOKS]; MAX_CHANNELS];
        let mut bits_counter = 0u32;
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                let abits = self.abits[ch][band];
                if abits != 0 && abits as usize <= CODE_BOOKS {
                    let id = abits as usize - 1;
                    let group = usize::from(QUANT_INDEX_GROUP_SIZE[id]);
                    for (sel, total) in huff_bits[ch][id].iter_mut().enumerate().take(group) {
                        *total += self.quantized[ch][band]
                            .iter()
                            .map(|&v| u32::from(quant_code(id, sel, v).1))
                            .sum::<u32>();
                    }
                    clc_bits[ch][id] += BIT_CONSUMPTION[abits as usize] as u32;
                } else {
                    // Code nought costs −8: no transition mode, no scale factor.
                    bits_counter =
                        bits_counter.wrapping_add(BIT_CONSUMPTION[abits as usize] as u32);
                }
            }
        }
        for ch in 0..self.fullband_channels {
            bits_counter = bits_counter.wrapping_add(Self::best_code(
                &huff_bits[ch],
                &clc_bits[ch],
                &mut self.quant_index_sel[ch],
            ));
        }
        self.consumed_bits = self.consumed_bits.wrapping_add(bits_counter as i32);
        ret
    }

    /// `assign_bits`: the lowest SNR offset whose frame fits, by a coarse
    /// walk from last frame's and a binary search.
    fn assign_bits(&mut self) {
        let mut used_abits = 0;
        let mut forbid_zero = true;
        let mut high = 'restart: loop {
            let worst = self.worst_quantization_noise;
            self.init_quantization_noise(worst, forbid_zero);
            let mut low = worst;
            let mut high = worst;
            if self.consumed_bits > self.frame_bits {
                while self.consumed_bits > self.frame_bits {
                    if used_abits == USED_1ABITS && forbid_zero {
                        forbid_zero = false;
                        continue 'restart;
                    }
                    high += SNR_FUDGE;
                    used_abits = self.init_quantization_noise(high, forbid_zero);
                }
            } else {
                while self.consumed_bits <= self.frame_bits {
                    high = low;
                    if used_abits == USED_26ABITS {
                        // The rate is more than the signal needs: pad.
                        self.finish_assign(high);
                        return;
                    }
                    low -= SNR_FUDGE;
                    used_abits = self.init_quantization_noise(low, forbid_zero);
                }
            }
            break high;
        };
        let mut down = SNR_FUDGE >> 1;
        while down > 0 {
            self.init_quantization_noise(high - down, forbid_zero);
            if self.consumed_bits <= self.frame_bits {
                high -= down;
            }
            down >>= 1;
        }
        self.init_quantization_noise(high, forbid_zero);
        self.finish_assign(high);
    }

    fn finish_assign(&mut self, high: i32) {
        self.worst_quantization_noise = high;
        self.worst_noise_ever = self.worst_noise_ever.max(high);
    }

    fn shift_history(&mut self, input: &[i32], lfe_history_quirk: bool) {
        let channels = self.channels;
        for ch in 0..self.fullband_channels {
            let chi = self.channel_order[ch] as usize;
            for k in 0..FRAME_SAMPLES {
                self.history[ch][k] = input[k * channels + chi];
            }
        }
        if self.lfe_channel {
            let lfe = &mut self.history[channels - 1];
            if lfe_history_quirk {
                // FFmpeg: `input[k * channels + (-1)]`.
                lfe[0] = 0;
                for k in 1..FRAME_SAMPLES {
                    lfe[k] = input[k * channels - 1];
                }
            } else {
                for k in 0..FRAME_SAMPLES {
                    lfe[k] = input[k * channels + self.lfe_index];
                }
            }
        }
    }

    /// `fill_in_adpcm_bufer`: carry each subband's last four samples, as
    /// the decoder reconstructs them, to the next frame — the prediction
    /// there starts from them, and the analysis looks at them rather than
    /// at the originals, which the decoder does not have.
    fn fill_in_adpcm_buffer(&mut self) {
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                if self.prediction_mode[ch][band] == -1 {
                    let step_size = self.step_size(ch, band);
                    let scale = SCALE_FACTOR_QUANT7[self.scale_factor[ch][band] as usize] as i32;
                    let quantized = self.quantized[ch][band];
                    adpcm::dequantize(
                        &mut self.adpcm_history[ch][band][..ADPCM_COEFFS],
                        &quantized[12..],
                        step_size,
                        scale,
                    );
                } else {
                    self.adpcm_history[ch][band].copy_within(ADPCM_COEFFS.., 0);
                }
                for i in 0..ADPCM_COEFFS {
                    self.subband[ch][band][i] =
                        self.adpcm_history[ch][band][i].wrapping_mul(1 << 7);
                }
            }
        }
    }

    fn put_primary_audio_header(&self, w: &mut BitWriter) {
        let channels = self.fullband_channels;
        // Subframes.
        w.put(4, (SUBFRAMES - 1) as u32);
        // Primary audio channels.
        w.put(3, (channels - 1) as u32);
        // Subband activity count: all 32.
        for _ in 0..channels {
            w.put(5, (SUBBANDS - 2) as u32);
        }
        // High-frequency VQ start subband: none.
        for _ in 0..channels {
            w.put(5, (SUBBANDS - 1) as u32);
        }
        // Joint intensity coding: off.
        for _ in 0..channels {
            w.put(3, 0);
        }
        // Transient mode codebook: A4.
        for _ in 0..channels {
            w.put(2, 0);
        }
        // Scale factor codebook: 7-bit linear.
        for _ in 0..channels {
            w.put(3, 6);
        }
        // Bit allocation quantiser.
        for &sel in &self.bit_allocation_sel[..channels] {
            w.put(3, sel as u32);
        }
        // Quantisation index codebooks.
        for i in 0..CODE_BOOKS {
            for sel in &self.quant_index_sel[..channels] {
                w.put(u32::from(QUANT_INDEX_SEL_NBITS[i]), sel[i] as u32);
            }
        }
        // Scale factor adjustment, sent where a Huffman codebook is used.
        for i in 0..CODE_BOOKS {
            for sel in &self.quant_index_sel[..channels] {
                if sel[i] < i32::from(QUANT_INDEX_GROUP_SIZE[i]) {
                    w.put(2, 0);
                }
            }
        }
        // No audio header checksum.
    }

    fn put_subframe_samples(&self, w: &mut BitWriter, ss: usize, band: usize, ch: usize) {
        let abits = self.abits[ch][band];
        let samples = &self.quantized[ch][band][ss * 8..ss * 8 + 8];
        if abits as usize <= CODE_BOOKS {
            debug_assert!(abits > 0);
            let id = abits as usize - 1;
            let sel = self.quant_index_sel[ch][id];
            if sel < i32::from(QUANT_INDEX_GROUP_SIZE[id]) {
                for &v in samples {
                    let (code, len) = quant_code(id, sel as usize, v);
                    w.put(u32::from(len), u32::from(code));
                }
                return;
            }
            // Block codes: four samples to a word.
            if abits <= 7 {
                let levels = QUANT_LEVELS[abits as usize] as i32;
                for i in (0..8).step_by(4) {
                    let mut sum = 0i32;
                    for j in (0..4).rev() {
                        sum *= levels;
                        sum += samples[i + j];
                        sum += (levels - 1) / 2;
                    }
                    w.put((BIT_CONSUMPTION[abits as usize] / 4) as u32, sum as u32);
                }
                return;
            }
        }
        let bits = (BIT_CONSUMPTION[abits as usize] / 16) as u32;
        for &v in samples {
            w.put(bits, v as u32);
        }
    }

    fn put_subframe(&self, w: &mut BitWriter) {
        let channels = self.fullband_channels;
        // Subsubframes.
        w.put(2, (SUBSUBFRAMES - 1) as u32);
        // Partial subsubframe sample count.
        w.put(3, 0);
        // Prediction mode.
        for modes in &self.prediction_mode[..channels] {
            for &mode in modes {
                w.put_bit(mode != -1);
            }
        }
        // Prediction VQ address.
        for modes in &self.prediction_mode[..channels] {
            for &mode in modes {
                if mode >= 0 {
                    w.put(12, mode as u32);
                }
            }
        }
        // Bit allocation index.
        for ch in 0..channels {
            let sel = self.bit_allocation_sel[ch];
            if sel == 6 {
                for &a in &self.abits[ch] {
                    w.put(5, a as u32);
                }
            } else {
                let book = &CODES.bitalloc[sel as usize];
                for &a in &self.abits[ch] {
                    let (code, len) = book[a as usize];
                    w.put(u32::from(len), u32::from(code));
                }
            }
        }
        // Transition mode: none, in every band that is coded.
        if SUBSUBFRAMES > 1 {
            for abits in &self.abits[..channels] {
                for &a in abits {
                    if a != 0 {
                        w.put_bit(false);
                    }
                }
            }
        }
        // Scale factors.
        for ch in 0..channels {
            for band in 0..SUBBANDS {
                if self.abits[ch][band] != 0 {
                    w.put(7, self.scale_factor[ch][band] as u32);
                }
            }
        }
        // No joint subband scale factors, stereo downmix, dynamic range,
        // side information checksum or VQ-coded high subbands.

        // LFE: eight samples and a scale factor.
        if self.lfe_channel {
            for &code in &self.lfe_codes {
                w.put(8, (code & 0xff) as u32);
            }
            w.put(8, self.lfe_scale_factor as u32);
        }

        // Audio data, subsubframe by subsubframe.
        for ss in 0..SUBSUBFRAMES {
            for ch in 0..channels {
                for band in 0..SUBBANDS {
                    if self.abits[ch][band] != 0 {
                        self.put_subframe_samples(w, ss, band, ch);
                    }
                }
            }
        }

        // DSYNC.
        w.put(16, 0xffff);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn tone(frames: usize, channels: usize) -> Vec<i32> {
        let mut pcm = vec![0i32; frames * FRAME_SAMPLES * channels];
        for (n, frame) in pcm.chunks_exact_mut(channels).enumerate() {
            for (ch, s) in frame.iter_mut().enumerate() {
                let f = 220.0 * (ch + 1) as f64;
                let phase = 2.0 * std::f64::consts::PI * f * n as f64 / 48000.0;
                *s = (phase.sin() * 0.25 * 2_147_483_647.0) as i32;
            }
        }
        pcm
    }

    /// Six channels synthesised in integers alone, so that the input is the
    /// same on every platform: two tones from an integer resonator — which
    /// subband prediction pays on — noise through one-pole low-passes of
    /// different corners, and a square wave in the LFE.
    pub(crate) fn fixture(frames: usize) -> Vec<i32> {
        let mut seed = 0x2545_f491u32;
        let mut state = [0i32; 6];
        // y[n] = 2cos(w)·y[n−1] − y[n−2], at 440 Hz and 1250 Hz, started at
        // nought and A·sin(w) for amplitudes of 2^29 and 2^28.
        let mut tones = [[0i64, 30_904_452], [0i64, 43_726_921]];
        let coefficients = [2_143_922_726i64, 2_118_800_422];
        let mut pcm = Vec::with_capacity(frames * FRAME_SAMPLES * 6);
        for n in 0..frames * FRAME_SAMPLES {
            for ((tone, c), ch) in tones.iter_mut().zip(coefficients).zip([0, 4]) {
                let next = ((c * tone[1]) >> 30) - tone[0];
                *tone = [tone[1], next];
                state[ch] = next as i32;
            }
            for ch in [1, 2, 3, 5] {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let x = if ch == 3 {
                    if (n / 400) % 2 == 0 {
                        1 << 29
                    } else {
                        -(1 << 29)
                    }
                } else {
                    (seed as i32) >> 2
                };
                let y = &mut state[ch];
                *y += (x - *y) >> [0, 3, 5, 6, 0, 2][ch];
            }
            pcm.extend_from_slice(&state);
        }
        pcm
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        })
    }

    /// The streams FFmpeg n9.0.2's encoder writes from [`fixture`] —
    /// `ffmpeg -f s32le -ar 48000 -ch_layout '5.1(side)' -i fixture.s32
    /// -c:a dca -strict -2 -b:a 1509000 [-dca_adpcm 1] -f dts` — hashed. The
    /// port makes the same bytes, given FFmpeg's LFE history. The tables
    /// behind the masking model are built through the platform's `libm`, so
    /// one whose last bit differs from glibc's could move an entry, and this
    /// is the test that would say so.
    #[test]
    fn the_stream_is_ffmpegs() {
        for (adpcm, want) in [
            (false, 0xd007_6928_5ccf_adacu64),
            (true, 0x17eb_0831_c2b4_afcb),
        ] {
            let mut encoder = Encoder::new(Config {
                adpcm,
                lfe_history_quirk: true,
                ..Config::reference_five_one()
            })
            .unwrap();
            let mut out = Vec::new();
            for frame in fixture(12).chunks_exact(FRAME_SAMPLES * 6) {
                encoder.encode(frame, &mut out);
            }
            assert_eq!(out.len(), 12 * 2012);
            assert_eq!(fnv1a(&out), want, "adpcm {adpcm}");
        }
    }

    #[test]
    fn the_lfe_history_is_the_only_difference_from_ffmpeg() {
        let streams = [false, true].map(|lfe_history_quirk| {
            let mut encoder = Encoder::new(Config {
                lfe_history_quirk,
                ..Config::reference_five_one()
            })
            .unwrap();
            let mut out = Vec::new();
            for frame in fixture(12).chunks_exact(FRAME_SAMPLES * 6) {
                encoder.encode(frame, &mut out);
            }
            out
        });
        let [ours, theirs] = &streams;
        // The first frame's history is silence either way; after it, the
        // frames differ in at most the 72 bits of LFE data and scale factor.
        for (f, (a, b)) in ours.chunks(2012).zip(theirs.chunks(2012)).enumerate() {
            let differing: Vec<usize> = (0..2012 * 8)
                .filter(|&bit| (a[bit / 8] ^ b[bit / 8]) & (0x80 >> (bit % 8)) != 0)
                .collect();
            if f == 0 {
                assert!(differing.is_empty());
            } else if let (Some(first), Some(last)) = (differing.first(), differing.last()) {
                assert!(last - first < 72, "frame {f}: bits {first} to {last}");
            }
        }
        assert_ne!(ours, theirs);
    }

    #[test]
    fn the_codes_are_prefix_codes_and_fill_their_space() {
        // Kraft: a complete prefix code sums to exactly one.
        for (table, &size) in BITALLOC_SIZES.iter().enumerate() {
            for sel in 0..usize::from(QUANT_INDEX_GROUP_SIZE[table]) {
                let start = usize::from(CODES.quant_start[table][sel]);
                let kraft: f64 = CODES.quant[start..start + usize::from(size)]
                    .iter()
                    .map(|&(_, len)| 0.5f64.powi(i32::from(len)))
                    .sum();
                assert_eq!(kraft, 1.0, "book {table} set {sel}");
            }
        }
        for book in &CODES.bitalloc {
            let kraft: f64 = book[1..]
                .iter()
                .map(|&(_, len)| 0.5f64.powi(i32::from(len)))
                .sum();
            assert_eq!(kraft, 1.0);
        }
    }

    #[test]
    fn a_reference_frame_has_the_reference_shape() {
        let mut encoder = Encoder::new(Config::reference_five_one()).unwrap();
        assert_eq!(encoder.frame_size(), 2012);
        let header = encoder.header();
        assert_eq!(header.bit_rate_code, 24);
        assert_eq!(BIT_RATES[usize::from(header.bit_rate_code)], 1_536_000);
        let pcm = tone(4, 6);
        let mut out = Vec::new();
        for frame in pcm.chunks_exact(FRAME_SAMPLES * 6) {
            encoder.encode(frame, &mut out);
        }
        assert_eq!(out.len(), 4 * 2012);
        for frame in out.chunks_exact(2012) {
            assert_eq!(CoreHeader::read(frame).unwrap(), header);
        }
    }

    #[test]
    fn every_layout_and_rate_fits_its_frame() {
        for layout in [
            Layout::Mono,
            Layout::Stereo,
            Layout::TwoTwo,
            Layout::FivePointZero,
            Layout::FivePointOne,
        ] {
            for (bit_rate, adpcm) in [(768_000, false), (1_509_000, true), (3_840_000, false)] {
                let mut encoder = Encoder::new(Config {
                    layout,
                    bit_rate,
                    adpcm,
                    ..Config::reference_five_one()
                })
                .unwrap();
                let channels = layout.channels();
                let mut out = Vec::new();
                for frame in tone(3, channels).chunks_exact(FRAME_SAMPLES * channels) {
                    encoder.encode(frame, &mut out);
                }
                assert_eq!(out.len(), 3 * encoder.frame_size());
            }
        }
    }

    #[test]
    fn a_rate_too_low_for_the_channels_is_refused() {
        let config = Config {
            bit_rate: 192_000,
            ..Config::reference_five_one()
        };
        assert!(Encoder::new(config).is_err());
        let config = Config {
            sample_rate: 96000,
            ..Config::reference_five_one()
        };
        assert!(Encoder::new(config).is_err());
    }

    #[test]
    fn only_a_defined_word_length_is_written() {
        for code in 0..=8u8 {
            let config = Config {
                pcm_resolution_code: code,
                ..Config::reference_five_one()
            };
            match Encoder::new(config) {
                Ok(mut encoder) => {
                    assert!(matches!(code, 0..=3 | 5 | 6), "code {code} accepted");
                    let mut out = Vec::new();
                    encoder.encode(&[0; FRAME_SAMPLES * 6], &mut out);
                    let header = CoreHeader::read(&out).unwrap();
                    assert_eq!(header.pcm_resolution_code, code);
                }
                Err(_) => assert!(matches!(code, 4 | 7 | 8), "code {code} refused"),
            }
        }
    }

    /// The dialnorm is the header's revision and code as table 5-20 pairs
    /// them: 0 to 15 dB under revision 7, 16 to 31 under revision 6; nothing
    /// else in the frame changes.
    #[test]
    fn dialnorm_is_stated_in_the_header() {
        let silence = [0; FRAME_SAMPLES * 6];
        let frame = |dialnorm| {
            let mut encoder = Encoder::new(Config {
                dialnorm,
                ..Config::reference_five_one()
            })?;
            let mut out = Vec::new();
            encoder.encode(&silence, &mut out);
            Ok::<_, Unsupported>(out)
        };
        let none = frame(0).unwrap();
        for (dialnorm, revision, code) in
            [(0, 7, 0), (1, 7, 1), (15, 7, 15), (16, 6, 0), (31, 6, 15)]
        {
            let out = frame(dialnorm).unwrap();
            let header = CoreHeader::read(&out).unwrap();
            assert_eq!(
                (header.encoder_revision, header.dialnorm_code),
                (revision, code),
                "{dialnorm} dB"
            );
            let len = header.len();
            assert_eq!(out[len..], none[len..], "{dialnorm} dB");
        }
        assert!(frame(32).is_err());
    }

    /// Rate code 3 is 96 kb/s, not lossless: the steps are the lossy ones,
    /// which is what a decoder uses there.
    #[test]
    fn rate_code_3_dequantises_with_the_lossy_steps() {
        let encoder = Encoder::new(Config {
            layout: Layout::Mono,
            sample_rate: 16000,
            bit_rate: 96_000,
            adpcm: true,
            ..Config::reference_five_one()
        })
        .unwrap();
        assert_eq!(encoder.header().bit_rate_code, 3);
        let mut state = encoder.state;
        for abits in 1..27 {
            state.abits[0][0] = abits;
            assert_eq!(state.step_size(0, 0), LOSSY_QUANT[abits as usize] as i32);
        }
    }
}
