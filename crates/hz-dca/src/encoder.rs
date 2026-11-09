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
//! The loops are not FFmpeg's: they make the same values faster. A level's
//! centibels are looked up by bucket rather than searched for; the masking
//! model's chains of centibel sums run side by side, each in its own order,
//! leaving out the steps that cannot change what they add to; the integer
//! products the analysis sums in wrapping arithmetic, which any order sums
//! alike, are made from the unsigned product a vector unit has; and each
//! pass of the SNR search requantises only the subbands it moves, which
//! depend on nothing else within a frame. The tests keep the loops as they
//! were ported, to compare against.
//!
//! A frame's analysis — the filter bank, the LFE's decimation, the masking
//! curve and the peaks — depends on nothing but the input, this frame's and
//! the last's, so one encoder can make it ahead of another that allocates
//! and writes (`Encoder::analyse`, `Encoder::encode_analysed`); and with
//! ADPCM, the predictor search can run on threads beside the encoder's own
//! ([`Encoder::with_search_threads`]).
//!
//! Channels are indexed as FFmpeg indexes them: coded channels in their coded
//! order (centre, left, right, left surround, right surround for 5.1), then
//! the LFE. The input is interleaved 32-bit PCM in FFmpeg's — and WAV's —
//! order: L, R, C, LFE, Ls, Rs.
//!
//! Nothing allocates per frame but the queues to the search threads. The
//! state is set up once: about 210 kB, and 160 kB more for the ADPCM
//! predictor search when it is on, which the search threads share.

use crate::adpcm::{self, Adpcm, Found, Searchers, mul32, mul32_unsigned, norm, quantize_value};
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
use std::sync::Arc;

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
/// The masking model's absolute threshold and its offset, in centibels.
const CA_CB: i32 = -1114;
const CS_CB: i32 = 928;

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

/// Per allocation code 1 to 10, where its values start in `lengths`; per
/// value, from the lowest, the length of its code in each of the code's
/// Huffman codebooks, sixteen bits each, the first four in the first word:
/// adding them up adds up every codebook's bits at once, since no subband's
/// sixteen samples come to 2^16 bits in one.
struct HuffmanLengths {
    start: [u16; CODE_BOOKS],
    lengths: [[u64; 2]; HUFFMAN_VALUES],
}

const HUFFMAN_VALUES: usize = {
    let mut values = 0;
    let mut i = 0;
    while i < CODE_BOOKS {
        values += BITALLOC_SIZES[i] as usize;
        i += 1;
    }
    values
};

const HUFFMAN_LENGTHS: HuffmanLengths = {
    let mut packed = HuffmanLengths {
        start: [0; CODE_BOOKS],
        lengths: [[0; 2]; HUFFMAN_VALUES],
    };
    let mut at = 0;
    let mut table = 0;
    while table < CODE_BOOKS {
        packed.start[table] = at as u16;
        let mut value = 0;
        while value < BITALLOC_SIZES[table] as usize {
            let mut sel = 0;
            while sel < QUANT_INDEX_GROUP_SIZE[table] as usize {
                let len = CODES.quant[CODES.quant_start[table][sel] as usize + value].1;
                packed.lengths[at + value][sel / 4] |= (len as u64) << (16 * (sel % 4));
                sel += 1;
            }
            value += 1;
        }
        at += BITALLOC_SIZES[table] as usize;
        table += 1;
    }
    packed
};

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
    adpcm: Option<Arc<Adpcm>>,
    /// Threads that search for predictors beside this one, when there are
    /// any ([`Encoder::with_search_threads`]).
    searchers: Option<Searchers>,
    writer: BitWriter,
}

/// Everything per stream and per frame, in one allocation.
#[cfg_attr(test, derive(Clone))]
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
    /// The allocation each subband was last quantised at in this frame, −1
    /// for none yet: the SNR search tries one offset after another, most of
    /// which leave most subbands where they were, and a subband's scale
    /// factor, samples and bits depend on nothing else within a frame.
    quantized_at: [[i32; SUBBANDS]; MAX_CHANNELS],
    /// What those samples cost in each Huffman codebook of their allocation.
    huffman_bits: [[[u16; 7]; SUBBANDS]; MAX_CHANNELS],
    /// `calc_one_scale`'s scale factor by peak and allocation, once it has
    /// been asked for: [`NO_SCALE`] until then.
    scales: [u8; 2048 * 32],
    /// The frame's input per coded channel, after the last frame's, then
    /// the LFE's after its history.
    window: [[i32; 2 * FRAME_SAMPLES]; MAX_CHANNELS],

    cos_table: [i32; 2048],
    band_interpolation: [i32; 512],
    /// The cosine modulation of the analysis, by folded tap and subband.
    cos_mod: [[i32; SUBBANDS]; 32],
    band_spectrum: [i32; 8],
    /// The auditory filters' response, outer and middle ear included, at
    /// the stream's sample rate: centibels by filter and MDCT bin.
    auf: [[i32; 256]; AUBANDS],
    /// Per [`DENOMINATOR_LANES`] filters, the bins where one of them can
    /// raise its denominator, as a range: elsewhere none can, whatever the
    /// signal.
    denominator_bins: [(u16, u16); AUBANDS / DENOMINATOR_LANES],
    /// Per [`CURVE_LANES`] bins, the filters that can raise the
    /// unnormalised masking curve at one of them, as a range.
    curve_filters: [(u8, u8); 256 / CURVE_LANES],
    /// The MDCT's window.
    mdct_window: [i32; 512],
    cb_to_add: [i32; 256],
    /// `cb_to_add` by the signed difference of two levels, from −256 to
    /// 256: nought at either end, where the difference is 256 or more.
    add_by_difference: [i32; 513],
    /// The least difference from which `cb_to_add` is nought: an `add_cb`
    /// of levels that far apart adds nothing to the louder.
    silent_gap: i32,
    /// `cb_to_level`, then noughts that no level of one or more reaches.
    cb_to_level: [i32; 2048 + LEVEL_SPAN],
    /// For `get_cb`: per bucket of levels, the centibels of its loudest.
    level_buckets: [u16; 32 << LEVEL_BUCKET_BITS],
    lfe_fir_64i: [i32; 512],
}

/// A scale factor not yet known: they run from 0 to 124.
const NO_SCALE: u8 = u8::MAX;

/// Filters whose denominators `adjust_jnd` takes along the bins together.
const DENOMINATOR_LANES: usize = 5;
/// Bins `adjust_jnd` looks at together for a run that leaves the
/// denominators as they are.
const DENOMINATOR_RUN: usize = 16;
/// Bins whose unnormalised curve `adjust_jnd` takes along the filters
/// together.
const CURVE_LANES: usize = 8;

/// The bits after its leading one that put a level in its bucket for
/// `get_cb`: a bucket spans 1/64 of an octave, a fraction of a centibel.
const LEVEL_BUCKET_BITS: u32 = 6;
/// The most centibels a bucket spans, past its loudest level's.
const LEVEL_SPAN: usize = 2;
/// A bucket that spans more, searched as `get_cb` searches.
const LEVEL_SEARCH: u16 = 0x8000;

/// `get_cb`'s search: the most centibels below full scale whose level is
/// still `input`'s or above, in a table of levels that falls with the index.
fn search_level(cb_to_level: &[i32], input: i32) -> usize {
    let mut res = 0usize;
    let mut i = 1024;
    while i > 0 {
        if cb_to_level[i + res] >= input {
            res += i;
        }
        i >>= 1;
    }
    res
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
            quantized_at: [[-1; SUBBANDS]; MAX_CHANNELS],
            huffman_bits: [[[0; 7]; SUBBANDS]; MAX_CHANNELS],
            scales: [NO_SCALE; 2048 * 32],
            window: [[0; 2 * FRAME_SAMPLES]; MAX_CHANNELS],
            cos_table: [0; 2048],
            band_interpolation: [0; 512],
            cos_mod: [[0; SUBBANDS]; 32],
            band_spectrum: [0; 8],
            auf: [[0; 256]; AUBANDS],
            denominator_bins: [(0, 0); AUBANDS / DENOMINATOR_LANES],
            curve_filters: [(0, 0); 256 / CURVE_LANES],
            mdct_window: [0; 512],
            cb_to_add: [0; 256],
            add_by_difference: [0; 513],
            silent_gap: 256,
            cb_to_level: [0; 2048 + LEVEL_SPAN],
            level_buckets: [0; 32 << LEVEL_BUCKET_BITS],
            lfe_fir_64i: [0; 512],
        });
        state.init_tables(config.sample_rate);

        Ok(Self {
            adpcm: config.adpcm.then(|| Arc::new(Adpcm::new())),
            searchers: None,
            config,
            state,
            mdct: Mdct::new(),
            writer: BitWriter::with_capacity(MAX_FRAME_SIZE as usize),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Search for the subbands' predictors on up to `threads` threads
    /// besides the caller's, when the encoder codes ADPCM: the search is
    /// most of its work then, and a subband's depends on nothing but that
    /// subband. The stream is the same to the byte; threads that cannot be
    /// started leave the search where it was.
    pub fn with_search_threads(mut self, threads: usize) -> Self {
        if self.searchers.is_none() && threads > 0 {
            if let Some(adpcm) = &self.adpcm {
                self.searchers = Searchers::new(adpcm, threads);
            }
        }
        self
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
        self.analyse_here(pcm);
        self.allocate_and_write(out);
    }

    /// The first half of [`Encoder::encode`], which depends on nothing but
    /// the input — this frame's and the last's: the subband analysis, the
    /// LFE decimated, the masking curve and the peaks, into `analysis`. An
    /// encoder that is handed them ([`Encoder::encode_analysed`]) writes the
    /// frame this one would have; this one keeps the input as its history.
    ///
    /// # Panics
    /// If `pcm` is not exactly one frame.
    pub(crate) fn analyse(&mut self, pcm: &[i32], analysis: &mut Analysis) {
        self.analyse_here(pcm);
        let s = &*self.state;
        for (into, from) in analysis.subband.iter_mut().zip(&s.subband) {
            for (into, from) in into.iter_mut().zip(from) {
                into.copy_from_slice(&from[ADPCM_COEFFS..]);
            }
        }
        analysis.downsampled_lfe = s.downsampled_lfe;
        analysis.band_masking_cb = s.band_masking_cb;
        analysis.peak_cb = s.peak_cb;
        analysis.lfe_peak_cb = s.lfe_peak_cb;
    }

    /// The second half of [`Encoder::encode`], from an analysis of the
    /// frame that another encoder of the same configuration made
    /// ([`Encoder::analyse`]): the predictors, the allocation and the frame.
    /// This one's history is left as it was, unused.
    pub(crate) fn encode_analysed(&mut self, analysis: &Analysis, out: &mut Vec<u8>) {
        let s = &mut *self.state;
        for (into, from) in s.subband.iter_mut().zip(&analysis.subband) {
            for (into, from) in into.iter_mut().zip(from) {
                into[ADPCM_COEFFS..].copy_from_slice(from);
            }
        }
        s.downsampled_lfe = analysis.downsampled_lfe;
        s.band_masking_cb = analysis.band_masking_cb;
        s.peak_cb = analysis.peak_cb;
        s.lfe_peak_cb = analysis.lfe_peak_cb;
        self.allocate_and_write(out);
    }

    fn analyse_here(&mut self, pcm: &[i32]) {
        assert_eq!(
            pcm.len(),
            FRAME_SAMPLES * self.state.channels,
            "a frame is {FRAME_SAMPLES} samples of each channel"
        );
        let s = &mut *self.state;
        s.fill_window(pcm);
        s.subband_transform();
        if s.lfe_channel {
            s.lfe_downsample();
        }
        s.calc_masking(&mut self.mdct);
        s.find_peaks();
        s.shift_history(pcm, self.config.lfe_history_quirk);
    }

    fn allocate_and_write(&mut self, out: &mut Vec<u8>) {
        let s = &mut *self.state;
        if let Some(adpcm) = &self.adpcm {
            s.adpcm_analysis(adpcm, self.searchers.as_mut());
        }
        s.assign_bits();
        if s.lfe_channel {
            let (scale, quant) = s.scale(s.lfe_peak_cb, 11);
            s.lfe_scale_factor = scale;
            s.lfe_quant = quant;
            for (code, &sample) in s.lfe_codes.iter_mut().zip(&s.downsampled_lfe) {
                *code = quantize_value(sample, quant);
            }
        }
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

/// What [`Encoder::analyse`] makes of a frame for [`Encoder::encode_analysed`].
#[derive(Clone)]
pub(crate) struct Analysis {
    subband: [[[i32; SUBBAND_SAMPLES]; SUBBANDS]; MAX_CHANNELS],
    downsampled_lfe: [i32; LFE_SAMPLES],
    band_masking_cb: [i32; 32],
    peak_cb: [[i32; SUBBANDS]; MAX_CHANNELS],
    lfe_peak_cb: i32,
}

impl Default for Analysis {
    fn default() -> Self {
        Self {
            subband: [[[0; SUBBAND_SAMPLES]; SUBBANDS]; MAX_CHANNELS],
            downsampled_lfe: [0; LFE_SAMPLES],
            band_masking_cb: [0; 32],
            peak_cb: [[0; SUBBANDS]; MAX_CHANNELS],
            lfe_peak_cb: 0,
        }
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

        for (i, level) in self.cb_to_level[..2048].iter_mut().enumerate() {
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

        self.init_lookups();
    }

    /// What the hot loops look up instead of computing, made from the tables
    /// above with integers alone.
    fn init_lookups(&mut self) {
        for i in 16..48 {
            for band in 0..SUBBANDS {
                self.cos_mod[i - 16][band] = self.cos_t(((2 * band + 1) * (2 * (i + 16) + 1)) << 3);
            }
        }
        for i in 0..512 {
            self.mdct_window[i] = 0x3fff_ffff - (self.cos_t(4 * i + 2) >> 1);
        }

        // The denominators start at `CA_CB` and the unnormalised curve at
        // −2047, and an `add_cb` never lowers what it adds to: with no
        // `cb_to_add` below nought, a level `silent_gap` (164) cB or more
        // under either floor leaves it as it is. A bin's power is at most
        // `add_cb(0, 0)`; the curve adds `a − denominator`, at most
        // `a − CA_CB`.
        self.silent_gap = self
            .cb_to_add
            .iter()
            .rposition(|&a| a != 0)
            .map_or(0, |last| last as i32 + 1);
        for (at, add) in self.add_by_difference.iter_mut().enumerate() {
            *add = self.cb_to_add.get(at.abs_diff(256)).copied().unwrap_or(0);
        }
        let floors = self.cb_to_add.iter().all(|&a| a >= 0);
        let most_power = self.cb_to_add[0];
        let gap = self.silent_gap;
        let raises_denominator = |a: i32| !floors || most_power + a > CA_CB - gap;
        let raises_curve = |a: i32| !floors || a - CA_CB > -2047 - gap;
        // The hull of the indices where `raises` holds, empty when nowhere.
        fn hull(raises: impl Fn(usize) -> bool, len: usize) -> (usize, usize) {
            let first = (0..len).find(|&i| raises(i));
            let last = (0..len).rev().find(|&i| raises(i));
            match (first, last) {
                (Some(first), Some(last)) => (first, last + 1),
                _ => (0, 0),
            }
        }
        for (block, bins) in self.denominator_bins.iter_mut().enumerate() {
            let filters = &self.auf[block * DENOMINATOR_LANES..][..DENOMINATOR_LANES];
            let (first, end) = hull(
                |bin| filters.iter().any(|auf| raises_denominator(auf[bin])),
                256,
            );
            *bins = (first as u16, end as u16);
        }
        for (block, filters) in self.curve_filters.iter_mut().enumerate() {
            let bins = block * CURVE_LANES..(block + 1) * CURVE_LANES;
            let (first, end) = hull(
                |j| self.auf[j][bins.clone()].iter().any(|&a| raises_curve(a)),
                AUBANDS,
            );
            *filters = (first as u8, end as u8);
        }

        // `get_cb` by bucket: a level's bucket is its leading zeros and the
        // six bits after its leading one. With the levels falling, the
        // answer for any level in a bucket lies between the answers for its
        // loudest and its quietest; where those are at most `LEVEL_SPAN`
        // apart, comparing against the levels just past the loudest's finds
        // it. A bucket that spans more is searched.
        let levels = &self.cb_to_level[..2048];
        let falling = levels.windows(2).all(|w| w[0] >= w[1]);
        for zeros in 1..32u32 {
            for sub in 0..1u64 << LEVEL_BUCKET_BITS {
                let top = ((1 << LEVEL_BUCKET_BITS) | sub) << (31 - LEVEL_BUCKET_BITS);
                let quietest = top.div_ceil(1 << zeros);
                let loudest = (top + (1 << (31 - LEVEL_BUCKET_BITS)) - 1) >> zeros;
                let bucket =
                    &mut self.level_buckets[(zeros as usize) << LEVEL_BUCKET_BITS | sub as usize];
                if quietest > loudest {
                    // No level falls in it.
                    continue;
                }
                let from = search_level(levels, loudest as i32);
                let to = search_level(levels, quietest as i32);
                *bucket = if falling && to - from <= LEVEL_SPAN {
                    from as u16
                } else {
                    LEVEL_SEARCH
                };
            }
        }
    }

    #[inline(always)]
    fn cos_t(&self, x: usize) -> i32 {
        self.cos_table[x & 2047]
    }

    /// Each coded channel's input, and the LFE's, after its history: what
    /// the analysis and the masking model see.
    fn fill_window(&mut self, input: &[i32]) {
        let channels = self.channels;
        for ch in 0..self.fullband_channels {
            let chi = self.channel_order[ch] as usize;
            let (history, frame) = self.window[ch].split_at_mut(FRAME_SAMPLES);
            history.copy_from_slice(&self.history[ch]);
            for (w, samples) in frame.iter_mut().zip(input.chunks_exact(channels)) {
                *w = samples[chi];
            }
        }
        if self.lfe_channel {
            let lfech = self.lfe_index;
            let (history, frame) = self.window[channels - 1].split_at_mut(FRAME_SAMPLES);
            history.copy_from_slice(&self.history[channels - 1]);
            for (w, samples) in frame.iter_mut().zip(input.chunks_exact(channels)) {
                *w = samples[lfech];
            }
        }
    }

    /// The 32-band polyphase analysis of each coded channel.
    ///
    /// Subband sample `subs` is the 512 taps from input sample `32 · subs`
    /// on, counting the last frame's: the window, then the cosine
    /// modulation, of products summed in wrapping arithmetic, which any
    /// order sums alike.
    fn subband_transform(&mut self) {
        for ch in 0..self.fullband_channels {
            for subs in 0..SUBBAND_SAMPLES {
                let taps = &self.window[ch][32 * subs..32 * subs + 512];
                // All the convolutions at once.
                let mut accum = [0i32; 64];
                for (taps, coeffs) in taps
                    .chunks_exact(64)
                    .zip(self.band_interpolation.chunks_exact(64))
                {
                    for ((a, &x), &c) in accum.iter_mut().zip(taps).zip(coeffs) {
                        *a = a.wrapping_add(mul32_unsigned(x, c));
                    }
                }
                for k in 16..32 {
                    accum[k] = accum[k].wrapping_sub(accum[31 - k]);
                }
                for k in 32..48 {
                    accum[k] = accum[k].wrapping_add(accum[95 - k]);
                }
                let mut resp = [0i32; SUBBANDS];
                for (&a, cos) in accum[16..48].iter().zip(&self.cos_mod) {
                    for (r, &c) in resp.iter_mut().zip(cos) {
                        *r = r.wrapping_add(mul32_unsigned(a, c) >> 3);
                    }
                }
                for (band, &resp) in resp.iter().enumerate() {
                    self.subband[ch][band][ADPCM_COEFFS + subs] = if (band + 1) & 2 != 0 {
                        resp.wrapping_neg()
                    } else {
                        resp
                    };
                }
            }
        }
    }

    /// The LFE decimated 64 times, through the interpolator's filter.
    fn lfe_downsample(&mut self) {
        let window = &self.window[self.channels - 1];
        for (lfes, out) in self.downsampled_lfe.iter_mut().enumerate() {
            let taps = &window[64 * lfes..64 * lfes + 512];
            let mut accum = [0i32; 8];
            for (taps, coeffs) in taps.chunks_exact(8).zip(self.lfe_fir_64i.chunks_exact(8)) {
                for ((a, &x), &c) in accum.iter_mut().zip(taps).zip(coeffs) {
                    *a = a.wrapping_add(mul32(x, c));
                }
            }
            *out = accum.iter().fold(0i32, |s, &a| s.wrapping_add(a));
        }
    }

    /// `get_cb`: a level as negative centibels below full scale.
    ///
    /// Looked up by bucket ([`State::init_lookups`]): the same answer as
    /// the search over `cb_to_level`, without its eleven unpredictable
    /// branches.
    #[inline(always)]
    fn get_cb(&self, input: i32) -> i32 {
        let input = input.wrapping_abs();
        // Nought, and the −2^31 whose absolute value wraps, are under every
        // level.
        if (input as u32).wrapping_sub(1) >= 0x7fff_ffff {
            return -2047;
        }
        let zeros = input.leading_zeros();
        let sub = ((input as u32) << zeros >> (31 - LEVEL_BUCKET_BITS)) as usize
            & ((1 << LEVEL_BUCKET_BITS) - 1);
        let bucket = self.level_buckets[(zeros as usize) << LEVEL_BUCKET_BITS | sub];
        if bucket == LEVEL_SEARCH {
            return -(search_level(&self.cb_to_level, input) as i32);
        }
        let from = usize::from(bucket);
        let res = from
            + usize::from(self.cb_to_level[from + 1] >= input)
            + usize::from(self.cb_to_level[from + 2] >= input);
        -(res as i32)
    }

    /// `add_cb`: the sum of two levels in centibels — the louder, and
    /// `cb_to_add` of how much quieter the other is, nought from 256 cB.
    /// The levels it sums are within a few thousand centibels of nought, so
    /// their difference is exact.
    #[inline(always)]
    fn add_cb(&self, a: i32, b: i32) -> i32 {
        let at = ((a - b + 256) as u32).min(512) as usize;
        a.max(b) + self.add_by_difference[at]
    }

    fn calc_power(&self, mdct: &mut Mdct, input: &[i32], power: &mut [i32; 256]) {
        let mut data = [0i32; 2 * mdct::LEN];
        for ((d, &x), &window) in data.iter_mut().zip(input).zip(&self.mdct_window) {
            *d = norm(i64::from(mul32(x, window)), 4);
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
    ///
    /// Each filter's denominator is a chain of `add_cb`s along the bins, so
    /// they run [`DENOMINATOR_LANES`] filters at a time, side by side; the
    /// unnormalised curve is a chain along the filters at each bin, so it
    /// runs [`CURVE_LANES`] bins at a time. Every chain keeps its order. The
    /// steps left out are those that cannot change what they add to
    /// ([`State::init_lookups`]).
    fn adjust_jnd(&self, mdct: &mut Mdct, input: &[i32], out_cb: &mut [i32; 256]) {
        let mut power = [0i32; 256];
        self.calc_power(mdct, input, &mut power);
        self.jnd_from_power(&power, out_cb);
    }

    /// The rest of `adjust_jnd`, from the block's power by bin.
    fn jnd_from_power(&self, power: &[i32; 256], out_cb: &mut [i32; 256]) {
        let mut denom = [CA_CB; AUBANDS];
        for ((denom, filters), &(first, end)) in denom
            .chunks_exact_mut(DENOMINATOR_LANES)
            .zip(self.auf.chunks_exact(DENOMINATOR_LANES))
            .zip(&self.denominator_bins)
        {
            let (first, end) = (usize::from(first), usize::from(end));
            let mut d = [CA_CB; DENOMINATOR_LANES];
            // A run of bins whose levels are all `silent_gap` cB or more
            // under a denominator leaves it as it is: an `add_cb` adds
            // nought to the louder there, and the denominator only grows.
            let mut bin = first;
            while bin < end {
                let n = (end - bin).min(DENOMINATOR_RUN);
                let power = &power[bin..bin + n];
                let mut levels = [[0i32; DENOMINATOR_RUN]; DENOMINATOR_LANES];
                let mut moves = false;
                for ((levels, auf), &d) in levels.iter_mut().zip(filters).zip(&d) {
                    let mut loudest = i32::MIN;
                    for ((level, &p), &a) in levels.iter_mut().zip(power).zip(&auf[bin..bin + n]) {
                        *level = p + a;
                        loudest = loudest.max(*level);
                    }
                    moves |= loudest > d - self.silent_gap;
                }
                if moves {
                    for i in 0..n {
                        for (d, levels) in d.iter_mut().zip(&levels) {
                            *d = self.add_cb(*d, levels[i]);
                        }
                    }
                }
                bin += n;
            }
            denom.copy_from_slice(&d);
        }
        let mut out_cb_unnorm = [-2047i32; 256];
        for (block, (unnorm, &(first, end))) in out_cb_unnorm
            .chunks_exact_mut(CURVE_LANES)
            .zip(&self.curve_filters)
            .enumerate()
        {
            let (first, end) = (usize::from(first), usize::from(end));
            // Can only grow.
            let mut u = [-2047i32; CURVE_LANES];
            for (auf, &denom) in self.auf[first..end].iter().zip(&denom[first..end]) {
                let auf = &auf[block * CURVE_LANES..][..CURVE_LANES];
                for (u, &a) in u.iter_mut().zip(auf) {
                    *u = self.add_cb(*u, -denom + a);
                }
            }
            unnorm.copy_from_slice(&u);
        }
        for (o, &u) in out_cb.iter_mut().zip(&out_cb_unnorm) {
            *o = self.add_cb(*o, -u - CA_CB - CS_CB);
        }
    }

    fn calc_masking(&mut self, mdct: &mut Mdct) {
        let mut masking = [[-2047i32; 256]; SUBSUBFRAMES];
        for (ssf, curve) in masking.iter_mut().enumerate() {
            for ch in 0..self.fullband_channels {
                let first = 128 + 256 * ssf;
                self.adjust_jnd(mdct, &self.window[ch][first..first + 512], curve);
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

    /// The predictor of every subband, here or on `searchers`.
    fn adpcm_analysis(&mut self, adpcm: &Adpcm, searchers: Option<&mut Searchers>) {
        self.consumed_adpcm_bits = 0;
        let bands = self.fullband_channels * SUBBANDS;
        let mut found: [Found; MAX_CHANNELS * SUBBANDS] =
            [(None, [0; SUBBAND_SAMPLES]); MAX_CHANNELS * SUBBANDS];
        let found = &mut found[..bands];
        let subbands = self.subband.iter().flatten().take(bands);
        match searchers {
            Some(searchers) => {
                let mut inputs = [[0; ADPCM_COEFFS + SUBBAND_SAMPLES]; MAX_CHANNELS * SUBBANDS];
                for (input, subband) in inputs.iter_mut().zip(subbands) {
                    *input = *subband;
                }
                searchers.search(&inputs[..bands], found);
            }
            None => {
                for ((vq, diff), subband) in found.iter_mut().zip(subbands) {
                    *vq = adpcm.subband_analysis(subband, diff);
                }
            }
        }
        for (i, (vq, diff)) in found.iter().enumerate() {
            let (ch, band) = (i / SUBBANDS, i % SUBBANDS);
            match *vq {
                Some(vq) => {
                    self.prediction_mode[ch][band] = i32::from(vq);
                    // Twelve bits for the predictor's index.
                    self.consumed_adpcm_bits += 12;
                    self.diff_peak_cb[ch][band] = self.find_peak(diff);
                }
                None => self.prediction_mode[ch][band] = -1,
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

    /// [`State::calc_one_scale`], which depends on nothing but its
    /// arguments: its search is made once for each, and the multiplier
    /// made again from the scale factor as it makes it.
    fn scale(&mut self, peak_cb: i32, abits: i32) -> (i32, SoftFloat) {
        debug_assert!((0..32).contains(&abits));
        let at = (-peak_cb) as usize * 32 + abits as usize;
        if self.scales[at] == NO_SCALE {
            self.scales[at] = self.calc_one_scale(peak_cb, abits).0 as u8;
        }
        let nscale = usize::from(self.scales[at]);
        let abits = abits as usize;
        let quant = SoftFloat {
            m: mul32(SCALEFACTOR_INV[nscale].m, STEPSIZE_INV[abits].m),
            e: SCALEFACTOR_INV[nscale].e + STEPSIZE_INV[abits].e - 17,
        };
        (nscale as i32, quant)
    }

    /// Scale and quantise subband `band` of coded channel `ch` at its
    /// allocation — `quantize_adpcm` or `quantize_pcm` with the scale factor
    /// before it — and count what its samples cost in each Huffman codebook
    /// of the allocation.
    fn quantize_band(&mut self, ch: usize, band: usize) {
        let abits = self.abits[ch][band];
        let vq = self.prediction_mode[ch][band];
        if vq == -1 {
            let (scale, quant) = self.scale(self.peak_cb[ch][band], abits);
            self.scale_factor[ch][band] = scale;
            self.quant[ch][band] = quant;
            for (q, &s) in self.quantized[ch][band]
                .iter_mut()
                .zip(&self.subband[ch][band][ADPCM_COEFFS..])
            {
                *q = quantize_value(s, quant);
            }
        } else if vq >= 0 {
            let diff_peak_cb = self.diff_peak_cb[ch][band];
            let (scale, quant) = self.scale(diff_peak_cb, abits);
            self.scale_factor[ch][band] = scale;
            self.quant[ch][band] = quant;
            let step_size = self.step_size(ch, band);
            let peak = self.cb_to_level[(-diff_peak_cb) as usize];
            let history = &mut self.adpcm_history[ch][band];
            let prev: [i32; ADPCM_COEFFS] = history[..ADPCM_COEFFS].try_into().unwrap();
            let next: &mut [i32; ADPCM_COEFFS] = (&mut history[ADPCM_COEFFS..]).try_into().unwrap();
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
        if abits != 0 && abits as usize <= CODE_BOOKS {
            let id = abits as usize - 1;
            let group = usize::from(QUANT_INDEX_GROUP_SIZE[id]);
            let quantized = &self.quantized[ch][band];
            let bits = &mut self.huffman_bits[ch][band][..group];
            // The values from the lowest the codebooks have.
            let lowest = i32::from(BITALLOC_OFFSETS[id]);
            let size = u32::from(BITALLOC_SIZES[id]);
            if quantized
                .iter()
                .all(|&v| (v.wrapping_sub(lowest) as u32) < size)
            {
                let lengths = &HUFFMAN_LENGTHS.lengths[usize::from(HUFFMAN_LENGTHS.start[id])..];
                let mut sum = [0u64; 2];
                for &v in quantized {
                    let packed = lengths[(v - lowest) as usize];
                    sum[0] += packed[0];
                    sum[1] += packed[1];
                }
                for (sel, bits) in bits.iter_mut().enumerate() {
                    *bits = (sum[sel / 4] >> (16 * (sel % 4))) as u16;
                }
            } else {
                for (sel, bits) in bits.iter_mut().enumerate() {
                    *bits = quantized
                        .iter()
                        .map(|&v| u16::from(quant_code(id, sel, v).1))
                        .sum();
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

        // Scale factors again on every pass, to count the Huffman codes:
        // a subband whose allocation this pass leaves where the last pass of
        // the frame put it has them already.
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                let abits = self.abits[ch][band];
                if self.quantized_at[ch][band] != abits {
                    self.quantized_at[ch][band] = abits;
                    self.quantize_band(ch, band);
                }
            }
        }

        let mut huff_bits = [[[0u32; 7]; CODE_BOOKS]; MAX_CHANNELS];
        let mut clc_bits = [[0u32; CODE_BOOKS]; MAX_CHANNELS];
        let mut bits_counter = 0u32;
        for ch in 0..self.fullband_channels {
            for band in 0..SUBBANDS {
                let abits = self.abits[ch][band];
                if abits != 0 && abits as usize <= CODE_BOOKS {
                    let id = abits as usize - 1;
                    let group = usize::from(QUANT_INDEX_GROUP_SIZE[id]);
                    for (total, &bits) in huff_bits[ch][id][..group]
                        .iter_mut()
                        .zip(&self.huffman_bits[ch][band])
                    {
                        *total += u32::from(bits);
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
        // The frame's subbands, peaks and predictors are new.
        self.quantized_at = [[-1; SUBBANDS]; MAX_CHANNELS];
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

    /// Silence, then full-scale noise in bursts with a decaying tail, then
    /// a quiet tone: every allocation from nought to 26 codes, and frames
    /// whose SNR search walks a long way from the frame before's.
    fn bursts(frames: usize, channels: usize) -> Vec<i32> {
        let mut seed = 0x9e37_79b9u32;
        let mut pcm = Vec::with_capacity(frames * FRAME_SAMPLES * channels);
        for n in 0..frames * FRAME_SAMPLES {
            let frame = n / FRAME_SAMPLES;
            for ch in 0..channels {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed as i32) >> 8 << 8;
                let s = match frame % 24 {
                    0..=2 => 0,
                    3..=7 => noise,
                    8..=13 => noise >> (2 * (frame % 24 - 7) + ch),
                    14..=17 => {
                        let w = (n % (97 + 13 * ch)) as i32 - 48;
                        w << 20
                    }
                    _ => (noise >> 20) << (ch % 3),
                };
                pcm.push(s);
            }
        }
        pcm
    }

    /// Streams and their reconstructions as the encoder wrote them before it
    /// was made faster, hashed: every layout, rates from the lowest a layout
    /// takes to the highest, three sample rates, ADPCM on and off. The
    /// hashes are what this test's body makes run against the encoder and
    /// decoder of commit 072235e, before the change.
    #[test]
    fn the_stream_and_its_reconstruction_are_unchanged() {
        use crate::core_decode::CoreDecoder;
        let cases = [
            (
                Layout::FivePointOne,
                48000,
                1_509_000,
                false,
                0x607e_9710_e32b_ab17u64,
            ),
            (
                Layout::FivePointOne,
                48000,
                1_509_000,
                true,
                0x2b12_de8f_6e36_042f,
            ),
            (
                Layout::FivePointOne,
                48000,
                768_000,
                true,
                0x9863_bbd7_82e6_812f,
            ),
            (
                Layout::FivePointZero,
                44100,
                1_411_200,
                true,
                0xab4c_f433_2ba0_922e,
            ),
            (
                Layout::TwoTwo,
                48000,
                1_000_000,
                false,
                0x7bae_9fdb_f38a_c81d,
            ),
            (
                Layout::Stereo,
                48000,
                3_840_000,
                false,
                0x31a1_5d6f_38a2_ef0c,
            ),
            (Layout::Stereo, 32000, 256_000, true, 0x069e_dcfa_640d_67be),
            (Layout::Mono, 48000, 160_000, false, 0x960f_b9bd_1430_6794),
        ];
        for (case, &(layout, sample_rate, bit_rate, adpcm, want)) in cases.iter().enumerate() {
            let channels = layout.channels();
            let mut pcm = if layout == Layout::FivePointOne {
                fixture(10)
            } else {
                fixture(10)
                    .chunks_exact(6)
                    .flat_map(|s| s[..channels].to_vec())
                    .collect()
            };
            pcm.extend(bursts(30, channels));
            let mut encoder = Encoder::new(Config {
                layout,
                sample_rate,
                bit_rate,
                adpcm,
                ..Config::reference_five_one()
            })
            .unwrap();
            let mut decoder = CoreDecoder::new();
            let mut out = Vec::new();
            let mut hash = 0xcbf2_9ce4_8422_2325u64;
            for frame in pcm.chunks_exact(FRAME_SAMPLES * channels) {
                out.clear();
                encoder.encode(frame, &mut out);
                decoder.decode(&encoder);
                hash = fnv1a_from(hash, &out);
                for ch in 0..channels {
                    let samples = decoder.channel(ch);
                    hash = fnv1a_from(hash, &samples.map(|s| s.to_le_bytes()).concat());
                }
            }
            assert_eq!(hash, want, "case {case}");
        }
    }

    /// The frame an encoder writes from another's analysis is the one it
    /// writes analysing for itself, and so is the one an encoder searching
    /// for predictors on threads writes: every layout, ADPCM on and off,
    /// FFmpeg's LFE history too, across the first frame's restart.
    #[test]
    fn analysed_apart_and_searched_on_threads_the_frames_are_the_same() {
        for (layout, adpcm, bit_rate, lfe_history_quirk) in [
            (Layout::FivePointOne, false, 1_509_000, false),
            (Layout::FivePointOne, true, 768_000, true),
            (Layout::FivePointZero, true, 1_411_200, false),
            (Layout::Stereo, false, 3_840_000, false),
            (Layout::Mono, true, 160_000, false),
        ] {
            let config = Config {
                layout,
                adpcm,
                bit_rate,
                lfe_history_quirk,
                ..Config::reference_five_one()
            };
            let mut here = Encoder::new(config.clone()).unwrap();
            let mut analyser = Encoder::new(config.clone()).unwrap();
            let mut apart = Encoder::new(config.clone()).unwrap();
            let mut threads = Encoder::new(config).unwrap().with_search_threads(3);
            assert_eq!(threads.searchers.is_some(), adpcm);
            let channels = layout.channels();
            let mut analysis = Box::default();
            for (f, frame) in stage_input(24, channels)
                .chunks_exact(FRAME_SAMPLES * channels)
                .enumerate()
            {
                let mut want = Vec::new();
                here.encode(frame, &mut want);
                analyser.analyse(frame, &mut analysis);
                let mut got = Vec::new();
                apart.encode_analysed(&analysis, &mut got);
                assert_eq!(got, want, "{layout:?}, frame {f}: analysed apart");
                got.clear();
                threads.encode(frame, &mut got);
                assert_eq!(got, want, "{layout:?}, frame {f}: searched on threads");
                if f == 0 {
                    for encoder in [&mut here, &mut apart, &mut threads] {
                        encoder.restart_prediction();
                    }
                }
            }
        }
    }

    /// The encoder writes the frames it wrote before its stages were made
    /// faster and some of them moved: the analysis, the predictor search
    /// and the masking curve as they were, in the order they were in —
    /// every layout, ADPCM on and off, searched here and on threads.
    #[test]
    fn the_encoder_writes_what_it_wrote() {
        for (layout, adpcm, bit_rate, lfe_history_quirk) in [
            (Layout::FivePointOne, false, 1_509_000, false),
            (Layout::FivePointOne, true, 1_509_000, true),
            (Layout::FivePointZero, true, 768_000, false),
            (Layout::TwoTwo, false, 1_000_000, false),
            (Layout::Stereo, true, 3_840_000, false),
            (Layout::Mono, false, 160_000, false),
        ] {
            let config = Config {
                layout,
                adpcm,
                bit_rate,
                lfe_history_quirk,
                ..Config::reference_five_one()
            };
            let mut was = Encoder::new(config.clone()).unwrap();
            let mut here = Encoder::new(config.clone()).unwrap();
            let mut threads = Encoder::new(config).unwrap().with_search_threads(3);
            let channels = layout.channels();
            for (f, frame) in stage_input(24, channels)
                .chunks_exact(FRAME_SAMPLES * channels)
                .enumerate()
            {
                let mut want = Vec::new();
                before::encode(&mut was, frame, &mut want);
                for (encoder, how) in [(&mut here, "here"), (&mut threads, "on threads")] {
                    let mut got = Vec::new();
                    encoder.encode(frame, &mut got);
                    assert_eq!(got, want, "{layout:?}, frame {f}, searched {how}");
                }
                if f == 0 {
                    for encoder in [&mut was, &mut here, &mut threads] {
                        encoder.restart_prediction();
                    }
                }
            }
        }
    }

    fn fnv1a_from(hash: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(hash, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        })
    }

    /// The encoder as it was before it was made faster, kept to compare
    /// against: each of these is the code it replaced, as it was.
    mod before {
        use super::super::*;

        pub fn get_cb(s: &State, input: i32) -> i32 {
            let input = input.wrapping_abs();
            let mut res = 0usize;
            let mut i = 1024;
            while i > 0 {
                if s.cb_to_level[i + res] >= input {
                    res += i;
                }
                i >>= 1;
            }
            -(res as i32)
        }

        pub fn add_cb(s: &State, a: i32, b: i32) -> i32 {
            let (a, b) = if a < b { (b, a) } else { (a, b) };
            if a - b >= 256 {
                return a;
            }
            a + s.cb_to_add[(a - b) as usize]
        }

        fn calc_power(s: &State, mdct: &mut Mdct, input: &[i32; 512], power: &mut [i32; 256]) {
            let mut data = [0i32; 2 * mdct::LEN];
            for (i, d) in data.iter_mut().enumerate() {
                let window = 0x3fff_ffff - (s.cos_t(4 * i + 2) >> 1);
                *d = norm(i64::from(mul32(input[i], window)), 4);
            }
            let mut coeff = [0i32; mdct::LEN];
            mdct.forward(&data, &mut coeff);
            for (p, &c) in power.iter_mut().zip(&coeff) {
                let cb = get_cb(s, c);
                *p = add_cb(s, cb, cb);
            }
        }

        pub fn adjust_jnd(s: &State, mdct: &mut Mdct, input: &[i32; 512], out_cb: &mut [i32; 256]) {
            let mut power = [0i32; 256];
            calc_power(s, mdct, input, &mut power);
            jnd_from_power(s, &power, out_cb);
        }

        pub fn jnd_from_power(s: &State, power: &[i32; 256], out_cb: &mut [i32; 256]) {
            const CA_CB: i32 = -1114;
            const CS_CB: i32 = 928;
            // Can only grow.
            let mut out_cb_unnorm = [-2047i32; 256];
            for auf in &s.auf {
                let mut denom = CA_CB;
                for (&p, &a) in power.iter().zip(auf) {
                    denom = add_cb(s, denom, p + a);
                }
                for (u, &a) in out_cb_unnorm.iter_mut().zip(auf) {
                    *u = add_cb(s, *u, -denom + a);
                }
            }
            for (o, &u) in out_cb.iter_mut().zip(&out_cb_unnorm) {
                *o = add_cb(s, *o, -u - CA_CB - CS_CB);
            }
        }

        pub fn subband_transform(s: &mut State, input: &[i32]) {
            let channels = s.channels;
            for ch in 0..s.fullband_channels {
                // The history is copied because the masking model needs it too.
                let mut hist = s.history[ch];
                let mut hist_start = 0;
                let chi = s.channel_order[ch] as usize;
                for subs in 0..SUBBAND_SAMPLES {
                    // All the convolutions at once.
                    let mut accum = [0i32; 64];
                    let mut k = 0;
                    for (j, i) in (hist_start..512).chain(0..hist_start).enumerate() {
                        accum[k] = accum[k].wrapping_add(mul32(hist[i], s.band_interpolation[j]));
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
                            let x = (2 * band + 1) * (2 * (i + 16) + 1);
                            resp = resp.wrapping_add(mul32(a, s.cos_t(x << 3)) >> 3);
                        }
                        s.subband[ch][band][ADPCM_COEFFS + subs] = if (band + 1) & 2 != 0 {
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

        pub fn lfe_downsample(s: &mut State, input: &[i32]) {
            let channels = s.channels;
            let lfech = s.lfe_index;
            let mut hist = s.history[channels - 1];
            let mut hist_start = 0;
            for lfes in 0..LFE_SAMPLES {
                let mut accum = 0i32;
                for (j, i) in (hist_start..512).chain(0..hist_start).enumerate() {
                    accum = accum.wrapping_add(mul32(hist[i], s.lfe_fir_64i[j]));
                }
                s.downsampled_lfe[lfes] = accum;
                for i in 0..64 {
                    hist[i + hist_start] = input[(lfes * 64 + i) * channels + lfech];
                }
                hist_start = (hist_start + 64) & 511;
            }
        }

        fn quantize_adpcm(s: &mut State) {
            for ch in 0..s.fullband_channels {
                for band in 0..SUBBANDS {
                    let vq = s.prediction_mode[ch][band];
                    if vq < 0 {
                        continue;
                    }
                    let diff_peak_cb = s.diff_peak_cb[ch][band];
                    let (scale, quant) = s.calc_one_scale(diff_peak_cb, s.abits[ch][band]);
                    s.scale_factor[ch][band] = scale;
                    s.quant[ch][band] = quant;
                    let step_size = s.step_size(ch, band);
                    let peak = s.cb_to_level[(-diff_peak_cb) as usize];
                    let history = &mut s.adpcm_history[ch][band];
                    let prev: [i32; ADPCM_COEFFS] = history[..ADPCM_COEFFS].try_into().unwrap();
                    let next: &mut [i32; ADPCM_COEFFS] =
                        (&mut history[ADPCM_COEFFS..]).try_into().unwrap();
                    adpcm::code_subband(
                        vq as usize,
                        quant,
                        SCALE_FACTOR_QUANT7[scale as usize] as i32,
                        step_size,
                        &prev,
                        &s.subband[ch][band][ADPCM_COEFFS..],
                        next,
                        &mut s.quantized[ch][band],
                        peak,
                    );
                }
            }
        }

        fn quantize_pcm(s: &mut State) {
            for ch in 0..s.fullband_channels {
                for band in 0..SUBBANDS {
                    if s.prediction_mode[ch][band] != -1 {
                        continue;
                    }
                    let quant = s.quant[ch][band];
                    for (q, &x) in s.quantized[ch][band]
                        .iter_mut()
                        .zip(&s.subband[ch][band][ADPCM_COEFFS..])
                    {
                        *q = quantize_value(x, quant);
                    }
                }
            }
        }

        pub fn calc_masking(s: &mut State, mdct: &mut Mdct, input: &[i32]) {
            let mut masking = [[-2047i32; 256]; SUBSUBFRAMES];
            let mut data = [0i32; 512];
            for (ssf, curve) in masking.iter_mut().enumerate() {
                for ch in 0..s.fullband_channels {
                    let chi = s.channel_order[ch] as usize;
                    let first = 128 + 256 * ssf;
                    let from_history = 512 - first;
                    data[..from_history].copy_from_slice(&s.history[ch][first..]);
                    for (k, d) in data[from_history..].iter_mut().enumerate() {
                        *d = input[k * s.channels + chi];
                    }
                    adjust_jnd(s, mdct, &data, curve);
                }
            }
            s.masking_curve_cb = masking;
            for i in 0..256 {
                let mut m = 2048;
                for curve in &s.masking_curve_cb {
                    m = m.min(curve[i]);
                }
                s.eff_masking_curve_cb[i] = m;
            }

            for band in 0..32 {
                s.band_masking_cb[band] = 2048;
                let mut update = |f: usize, spectrum: i32| {
                    let value = s.eff_masking_curve_cb[f] - spectrum;
                    if value < s.band_masking_cb[band] {
                        s.band_masking_cb[band] = value;
                    }
                };
                if band == 0 {
                    for f in 0..4 {
                        update(f, 0);
                    }
                } else {
                    for f in 0..8 {
                        update(8 * band - 4 + f, s.band_spectrum[7 - f]);
                    }
                }
                if band == 31 {
                    for f in 0..4 {
                        update(256 - 4 + f, 0);
                    }
                } else {
                    for f in 0..8 {
                        update(8 * band + 4 + f, s.band_spectrum[f]);
                    }
                }
            }
        }

        pub fn adpcm_analysis(s: &mut State, adpcm: &Adpcm) {
            s.consumed_adpcm_bits = 0;
            let mut estimated_diff = [0i32; SUBBAND_SAMPLES];
            for ch in 0..s.fullband_channels {
                for band in 0..SUBBANDS {
                    match adpcm.subband_analysis(&s.subband[ch][band], &mut estimated_diff) {
                        Some(vq) => {
                            s.prediction_mode[ch][band] = i32::from(vq);
                            // Twelve bits for the predictor's index.
                            s.consumed_adpcm_bits += 12;
                            s.diff_peak_cb[ch][band] = s.find_peak(&estimated_diff);
                        }
                        None => s.prediction_mode[ch][band] = -1,
                    }
                }
            }
        }

        /// `Encoder::encode` as it was, its stages in their order and as
        /// they were where they have changed.
        pub fn encode(encoder: &mut Encoder, pcm: &[i32], out: &mut Vec<u8>) {
            let Encoder {
                config,
                state,
                mdct,
                adpcm,
                ..
            } = &mut *encoder;
            let s = &mut **state;
            subband_transform(s, pcm);
            if s.lfe_channel {
                lfe_downsample(s, pcm);
            }
            calc_masking(s, mdct, pcm);
            if let Some(adpcm) = adpcm {
                adpcm_analysis(s, adpcm);
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
            s.shift_history(pcm, config.lfe_history_quirk);
            s.fill_in_adpcm_buffer();

            let header = encoder.header();
            let s = &*encoder.state;
            let w = &mut encoder.writer;
            w.clear();
            header.write(w);
            s.put_primary_audio_header(w);
            for _ in 0..SUBFRAMES {
                s.put_subframe(w);
            }
            w.align_to(8);
            let start = out.len();
            out.extend_from_slice(w.bytes());
            out.resize(start + s.frame_size, 0);
        }

        pub fn init_quantization_noise(s: &mut State, noise: i32, forbid_zero: bool) -> i32 {
            let mut ret = USED_26ABITS | USED_1ABITS;
            s.consumed_bits = 132 + 333 * s.fullband_channels as i32;
            s.consumed_bits += s.consumed_adpcm_bits;
            if s.lfe_channel {
                s.consumed_bits += 72;
            }

            for ch in 0..s.fullband_channels {
                for band in 0..SUBBANDS {
                    let snr_cb = s.peak_cb[ch][band] - s.band_masking_cb[band] - noise;
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
                    s.abits[ch][band] = abits;
                }
                let (sel, bits) = State::best_abits_code(&s.abits[ch]);
                s.bit_allocation_sel[ch] = sel;
                s.consumed_bits += bits;
            }

            // Scale factors again on every pass, to count the Huffman codes.
            for ch in 0..s.fullband_channels {
                for band in 0..SUBBANDS {
                    if s.prediction_mode[ch][band] == -1 {
                        let (scale, quant) =
                            s.calc_one_scale(s.peak_cb[ch][band], s.abits[ch][band]);
                        s.scale_factor[ch][band] = scale;
                        s.quant[ch][band] = quant;
                    }
                }
            }
            quantize_adpcm(s);
            quantize_pcm(s);

            let mut huff_bits = [[[0u32; 7]; CODE_BOOKS]; MAX_CHANNELS];
            let mut clc_bits = [[0u32; CODE_BOOKS]; MAX_CHANNELS];
            let mut bits_counter = 0u32;
            for ch in 0..s.fullband_channels {
                for band in 0..SUBBANDS {
                    let abits = s.abits[ch][band];
                    if abits != 0 && abits as usize <= CODE_BOOKS {
                        let id = abits as usize - 1;
                        let group = usize::from(QUANT_INDEX_GROUP_SIZE[id]);
                        for (sel, total) in huff_bits[ch][id].iter_mut().enumerate().take(group) {
                            *total += s.quantized[ch][band]
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
            for ch in 0..s.fullband_channels {
                bits_counter = bits_counter.wrapping_add(State::best_code(
                    &huff_bits[ch],
                    &clc_bits[ch],
                    &mut s.quant_index_sel[ch],
                ));
            }
            s.consumed_bits = s.consumed_bits.wrapping_add(bits_counter as i32);
            ret
        }
    }

    /// A generator for the comparisons: full 32-bit range, then every
    /// magnitude below it in turn.
    fn lcg(seed: &mut u32) -> i32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let x = *seed as i32;
        x >> (*seed >> 27)
    }

    #[test]
    fn get_cb_finds_what_the_search_finds() {
        let encoder = Encoder::new(Config::reference_five_one()).unwrap();
        let s = &*encoder.state;
        assert!(s.cb_to_level[..2048].windows(2).all(|w| w[0] >= w[1]));
        // Here, every bucket is looked up rather than searched.
        assert!(!s.level_buckets.contains(&LEVEL_SEARCH));
        let mut inputs = vec![0, 1, -1, 2, i32::MIN, i32::MIN + 1, i32::MAX, i32::MAX - 1];
        for &level in &s.cb_to_level[..2048] {
            for x in [level.wrapping_sub(1), level, level.wrapping_add(1)] {
                inputs.extend([x, x.wrapping_neg()]);
            }
        }
        for bits in 0..31 {
            for x in [(1 << bits) - 1, 1 << bits, (1 << bits) + 1, (2 << bits) - 1] {
                inputs.extend([x, -x]);
            }
        }
        let mut seed = 1;
        inputs.extend((0..200_000).map(|_| lcg(&mut seed)));
        for x in inputs {
            assert_eq!(s.get_cb(x), before::get_cb(s, x), "{x}");
        }
    }

    #[test]
    fn get_cb_searches_a_bucket_too_wide() {
        let mut encoder = Encoder::new(Config::reference_five_one()).unwrap();
        let s = &mut *encoder.state;
        // Every other bucket searched: the same answers either way.
        for bucket in s.level_buckets.iter_mut().step_by(2) {
            *bucket = LEVEL_SEARCH;
        }
        let mut seed = 7;
        for _ in 0..100_000 {
            let x = lcg(&mut seed);
            assert_eq!(s.get_cb(x), before::get_cb(s, x), "{x}");
        }
    }

    /// `cb_to_add` is nought from 164 cB on, and only from there: what
    /// the masking model leaves out is left out from there.
    #[test]
    fn levels_164_cb_apart_add_nothing() {
        let encoder = Encoder::new(Config::reference_five_one()).unwrap();
        let s = &*encoder.state;
        assert_eq!(s.silent_gap, 164);
        assert!(s.cb_to_add[164..].iter().all(|&a| a == 0));
        assert_ne!(s.cb_to_add[163], 0);
    }

    #[test]
    fn add_cb_adds_as_it_did() {
        let encoder = Encoder::new(Config::reference_five_one()).unwrap();
        let s = &*encoder.state;
        for a in (-6000..=3000).step_by(7) {
            for d in -700..=700 {
                let b = a + d;
                assert_eq!(s.add_cb(a, b), before::add_cb(s, a, b), "{a} {b}");
            }
        }
    }

    #[test]
    fn a_remembered_scale_is_the_one_searched_for() {
        let mut encoder = Encoder::new(Config::reference_five_one()).unwrap();
        let s = &mut *encoder.state;
        // Twice over, the second time from what the first remembered.
        for _ in 0..2 {
            for peak_cb in -2047..=0 {
                for abits in 0..27 {
                    assert_eq!(
                        s.scale(peak_cb, abits),
                        s.calc_one_scale(peak_cb, abits),
                        "{peak_cb} {abits}"
                    );
                }
            }
        }
    }

    #[test]
    fn mul32_from_the_unsigned_product_is_mul32() {
        let mut edges = vec![
            0,
            1,
            -1,
            2,
            -2,
            i32::MIN,
            i32::MIN + 1,
            i32::MAX,
            i32::MAX - 1,
        ];
        for bits in 0..31 {
            edges.extend([1 << bits, -(1 << bits), (1 << bits) - 1, 1 - (1 << bits)]);
        }
        for &a in &edges {
            for &b in &edges {
                assert_eq!(adpcm::mul32_unsigned(a, b), mul32(a, b), "{a} {b}");
            }
        }
        let mut seed = 3;
        for _ in 0..1_000_000 {
            let (a, b) = (lcg(&mut seed), lcg(&mut seed));
            assert_eq!(adpcm::mul32_unsigned(a, b), mul32(a, b), "{a} {b}");
        }
    }

    /// Input for the stage comparisons: every channel its own mix of the
    /// fixture and the bursts, so that frames run from silence to full scale.
    fn stage_input(frames: usize, channels: usize) -> Vec<i32> {
        let mut pcm: Vec<i32> = fixture(frames / 2)
            .chunks_exact(6)
            .flat_map(|s| (0..channels).map(|ch| s[ch % 6]).collect::<Vec<_>>())
            .collect();
        pcm.extend(bursts(frames - frames / 2, channels));
        pcm
    }

    #[test]
    fn the_analysis_and_the_masking_model_are_as_they_were() {
        for &(sample_rate, _) in &SAMPLE_RATES {
            for layout in [Layout::FivePointOne, Layout::Stereo, Layout::Mono] {
                let mut encoder = Encoder::new(Config {
                    layout,
                    sample_rate,
                    bit_rate: 768_000,
                    ..Config::reference_five_one()
                })
                .unwrap();
                let channels = layout.channels();
                let frames = if sample_rate == 48000 { 30 } else { 8 };
                let pcm = stage_input(frames, channels);
                let mut mdct = Mdct::new();
                for (f, frame) in pcm.chunks_exact(FRAME_SAMPLES * channels).enumerate() {
                    let mut was = encoder.state.clone();
                    let s = &mut *encoder.state;
                    s.fill_window(frame);
                    s.subband_transform();
                    before::subband_transform(&mut was, frame);
                    assert_eq!(s.subband, was.subband, "{sample_rate} Hz, frame {f}");
                    if s.lfe_channel {
                        s.lfe_downsample();
                        before::lfe_downsample(&mut was, frame);
                        assert_eq!(s.downsampled_lfe, was.downsampled_lfe);
                    }
                    for ssf in 0..SUBSUBFRAMES {
                        for ch in 0..s.fullband_channels {
                            let first = 128 + 256 * ssf;
                            let data: [i32; 512] =
                                s.window[ch][first..first + 512].try_into().unwrap();
                            // As `calc_masking` made it: history, then input.
                            let chi = s.channel_order[ch] as usize;
                            let from_history = 512 - first;
                            assert_eq!(data[..from_history], s.history[ch][first..]);
                            for (k, &d) in data[from_history..].iter().enumerate() {
                                assert_eq!(d, frame[k * channels + chi]);
                            }
                            let mut start = [-2047; 256];
                            let mut seed = (f * 8 + ch) as u32;
                            if ch % 2 == 1 {
                                for v in &mut start {
                                    *v = (lcg(&mut seed) >> 20).clamp(-2047, 800);
                                }
                            }
                            let (mut ours, mut theirs) = (start, start);
                            s.adjust_jnd(&mut mdct, &data, &mut ours);
                            before::adjust_jnd(&was, &mut mdct, &data, &mut theirs);
                            assert_eq!(ours, theirs, "{sample_rate} Hz, frame {f}, {ch}");
                        }
                    }
                    s.calc_masking(&mut mdct);
                    s.shift_history(frame, false);
                }
            }
        }
    }

    /// The masking model from a power spectrum of silence with one or two
    /// bins at every level a bin can have, at each of them in turn, and
    /// from noise at every loudness: where the steps left out are left out
    /// at their bounds, the curve is still the one the model made taking
    /// every step.
    #[test]
    fn the_masking_model_is_as_it_was_from_any_power() {
        for &(sample_rate, _) in &SAMPLE_RATES {
            let encoder = Encoder::new(Config {
                sample_rate,
                bit_rate: 768_000,
                ..Config::reference_five_one()
            })
            .unwrap();
            let s = &*encoder.state;
            // `add_cb(get_cb(x), get_cb(x))`: −2017 for nought, 30 at most.
            let silence = s.add_cb(-2047, -2047);
            let loudest = s.add_cb(0, 0);
            let mut spectra = vec![[silence; 256], [loudest; 256]];
            let levels = [loudest, 20, 0, -100, -300, -700, -1100, -1500];
            for bin in 0..256 {
                for &level in &levels {
                    let mut power = [silence; 256];
                    power[bin] = level;
                    spectra.push(power);
                    power[255 - bin / 2] = level - 37;
                    spectra.push(power);
                }
            }
            let mut seed = sample_rate;
            for spread in 0..12 {
                let mut power = [silence; 256];
                for p in &mut power {
                    *p = (loudest - 150 * spread - (lcg(&mut seed) as u32 % 400) as i32)
                        .max(silence);
                }
                spectra.push(power);
            }
            for (case, power) in spectra.iter().enumerate() {
                let (mut ours, mut theirs) = ([-2047; 256], [-2047; 256]);
                s.jnd_from_power(power, &mut ours);
                before::jnd_from_power(s, power, &mut theirs);
                assert_eq!(ours, theirs, "{sample_rate} Hz, spectrum {case}");
            }
        }
    }

    /// Each pass of the SNR search leaves the frame's allocation, scale
    /// factors, samples, predictor history and bit count as the passes did
    /// that quantised every subband every time — through the search's own
    /// walk, and through offsets that jump about.
    #[test]
    fn the_snr_search_passes_are_as_they_were() {
        for (layout, adpcm, bit_rate) in [
            (Layout::FivePointOne, false, 1_509_000),
            (Layout::FivePointOne, true, 768_000),
            (Layout::Stereo, true, 3_840_000),
            (Layout::Mono, false, 160_000),
        ] {
            let mut encoder = Encoder::new(Config {
                layout,
                adpcm,
                bit_rate,
                ..Config::reference_five_one()
            })
            .unwrap();
            let channels = layout.channels();
            let pcm = stage_input(24, channels);
            let mut seed = 11;
            for (f, frame) in pcm.chunks_exact(FRAME_SAMPLES * channels).enumerate() {
                let Encoder {
                    state, mdct, adpcm, ..
                } = &mut encoder;
                let s = &mut **state;
                s.fill_window(frame);
                s.subband_transform();
                if s.lfe_channel {
                    s.lfe_downsample();
                }
                s.calc_masking(mdct);
                if let Some(adpcm) = adpcm {
                    s.adpcm_analysis(adpcm, None);
                }
                s.find_peaks();
                let mut was = s.clone();
                s.quantized_at = [[-1; SUBBANDS]; MAX_CHANNELS];
                let mut noises = vec![s.worst_quantization_noise];
                for step in [128, 128, -128, 64, -32, 16, 8, -4, 2, 1, 0, 0, -1] {
                    noises.push(noises.last().unwrap() + step);
                }
                noises.extend((0..12).map(|_| (lcg(&mut seed) >> 21).clamp(-2047, 2047)));
                for (pass, &noise) in noises.iter().enumerate() {
                    let forbid_zero = pass % 5 != 4;
                    let ours = s.init_quantization_noise(noise, forbid_zero);
                    let theirs = before::init_quantization_noise(&mut was, noise, forbid_zero);
                    let at = format!("{layout:?}, frame {f}, pass {pass}");
                    assert_eq!(ours, theirs, "{at}");
                    assert_eq!(s.consumed_bits, was.consumed_bits, "{at}");
                    assert_eq!(s.abits, was.abits, "{at}");
                    assert_eq!(s.bit_allocation_sel, was.bit_allocation_sel, "{at}");
                    assert_eq!(s.quant_index_sel, was.quant_index_sel, "{at}");
                    assert_eq!(s.scale_factor, was.scale_factor, "{at}");
                    assert_eq!(s.quant, was.quant, "{at}");
                    assert_eq!(s.quantized, was.quantized, "{at}");
                    assert_eq!(s.adpcm_history, was.adpcm_history, "{at}");
                }
                // On through the frame as `encode` goes, from the search.
                s.assign_bits();
                s.shift_history(frame, false);
                s.fill_in_adpcm_buffer();
            }
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
