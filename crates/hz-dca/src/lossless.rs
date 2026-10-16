// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/dca_xll.c, dcadsp.c, dcamath.h — Copyright (C) 2016 foo86
// What was taken and what was changed is recorded in docs/provenance.md.

//! The lossless encoder: PCM in, one lossless (XLL) frame per 512 samples
//! out, each channel the core carries coded as its difference from the
//! core's reconstruction where that is cheaper, every other as itself.
//!
//! No open encoder of this layer exists to port, so this is the decoder run
//! backwards: every step a decoder takes (ETSI TS 102 114 §8.5, and FFmpeg's
//! `dca_xll.c`, which is what the arithmetic below mirrors rounding for
//! rounding) has its inverse here, and whatever the encoder chooses — the
//! channel pairs, the predictors, the codes — is chosen by pricing the
//! candidates exactly and taking the cheapest. Lossless whatever it chooses:
//! a poor choice costs bits, never samples.
//!
//! The channel sets are the reference streams'. A 5.1 is one six-channel set
//! (C, L, R, Ls, Rs, LFE). A 7.1 is a six-channel set — C, L, R, LFE and the
//! side surrounds Lss and Rss — and a two-channel set of the rear surrounds,
//! Lsr and Rsr, whose downmix is embedded in the first: the first set carries
//! its own channels with the rears already mixed in, at the coefficients the
//! second set's header states ([`Fold`]), and a decoder of both sets takes
//! them out again. A decoder of the first set alone gets the 5.1 that fold
//! makes, and so does the core ([`Encoder::core_input`]).
//!
//! A 7.1.4 is the DTS:X extension's standard profile ([`crate::dtsx`]): the
//! 7.1 above, of the composite each height is folded into, and a fourth set
//! of the four heights outside the hierarchy, every channel coded as itself,
//! in a block of its own at the end of the frame.
//!
//! Each set, each frame:
//!
//! 1. **The embedded downmix** (7.1, first set): each channel plus its share
//!    of the rears, `x + (y · c' + 2^14) >> 15`, `c'` the coefficient over
//!    the scale as the decoder computes it.
//! 2. **The core** (first set, [`Config::residual`]): each channel less the
//!    core's reconstruction of it ([`CoreDecoder`]) brought to the set's
//!    resolution — and, for a 7.1, back up from the fold's scale — exactly as
//!    the decoder adds it back (`combine_residual_frame`), wherever coding
//!    that difference costs fewer bits than coding the channel. The decoder
//!    adds the core before it undoes the downmix, so what the difference is
//!    taken from is the channel with its rears in it.
//! 3. **Pairwise decorrelation**: the channels paired, and in a pair the
//!    second less `(first · a + 4) >> 3` for the `a` that saves the most.
//! 4. **Prediction**, per channel: a fixed predictor of order 0 to 3, or an
//!    adaptive one of order 1 to 15 whose reflection coefficients are fitted
//!    (Levinson–Durbin), quantised to the format's table and turned into the
//!    direct form exactly as the decoder turns them.
//! 5. **Entropy coding**, per segment: Rice or binary codes, per channel or
//!    shared, or the previous segment's parameters again.
//!
//! Steps 1 to 5 work in buffers made when the encoder is built; what is
//! rebuilt each frame is the header model, a few hundred bytes.

use crate::bits::Bits;
use crate::core_decode::CoreDecoder;
use crate::dtsx::{self, FoldMatrix};
use crate::encoder::FRAME_SAMPLES;
use crate::navigation::{self, OutOfRange};
use crate::xll::{
    self, Band, BareSet, ChannelSet, CommonHeader, Decorrelation, Downmix, Headers, Lsb, Mapping,
    Navigation, Positions, REFLECTION_COEFFS, Speakers, zigzag,
};
use hz_core::bits::BitWriter;

/// The most taps an adaptive predictor has: its order is four bits.
pub const MAX_ORDER: usize = 15;

/// Sample rate code of 48 kHz.
const RATE_48K: u8 = 12;

/// The longest unary quotient a Rice code is given. FFmpeg reads any; the
/// `dca` decoder stops at 2^20. Far above anything the cost search picks.
const MAX_QUOTIENT: u32 = 1 << 16;

/// The channel layouts the encoder takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// L, R, C, LFE, Ls, Rs.
    FivePointOne,
    /// L, R, C, LFE, Lsr, Rsr, Lss, Rss: WAV's order for 7.1, the rears
    /// before the sides.
    SevenPointOne,
    /// The 7.1, then the heights TFL, TFR, TBL, TBR: WAV's order for 7.1.4.
    /// The DTS:X extension's standard profile ([`crate::dtsx`]): the heights
    /// folded into the 7.1's corners, and carried beside it in a block of
    /// their own. 24 bits only — a height's share of a corner is a 24-bit
    /// quantity, so a bed of fewer bits could not hold it exactly.
    SevenPointOneFour,
}

impl Layout {
    /// Channels of the input.
    pub fn channels(self) -> usize {
        match self {
            Layout::FivePointOne => 6,
            Layout::SevenPointOne => 8,
            Layout::SevenPointOneFour => 12,
        }
    }

    /// Channels of the bed: what a decoder of the lossless layer alone
    /// presents.
    pub fn bed_channels(self) -> usize {
        match self {
            Layout::SevenPointOneFour => 8,
            other => other.channels(),
        }
    }
}

/// DTS speaker mask bits (ETSI TS 102 114 table 6-22 order).
mod speaker {
    pub const C: u32 = 1 << 0;
    pub const L: u32 = 1 << 1;
    pub const R: u32 = 1 << 2;
    pub const LS: u32 = 1 << 3;
    pub const RS: u32 = 1 << 4;
    pub const LFE: u32 = 1 << 5;
    pub const LSR: u32 = 1 << 7;
    pub const RSR: u32 = 1 << 8;
    pub const LSS: u32 = 1 << 9;
    pub const RSS: u32 = 1 << 10;
}

/// How a 7.1's rear surrounds fold into the 5.1 below them, as the codes the
/// two-channel set's downmix matrix carries ([`xll::coefficient`],
/// [`xll::scale`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    /// The scale of every channel of the 5.1.
    pub scale: u16,
    /// The coefficient each rear surround folds into its side surround at.
    pub rear: u16,
}

impl Fold {
    /// The reference streams' fold: every channel of the 5.1 at −3 dB, each
    /// rear into its side at −3 dB, so that a side and its rear at full scale
    /// together reach full scale and no further.
    pub const REFERENCE: Fold = Fold {
        scale: 0x100 | 217,
        rear: 0x100 | 217,
    };
}

/// What to encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub layout: Layout,
    /// Bits per sample of the source, 16 or 24.
    pub bits: u8,
    /// `log2` of the segments per frame: 1 is two of 256 samples, the
    /// reference streams' and the longest segment 48 kHz allows.
    pub segments_log2: u8,
    /// Code the MSB/LSB split flag, splitting nothing — the shape of the
    /// 24-bit reference streams, sixteen bits and four a channel more than
    /// leaving it off, which the 16-bit one does.
    pub scalable_lsbs: bool,
    /// The fold of a 7.1's rears; unused for 5.1.
    pub fold: Fold,
    /// Code a channel the core carries as its difference from the core's
    /// reconstruction, where that costs less — as the reference streams do —
    /// when [`Encoder::encode`] is given that reconstruction. Off, every
    /// channel is coded as itself, independent of the core.
    pub residual: bool,
}

impl Config {
    /// The reference streams' shape for `layout` at `bits`.
    pub fn reference(layout: Layout, bits: u8) -> Self {
        Self {
            layout,
            bits,
            segments_log2: 1,
            scalable_lsbs: bits > 16,
            fold: Fold::REFERENCE,
            residual: true,
        }
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

/// Input the lossless layer cannot carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// A sample with bits below the configured resolution: coding it would
    /// lose them.
    Resolution {
        channel: usize,
        sample: usize,
        value: i32,
        bits: u8,
    },
    /// A 7.1.4 corner that, with its height folded in, passes 24 bits: a
    /// decoder would clip the composite, and the height could not be taken
    /// back out of it exactly. The master needs headroom before the fold.
    Fold {
        /// The input frame, counted from 0.
        frame: u64,
        /// The corner, as an input channel.
        channel: usize,
        sample: usize,
        /// The composite, in 24-bit units.
        value: i32,
    },
    /// A 7.1.4 frame whose extension block the navigation word cannot
    /// point at: the bed before it is too large, or the block itself is.
    /// The frame is not cut or coded coarser to make it fit; the stream
    /// ends here.
    Navigation {
        /// The input frame, counted from 0.
        frame: u64,
        cause: OutOfRange,
    },
    /// A frame the stream cannot carry under its peak bit rate ceiling, even
    /// with the decoder's smoothing buffer (`crate::peak`). It is not coded
    /// coarser to fit; the stream ends before it.
    PeakRate {
        /// The input frame, counted from 0.
        frame: u64,
        cause: crate::peak::Overrun,
    },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Resolution {
                channel,
                sample,
                value,
                bits,
            } => write!(
                f,
                "channel {channel}, sample {sample}: {value:#010x} has bits below the \
                 {bits}-bit resolution, which a lossless layer of {bits} bits would drop"
            ),
            Refused::Fold {
                frame,
                channel,
                sample,
                value,
            } => write!(
                f,
                "frame {frame}, channel {channel}, sample {sample}: the corner with its height \
                 folded in at −3 dB is {value}, past 24 bits; the master needs headroom before \
                 the fold"
            ),
            Refused::Navigation { frame, cause } => write!(
                f,
                "frame {frame} (from sample {}): {cause}, so the DTS:X extension cannot be \
                 located; not cut to fit",
                frame * FRAME_SAMPLES as u64
            ),
            Refused::PeakRate { frame, cause } => write!(
                f,
                "frame {frame} (from sample {}): {cause}; lossless data is not coded coarser \
                 to fit",
                frame * FRAME_SAMPLES as u64
            ),
        }
    }
}

impl Refused {
    /// The same refusal, of input frame `at`: for a caller that counts its
    /// input otherwise than the frames this encoder has coded.
    pub fn of_frame(mut self, at: u64) -> Self {
        if let Refused::Fold { frame, .. }
        | Refused::Navigation { frame, .. }
        | Refused::PeakRate { frame, .. } = &mut self
        {
            *frame = at;
        }
        self
    }
}

impl std::error::Error for Refused {}

/// `(a · b + 2^(bits−1)) >> bits`, truncated to 32 bits as the decoder's
/// `norm__` truncates.
fn mul(a: i32, b: i32, bits: u32) -> i32 {
    ((i64::from(a) * i64::from(b) + (1 << (bits - 1))) >> bits) as i32
}

fn clip23(a: i32) -> i32 {
    a.clamp(-(1 << 23), (1 << 23) - 1)
}

/// One channel set's fixed shape and its working buffers.
struct Set {
    /// Input channel, in WAV order, of each of the set's channels, which are
    /// in the order of the mask's bits.
    inputs: Vec<usize>,
    mask: u32,
    /// The core's channel (WAV 5.1 order) each of the set's channels is
    /// coded against, where the core carries it.
    core: Vec<Option<usize>>,
    /// Which channels the frame codes as themselves rather than against the
    /// core: the header's residual mask, a bit per channel.
    full: u32,
    /// The non-primary set's downmix codes, scale first in each row.
    downmix: Option<Vec<u16>>,
    /// The samples, per channel in the set's order: the input, then (first
    /// set of a 7.1) with the rears mixed in, then less the core where it is
    /// coded against it.
    samples: Vec<Vec<i32>>,
    /// The decorrelated channels, in coded order.
    coded: Vec<Vec<i32>>,
    /// Each coded channel's prediction residual, then zigzagged.
    residual: Vec<Vec<i32>>,
    codes: Vec<Vec<u32>>,
    band: Band,
}

impl Set {
    fn new(
        inputs: Vec<usize>,
        mask: u32,
        core: Vec<Option<usize>>,
        downmix: Option<Vec<u16>>,
        scalable: bool,
    ) -> Self {
        let n = inputs.len();
        let buffers = || vec![vec![0i32; FRAME_SAMPLES]; n];
        Self {
            inputs,
            mask,
            core,
            full: (1 << n) - 1,
            downmix,
            samples: buffers(),
            coded: buffers(),
            residual: buffers(),
            codes: vec![vec![0u32; FRAME_SAMPLES]; n],
            band: Band {
                decorrelation: None,
                adaptive_orders: vec![0; n],
                fixed_orders: vec![0; n],
                reflection: vec![Vec::with_capacity(MAX_ORDER); n],
                embedded_downmix: None,
                // The split flag with nothing split: no LSB section, no
                // width reduced.
                lsb: scalable.then(|| Lsb {
                    size: 0,
                    widths: vec![0; n],
                }),
                width_adjust: scalable.then(|| vec![0; n]),
            },
        }
    }

    fn channels(&self) -> usize {
        self.inputs.len()
    }
}

/// The lossless encoder.
pub struct Encoder {
    config: Config,
    sets: Vec<Set>,
    /// One row of the first set's downmix matrix per channel of the first
    /// set; none for a 5.1.
    fold: Vec<Row>,
    /// Bits of a coding parameter (`nabits`).
    parameter_bits: u32,
    /// The 5.1 the core is to be made from, interleaved in WAV order, 32-bit.
    core: Vec<i32>,
    headers: Headers,
    /// Per set, the coding parameters the last segment left.
    previous: Vec<Parameters>,
    /// One frame's band data.
    data: BitWriter,
    frame: Vec<u8>,
    search: Search,
    /// The DTS:X extension of a 7.1.4.
    heights: Option<Heights>,
    /// Frames encoded.
    frames: u64,
}

/// The DTS:X extension's standard profile: the heights, the block they go
/// out in, and the composite bed they are folded into.
struct Heights {
    /// TFL, TFR, TBL, TBR, every channel coded as itself.
    set: Set,
    /// The block's headers: the fold matrix, which the fold below is read
    /// from, and the height set's sub-header and navigation table.
    block: dtsx::Block,
    /// `[height][bed column]`: the Q15 gain a height goes into a column at.
    gains: Vec<[i32; 8]>,
    /// The bed column each of the 7.1's input channels is (WAV's L R C LFE
    /// Lsr Rsr Lss Rss in the bed's C L R LFE Lsr Rsr Lss Rss).
    columns: [usize; 8],
    /// The 7.1 with the heights in it, interleaved in WAV order, 32-bit:
    /// what the bed encodes and the core is made from.
    composite: Vec<i32>,
    /// The coding parameters the last segment left.
    previous: Parameters,
    /// One frame's height data.
    data: BitWriter,
    /// Where the frame last encoded put the block.
    navigation: navigation::Navigation,
}

impl Heights {
    fn new(scalable: bool) -> Self {
        let mut matrix = FoldMatrix::standard();
        matrix.fit();
        let gains = matrix
            .gains()
            .expect("the standard matrix's gains are known");
        let set = Set::new(vec![8, 9, 10, 11], 0, vec![None; 4], None, scalable);
        let header = ChannelSet {
            size: 0,
            channels: 4,
            residual_encode: 0xf,
            pcm_bit_res: 24,
            storage_bit_res: 24,
            frequency_code: RATE_48K,
            interpolation: 0,
            replacement: None,
            mapping: Mapping::Unmapped,
            extra_bands: None,
            bands: vec![set.band.clone()],
            reserved: Bits::new(),
            crc: 0,
        };
        Self {
            set,
            block: dtsx::Block {
                matrix,
                heights: BareSet {
                    set: header,
                    navigation: Navigation {
                        sizes: Vec::new(),
                        padding: Bits::new(),
                        crc: 0,
                    },
                },
            },
            gains,
            columns: [1, 2, 0, 3, 4, 5, 6, 7],
            composite: vec![0; FRAME_SAMPLES * 8],
            previous: Parameters::default(),
            data: BitWriter::with_capacity(FRAME_SAMPLES * 4 * 4),
            navigation: navigation::Navigation { offset: 0, size: 0 },
        }
    }

    /// The composite sample of bed input channel `ch` at sample `n` of
    /// `pcm`, in 24-bit units: the channel, and each height's share.
    fn composite(&self, pcm: &[i32], n: usize, ch: usize) -> i64 {
        let column = self.columns[ch];
        let mut x = i64::from(pcm[n * 12 + ch] >> 8);
        for (h, gains) in self.gains.iter().enumerate() {
            if gains[column] != 0 {
                x += i64::from(dtsx::fold(pcm[n * 12 + 8 + h] >> 8, gains[column]));
            }
        }
        x
    }

    /// Fold `pcm`, a 7.1.4 frame whose corners [`Heights::check`] has
    /// found room for, into the composite 7.1.
    fn fold(&mut self, pcm: &[i32]) {
        for n in 0..FRAME_SAMPLES {
            for ch in 0..8 {
                self.composite[n * 8 + ch] = (self.composite(pcm, n, ch) as i32) << 8;
            }
        }
    }

    /// The first corner of `pcm` that its heights take past 24 bits.
    fn check(&self, pcm: &[i32], frame: u64) -> Result<(), Refused> {
        for n in 0..FRAME_SAMPLES {
            for ch in 0..8 {
                let x = self.composite(pcm, n, ch);
                if !(-(1 << 23)..1 << 23).contains(&x) {
                    return Err(Refused::Fold {
                        frame,
                        channel: ch,
                        sample: n,
                        value: x as i32,
                    });
                }
            }
        }
        Ok(())
    }
}

/// One row of the fold as the decoder holds it.
struct Row {
    /// The scale, in Q15.
    scale: i32,
    /// Its inverse, in Q16: what brings the core back up from the scale
    /// before a residual is added to it.
    inverse: i32,
    /// Per rear, the coefficient the decoder subtracts with, in Q15.
    undo: Vec<i32>,
}

/// Scratch for the predictor and pairing searches.
struct Search {
    /// Every channel's part A and part B, priced as one run.
    runs: (Vec<u32>, Vec<u32>),
    /// A channel decorrelated, being priced.
    pair: Vec<i32>,
    work: Vec<i32>,
    best: Vec<i32>,
    codes: Vec<u32>,
    windowed: Vec<f64>,
    /// A channel less the core, being priced against the channel.
    difference: Vec<i32>,
}

impl Encoder {
    pub fn new(config: Config) -> Result<Self, Unsupported> {
        if config.bits != 16 && config.bits != 24 {
            return Err(Unsupported(format!(
                "{} bits: the lossless encoder takes 16 or 24",
                config.bits
            )));
        }
        // 256 samples is the longest segment 48 kHz allows, two the shortest
        // that is one: from two segments of a frame to 256. Checked before
        // any shift by the count.
        if !(1..=8).contains(&config.segments_log2) {
            return Err(Unsupported(format!(
                "2^{} segments: a 48 kHz frame has 2 to 256, segments of 256 to 2 samples",
                config.segments_log2
            )));
        }
        let segment_samples = FRAME_SAMPLES >> config.segments_log2;
        use speaker::*;
        let (sets, mask_bits) = match config.layout {
            // WAV: L R C LFE Ls Rs, the core's own order.
            Layout::FivePointOne => (
                vec![Set::new(
                    vec![2, 0, 1, 4, 5, 3],
                    C | L | R | LS | RS | LFE,
                    [2, 0, 1, 4, 5, 3].map(Some).to_vec(),
                    None,
                    config.scalable_lsbs,
                )],
                6,
            ),
            // WAV: L R C LFE Lsr Rsr Lss Rss; for a 7.1.4, of the composite
            // the heights are folded into.
            Layout::SevenPointOne | Layout::SevenPointOneFour => {
                if config.layout == Layout::SevenPointOneFour && config.bits != 24 {
                    return Err(Unsupported(format!(
                        "7.1.4 at {} bits: the DTS:X extension folds its heights into a \
                         24-bit bed",
                        config.bits
                    )));
                }
                let fold = config.fold;
                if xll::scale(fold.scale).is_none() || xll::coefficient(fold.rear).is_none() {
                    return Err(Unsupported(format!(
                        "fold codes {:#05x}, {:#05x}: not a downmix scale and coefficient",
                        fold.scale, fold.rear
                    )));
                }
                // One row per channel of the first set: C L R LFE Lss Rss.
                let mut codes = Vec::with_capacity(6 * 3);
                for row in 0..6 {
                    codes.push(fold.scale);
                    codes.push(if row == 4 { fold.rear } else { 0x100 });
                    codes.push(if row == 5 { fold.rear } else { 0x100 });
                }
                (
                    vec![
                        // The side surrounds against the core's surrounds,
                        // which carry them (`ff_dca_core_map_spkr`).
                        Set::new(
                            vec![2, 0, 1, 3, 6, 7],
                            C | L | R | LFE | LSS | RSS,
                            [2, 0, 1, 3, 4, 5].map(Some).to_vec(),
                            None,
                            config.scalable_lsbs,
                        ),
                        // The core has no rears.
                        Set::new(
                            vec![4, 5],
                            LSR | RSR,
                            vec![None, None],
                            Some(codes),
                            config.scalable_lsbs,
                        ),
                    ],
                    11,
                )
            }
        };
        let fold = match &sets.get(1) {
            Some(rears) => {
                let codes = rears.downmix.as_ref().expect("the rear set's matrix");
                codes
                    .chunks(3)
                    .map(|row| {
                        let (scale, inverse) = xll::scale(row[0]).expect("checked above");
                        let undo = row[1..]
                            .iter()
                            .map(|&code| {
                                // FFmpeg's `parse_dmix_coeffs`: the
                                // coefficient's magnitude over the scale's,
                                // then the coefficient's sign.
                                let c = xll::coefficient(code).expect("checked above");
                                let magnitude = mul(inverse.abs(), c.abs(), 16);
                                if c < 0 { -magnitude } else { magnitude }
                            })
                            .collect();
                        Row {
                            scale,
                            inverse,
                            undo,
                        }
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        let storage_bits = u32::from(config.bits);
        let mut parameter_bits = if storage_bits > 16 { 5 } else { 4 };
        if sets.len() > 1 && parameter_bits < 5 {
            parameter_bits += 1;
        }
        let headers = Headers {
            common: CommonHeader {
                version: 0,
                header_size: 0,
                frame_size_bits: 1,
                frame_size: 1,
                channel_sets: sets.len() as u8,
                segments_log2: config.segments_log2,
                segment_samples_log2: segment_samples.trailing_zeros() as u8,
                segment_size_bits: 1,
                band_crc: 0,
                fixed_lsb_width: config.scalable_lsbs.then_some(0),
                channel_mask_bits: mask_bits,
                reserved: Bits::new(),
                crc: 0,
            },
            channel_sets: sets
                .iter()
                .enumerate()
                .map(|(index, set)| ChannelSet {
                    size: 0,
                    channels: set.channels() as u8,
                    residual_encode: ((1u32 << set.channels()) - 1) as u16,
                    pcm_bit_res: config.bits,
                    storage_bit_res: config.bits,
                    frequency_code: RATE_48K,
                    interpolation: 0,
                    replacement: None,
                    mapping: Mapping::Speakers(Speakers {
                        primary: index == 0,
                        downmix: set.downmix.clone().map(|codes| Downmix {
                            embedded: true,
                            kind: None,
                            codes,
                        }),
                        hierarchical: true,
                        positions: Positions::Mask(set.mask),
                    }),
                    extra_bands: None,
                    bands: vec![set.band.clone()],
                    reserved: Bits::new(),
                    crc: 0,
                })
                .collect(),
            navigation: Navigation {
                sizes: Vec::new(),
                padding: Bits::new(),
                crc: 0,
            },
        };
        let heights = (config.layout == Layout::SevenPointOneFour)
            .then(|| Heights::new(config.scalable_lsbs));
        Ok(Self {
            heights,
            frames: 0,
            core: vec![0; FRAME_SAMPLES * 6],
            previous: vec![Parameters::default(); sets.len()],
            sets,
            fold,
            parameter_bits,
            headers,
            data: BitWriter::with_capacity(FRAME_SAMPLES * 8 * 4),
            frame: Vec::with_capacity(FRAME_SAMPLES * 8 * 4),
            search: Search {
                runs: (
                    Vec::with_capacity(FRAME_SAMPLES * MAX_SET_CHANNELS),
                    Vec::with_capacity(FRAME_SAMPLES * MAX_SET_CHANNELS),
                ),
                pair: vec![0; FRAME_SAMPLES],
                work: vec![0; FRAME_SAMPLES],
                best: vec![0; FRAME_SAMPLES],
                codes: vec![0; FRAME_SAMPLES],
                windowed: vec![0.0; FRAME_SAMPLES],
                difference: vec![0; FRAME_SAMPLES],
            },
            config,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The EXSS speaker activity mask of the asset: its code (the mask is
    /// `4 · (code + 1)` bits wide) and value.
    pub fn speaker_activity(&self) -> (u8, u32) {
        match self.config.layout {
            // C, L/R, Ls/Rs, LFE.
            Layout::FivePointOne => (0, 0x00f),
            // C, L/R, LFE, Lsr/Rsr, Lss/Rss: the heights are not the
            // bed's.
            Layout::SevenPointOne | Layout::SevenPointOneFour => (2, 0x84b),
        }
    }

    /// Where the DTS:X extension block of the frame last encoded sits in
    /// it, for the navigation word: a 7.1.4's, `None` for any other layout.
    pub fn extension(&self) -> Option<navigation::Navigation> {
        self.heights.as_ref().map(|h| h.navigation)
    }

    /// The 5.1 a decoder of the first channel set alone presents for `pcm`,
    /// interleaved in WAV order — L, R, C, LFE, Ls, Rs — as 32-bit samples:
    /// what the core is to be encoded from. For a 5.1 it is the input itself;
    /// for a 7.1, the first set as [`Encoder::encode`] folds it, scaled as a
    /// decoder of that set alone scales it; for a 7.1.4, the same of the
    /// composite 7.1 the heights are folded into. A 7.1.4 frame
    /// [`Encoder::check`] refuses gives its corners clipped.
    ///
    /// # Panics
    /// If `pcm` is not exactly one frame.
    pub fn core_input(&mut self, pcm: &[i32]) -> &[i32] {
        let channels = self.config.layout.channels();
        assert_eq!(
            pcm.len(),
            FRAME_SAMPLES * channels,
            "a frame is {FRAME_SAMPLES} samples of each channel"
        );
        let Some(heights) = &mut self.heights else {
            self.make_core_input(pcm, channels);
            return &self.core;
        };
        for n in 0..FRAME_SAMPLES {
            for ch in 0..8 {
                let x = heights
                    .composite(pcm, n, ch)
                    .clamp(-(1 << 23), (1 << 23) - 1);
                heights.composite[n * 8 + ch] = (x as i32) << 8;
            }
        }
        let composite = std::mem::take(&mut heights.composite);
        self.make_core_input(&composite, 8);
        self.heights.as_mut().expect("a 7.1.4").composite = composite;
        &self.core
    }

    /// [`Encoder::core_input`] of a 5.1 or a 7.1, `channels` wide.
    fn make_core_input(&mut self, pcm: &[i32], channels: usize) {
        let (front, rears) = match &self.sets[..] {
            [front, rears] => (front, rears),
            _ => {
                self.core.copy_from_slice(pcm);
                return;
            }
        };
        let shift = 32 - u32::from(self.config.bits);
        let (low, high) = (-(1i32 << (31 - shift)), (1i32 << (31 - shift)) - 1);
        for (row, (&input, slot)) in self.fold.iter().zip(front.inputs.iter().zip(&front.core)) {
            let slot = slot.expect("the core carries every channel of the first set");
            for n in 0..FRAME_SAMPLES {
                // As `fold_rears` folds, rear by rear.
                let mut x = pcm[n * channels + input] >> shift;
                for (&rear, &c) in rears.inputs.iter().zip(&row.undo) {
                    if c != 0 {
                        x = x.wrapping_add(mul(pcm[n * channels + rear] >> shift, c, 15));
                    }
                }
                self.core[n * 6 + slot] = mul(x, row.scale, 15).clamp(low, high) << shift;
            }
        }
    }

    /// The lossless frame last encoded.
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    /// Bytes no frame of this encoder's passes. A 7.1.4's are bounded by
    /// the navigation word, which cannot locate a block past its reach: the
    /// frame is refused first. Any other's by the binary code the search
    /// prices for every segment, which holds a sample in 32 bits at most,
    /// with room for the headers and the navigation tables.
    pub fn frame_bound(&self) -> usize {
        match self.config.layout {
            Layout::SevenPointOneFour => navigation::MAX_OFFSET + navigation::MAX_SIZE,
            layout => layout.channels() * FRAME_SAMPLES * 4 + 4096,
        }
    }

    /// How many channels the frame last encoded codes against the core.
    pub fn against_core(&self) -> u32 {
        self.sets
            .iter()
            .map(|set| (((1 << set.channels()) - 1) & !set.full).count_ones())
            .sum()
    }

    /// Refuse a frame the configured resolution cannot carry, or — a 7.1.4
    /// — whose heights take a corner past 24 bits, naming it as the next
    /// frame this encoder would code. The navigation word's limits are only
    /// known once the frame is coded, so [`Encoder::encode`] checks them.
    pub fn check(&self, pcm: &[i32]) -> Result<(), Refused> {
        let channels = self.config.layout.channels();
        let shift = 32 - u32::from(self.config.bits);
        if let Some(at) = pcm.iter().position(|&s| s & ((1 << shift) - 1) != 0) {
            return Err(Refused::Resolution {
                channel: at % channels,
                sample: at / channels,
                value: pcm[at],
                bits: self.config.bits,
            });
        }
        match &self.heights {
            Some(heights) => heights.check(pcm, self.frames),
            None => Ok(()),
        }
    }

    /// Frames encoded so far: the index of the next.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Encode one frame — [`FRAME_SAMPLES`] samples of every channel,
    /// interleaved in WAV order as 32-bit samples whose bits below the
    /// configured resolution are zero — and return the lossless frame.
    ///
    /// `core` is the core's reconstruction of the same samples — a decoder's
    /// output for the core frame that travels with this one, which is the
    /// core encoded from the *next* frame's [`Encoder::core_input`], since a
    /// decoder's output is a frame behind its input. With it, and
    /// [`Config::residual`] on, each channel the core carries is coded
    /// against it where that is cheaper; without it, every channel is coded
    /// as itself.
    ///
    /// A 7.1.4's heights are folded into the 7.1 first, and everything else
    /// is done to that composite; the heights go out as they came in, in
    /// the DTS:X extension block at the end of the frame. A frame whose
    /// block the navigation word cannot locate is refused
    /// ([`Refused::Navigation`]): the frame is coded, but there is no
    /// stream to put it in.
    ///
    /// # Panics
    /// If `pcm` is not exactly one frame.
    pub fn encode(&mut self, pcm: &[i32], core: Option<&CoreDecoder>) -> Result<&[u8], Refused> {
        let channels = self.config.layout.channels();
        assert_eq!(
            pcm.len(),
            FRAME_SAMPLES * channels,
            "a frame is {FRAME_SAMPLES} samples of each channel"
        );
        self.check(pcm)?;
        let segment_samples = FRAME_SAMPLES >> self.config.segments_log2;
        let parameter_bits = self.parameter_bits;
        match &mut self.heights {
            Some(heights) => {
                heights.fold(pcm);
                load(&mut heights.set, pcm, channels, 8);
                decorrelate(
                    &mut heights.set,
                    parameter_bits,
                    segment_samples,
                    &mut self.search,
                );
                for ch in 0..4 {
                    predict(
                        &mut heights.set,
                        ch,
                        parameter_bits,
                        segment_samples,
                        &mut self.search,
                    );
                }
                let composite = std::mem::take(&mut heights.composite);
                for set in &mut self.sets {
                    load(set, &composite, 8, 8);
                }
                self.heights.as_mut().expect("a 7.1.4").composite = composite;
            }
            None => {
                let shift = 32 - u32::from(self.config.bits);
                for set in &mut self.sets {
                    load(set, pcm, channels, shift);
                }
            }
        }
        self.fold_rears();
        if let Some(core) = core.filter(|_| self.config.residual) {
            self.subtract_core(core);
        }
        for set in &mut self.sets {
            decorrelate(set, parameter_bits, segment_samples, &mut self.search);
            for ch in 0..set.channels() {
                predict(set, ch, parameter_bits, segment_samples, &mut self.search);
            }
        }
        self.assemble()?;
        self.frames += 1;
        Ok(&self.frame)
    }

    /// Mix each rear into the first set as the decoder will take it out.
    fn fold_rears(&mut self) {
        let [front, rears] = &mut self.sets[..] else {
            return;
        };
        for (plane, row) in front.samples.iter_mut().zip(&self.fold) {
            for (rear, &c) in rears.samples.iter().zip(&row.undo) {
                if c == 0 {
                    continue;
                }
                for (x, &y) in plane.iter_mut().zip(rear) {
                    *x = x.wrapping_add(mul(y, c, 15));
                }
            }
        }
    }

    /// Take the core out of each channel of the first set it carries, where
    /// coding what is left costs less than coding the channel: the inverse
    /// of the decoder's `combine_residual_frame`. The core is brought to the
    /// set's resolution with rounding and, below a downmix, back up from its
    /// scale first and clipped to 24 bits; the decoder adds it to the
    /// channel before it undoes that downmix, so the channel here has its
    /// rears in it.
    fn subtract_core(&mut self, core: &CoreDecoder) {
        // No LSBs are split off, so the core comes down by what the
        // resolution lacks of 24 bits.
        let shift = 24 - u32::from(self.config.bits);
        let round = if shift > 0 { 1 << (shift - 1) } else { 0 };
        let segment_samples = FRAME_SAMPLES >> self.config.segments_log2;
        let parameter_bits = self.parameter_bits;
        let search = &mut self.search;
        let mut difference = std::mem::take(&mut search.difference);
        let set = &mut self.sets[0];
        for ch in 0..set.channels() {
            let Some(slot) = set.core[ch] else {
                continue;
            };
            let src = core.channel(slot);
            let x = &mut set.samples[ch];
            match self.fold.get(ch) {
                Some(row) => {
                    for ((d, &x), &s) in difference.iter_mut().zip(x.iter()).zip(src) {
                        let core = clip23(mul(s, row.inverse, 16).wrapping_add(round) >> shift);
                        *d = x.wrapping_sub(core);
                    }
                }
                None => {
                    for ((d, &x), &s) in difference.iter_mut().zip(x.iter()).zip(src) {
                        *d = x.wrapping_sub(s.wrapping_add(round) >> shift);
                    }
                }
            }
            let itself = search_prediction(x, parameter_bits, segment_samples, search).0;
            let against = search_prediction(&difference, parameter_bits, segment_samples, search).0;
            if against < itself {
                x.copy_from_slice(&difference);
                set.full &= !(1 << ch);
            }
        }
        search.difference = difference;
    }

    /// Lay the frame out: headers sized to the band data, the data, and
    /// zeroes to the next 32-bit word — then, for a 7.1.4, the extension
    /// block, which ends the frame.
    fn assemble(&mut self) -> Result<(), Refused> {
        let segments = 1usize << self.config.segments_log2;
        let segment_samples = FRAME_SAMPLES >> self.config.segments_log2;
        let parameter_bits = self.parameter_bits;
        self.data.clear();
        let sizes = &mut self.headers.navigation.sizes;
        sizes.clear();
        let previous = &mut self.previous;
        previous.fill(Parameters::default());
        for segment in 0..segments {
            for (set, previous) in self.sets.iter().zip(previous.iter_mut()) {
                let start = self.data.byte_position();
                write_segment(
                    &mut self.data,
                    set,
                    segment,
                    segment_samples,
                    parameter_bits,
                    previous,
                    &mut self.search.runs,
                );
                self.data.align_to(8);
                sizes.push((self.data.byte_position() - start) as u32);
            }
        }
        let mut largest = sizes.iter().copied().max().unwrap_or(1);
        // The height set's segments, which its own table sizes in the
        // frame's width.
        if let Some(heights) = &mut self.heights {
            heights.data.clear();
            heights.previous = Parameters::default();
            let sizes = &mut heights.block.heights.navigation.sizes;
            sizes.clear();
            for segment in 0..segments {
                let start = heights.data.byte_position();
                write_segment(
                    &mut heights.data,
                    &heights.set,
                    segment,
                    segment_samples,
                    parameter_bits,
                    &mut heights.previous,
                    &mut self.search.runs,
                );
                heights.data.align_to(8);
                sizes.push((heights.data.byte_position() - start) as u32);
            }
            largest = largest.max(sizes.iter().copied().max().unwrap_or(1));
        }
        for (header, set) in self.headers.channel_sets.iter_mut().zip(&self.sets) {
            header.residual_encode = set.full as u16;
            header.bands[0].clone_from(&set.band);
            header.reserved = Bits::new();
        }
        let common = &mut self.headers.common;
        common.reserved = Bits::new();
        common.segment_size_bits = bits_for(largest - 1).max(1) as u8;
        let block = match &mut self.heights {
            Some(heights) => {
                let header = &mut heights.block.heights.set;
                header.bands[0].clone_from(&heights.set.band);
                header.reserved = Bits::new();
                heights
                    .block
                    .heights
                    .fit(common)
                    .expect("the encoder's own height set fits");
                heights.block.size(common, heights.data.byte_position())
            }
            None => 0,
        };
        // The frame size's width moves the header, which moves the frame
        // size: settle it.
        let data = self.data.byte_position();
        let bed = loop {
            self.headers
                .fit(true)
                .expect("the encoder's own headers fit");
            let bed = (self.headers.len() + data).next_multiple_of(4);
            let size = (bed + block) as u32;
            let width = bits_for(size - 1).max(1) as u8;
            let settled = self.headers.common.frame_size == size
                && self.headers.common.frame_size_bits == width;
            self.headers.common.frame_size = size;
            self.headers.common.frame_size_bits = width;
            if settled {
                break bed;
            }
        };
        self.frame.clear();
        self.frame.extend(
            self.headers
                .write(true)
                .expect("the encoder's own headers write"),
        );
        self.frame.extend_from_slice(self.data.bytes());
        self.frame.resize(bed, 0);
        if let Some(heights) = &mut self.heights {
            self.frame.extend(
                heights
                    .block
                    .write(&self.headers.common)
                    .expect("the encoder's own block writes"),
            );
            self.frame.extend_from_slice(heights.data.bytes());
            // The trailer and the padding: zeroes to the frame's end.
            self.frame
                .resize(self.headers.common.frame_size as usize, 0);
            heights.navigation = navigation::Navigation {
                offset: bed,
                size: block,
            };
            if let Err(cause) = heights.navigation.word() {
                return Err(Refused::Navigation {
                    frame: self.frames,
                    cause,
                });
            }
        }
        Ok(())
    }
}

/// Take `set`'s channels of `pcm`, `channels` wide and `shift` bits above
/// the set's resolution, every channel coded as itself until the core says
/// otherwise.
fn load(set: &mut Set, pcm: &[i32], channels: usize, shift: u32) {
    for (plane, &input) in set.samples.iter_mut().zip(&set.inputs) {
        for (n, sample) in plane.iter_mut().enumerate() {
            *sample = pcm[n * channels + input] >> shift;
        }
    }
    set.full = (1 << set.channels()) - 1;
}

/// Bits that hold `value`.
fn bits_for(value: u32) -> u32 {
    u32::BITS - value.leading_zeros()
}

/// One parameter set of a segment: Rice or binary, and the parameter of
/// part A (the start of the frame an adaptive predictor has no history for)
/// and of part B — a Rice parameter, or a binary width (0 for zeroes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ParameterSet {
    rice: bool,
    part_a: u32,
    part_a_samples: usize,
    part_b: u32,
}

/// The most channels a set the encoder makes has.
const MAX_SET_CHANNELS: usize = 8;

/// What a segment's coding parameters are, as the decoder holds them from
/// one segment to the next: a segment can say "the same again".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Parameters {
    common: bool,
    /// Parameter sets in use: 1 when common, one per channel otherwise; 0
    /// before the first segment.
    count: usize,
    sets: [ParameterSet; MAX_SET_CHANNELS],
}

impl Parameters {
    fn sets(&self) -> &[ParameterSet] {
        &self.sets[..self.count]
    }

    fn of(&self, ch: usize) -> &ParameterSet {
        &self.sets[if self.common { 0 } else { ch }]
    }
}

/// Bits of `codes` in Rice code `k`.
fn rice_bits(codes: &[u32], k: u32) -> u64 {
    codes.len() as u64 * u64::from(k + 1) + codes.iter().map(|&u| u64::from(u >> k)).sum::<u64>()
}

/// Whether Rice code `k` keeps every quotient of `codes` in bounds.
fn rice_fits(codes: &[u32], k: u32) -> bool {
    codes.iter().all(|&u| u >> k <= MAX_QUOTIENT)
}

/// The cheapest Rice parameter up to `max_k` for `codes`, and its bits.
fn best_rice(codes: &[u32], max_k: u32) -> (u32, u64) {
    if codes.is_empty() {
        return (0, 0);
    }
    let sum: u64 = codes.iter().map(|&u| u64::from(u)).sum();
    let mean = sum / codes.len() as u64;
    let mut k = (u64::BITS - mean.leading_zeros()).min(max_k);
    let largest = codes.iter().copied().max().unwrap_or(0);
    while k < max_k && largest >> k > MAX_QUOTIENT {
        k += 1;
    }
    let mut bits = rice_bits(codes, k);
    // The cost is convex in k: walk down while it falls, then up.
    while k > 0 && rice_fits(codes, k - 1) {
        let lower = rice_bits(codes, k - 1);
        if lower > bits {
            break;
        }
        k -= 1;
        bits = lower;
    }
    while k < max_k {
        let higher = rice_bits(codes, k + 1);
        if higher >= bits {
            break;
        }
        k += 1;
        bits = higher;
    }
    (k, bits)
}

/// The binary width that holds every one of `codes`: 0 for all zeroes,
/// else at least 2 (a width of 1 cannot be coded).
fn binary_width(codes: &[u32]) -> u32 {
    let largest = codes.iter().copied().max().unwrap_or(0);
    if largest == 0 {
        0
    } else {
        bits_for(largest).max(2)
    }
}

/// The cheapest code for parts `a` and `b` under one Rice flag: the flag,
/// both parameters and their bits. `None` for binary where a width does not
/// fit the parameter's field.
fn best_pair(a: &[u32], b: &[u32], parameter_bits: u32, rice: bool) -> Option<(u32, u32, u64)> {
    let max = (1u32 << parameter_bits) - 1;
    if rice {
        let (ka, ba) = best_rice(a, max);
        let (kb, bb) = best_rice(b, max);
        // The quotient bound can leave a run unfittable at the largest k.
        if !rice_fits(a, ka) || !rice_fits(b, kb) {
            return None;
        }
        Some((ka, kb, ba + bb))
    } else {
        let (wa, wb) = (binary_width(a), binary_width(b));
        // A width is coded as itself less one.
        if wa > max + 1 || wb > max + 1 {
            return None;
        }
        Some((
            wa,
            wb,
            a.len() as u64 * u64::from(wa) + b.len() as u64 * u64::from(wb),
        ))
    }
}

/// Bits of a segment's data for one channel under `set`, or `None` when a
/// binary width does not hold the samples.
fn data_bits(codes: &[u32], set: &ParameterSet) -> Option<u64> {
    let (a, b) = codes.split_at(set.part_a_samples.min(codes.len()));
    let part = |run: &[u32], k: u32| {
        if set.rice {
            rice_fits(run, k).then(|| rice_bits(run, k))
        } else if (k == 0 && run.iter().all(|&u| u == 0)) || (k > 0 && binary_width(run) <= k) {
            Some(run.len() as u64 * u64::from(k))
        } else {
            None
        }
    };
    Some(part(a, set.part_a)? + part(b, set.part_b)?)
}

/// The cheapest fresh parameters for a segment of `set`: per channel or
/// shared, with the bits of their header fields and the data.
fn fresh_parameters(
    set: &Set,
    segment: usize,
    segment_samples: usize,
    parameter_bits: u32,
    scratch: &mut (Vec<u32>, Vec<u32>),
) -> (Parameters, u64) {
    let n = set.channels();
    let range = segment * segment_samples..(segment + 1) * segment_samples;
    let part_a = |ch: usize, common: bool| -> usize {
        if segment > 0 {
            0
        } else if common {
            usize::from(*set.band.adaptive_orders.iter().max().unwrap_or(&0))
        } else {
            usize::from(set.band.adaptive_orders[ch])
        }
    };
    let header_bits = |rice: bool, common: bool| -> u64 {
        // Rice flag, hybrid flag (distinct Rice only), part A's parameter
        // (segment 0 only) and part B's.
        1 + u64::from(rice && !common) + u64::from(parameter_bits) * (1 + u64::from(segment == 0))
    };
    // Distinct.
    let mut distinct = Parameters {
        common: false,
        count: n,
        ..Parameters::default()
    };
    let mut distinct_bits = 1u64;
    for ch in 0..n {
        let codes = &set.codes[ch][range.clone()];
        let samples = part_a(ch, false);
        let (a, b) = codes.split_at(samples);
        let mut best: Option<(ParameterSet, u64)> = None;
        for rice in [true, false] {
            if let Some((ka, kb, bits)) = best_pair(a, b, parameter_bits, rice) {
                let bits = bits + header_bits(rice, false);
                if best.is_none_or(|(_, b)| bits < b) {
                    best = Some((
                        ParameterSet {
                            rice,
                            part_a: ka,
                            part_a_samples: samples,
                            part_b: kb,
                        },
                        bits,
                    ));
                }
            }
        }
        let (parameters, bits) = best.expect("a Rice code holds anything");
        distinct.sets[ch] = parameters;
        distinct_bits += bits;
    }
    // Common: every channel's part A and part B under one code. Priced from
    // the concatenated runs, each channel's part A as long as the longest.
    let samples = part_a(0, true);
    let mut common: Option<(Parameters, u64)> = None;
    let (a_all, b_all) = (&mut scratch.0, &mut scratch.1);
    a_all.clear();
    b_all.clear();
    for ch in 0..n {
        let codes = &set.codes[ch][range.clone()];
        a_all.extend_from_slice(&codes[..samples]);
        b_all.extend_from_slice(&codes[samples..]);
    }
    for rice in [true, false] {
        if let Some((ka, kb, bits)) = best_pair(a_all, b_all, parameter_bits, rice) {
            let bits = 1 + bits + header_bits(rice, true);
            if common.as_ref().is_none_or(|(_, b)| bits < *b) {
                let mut parameters = Parameters {
                    common: true,
                    count: 1,
                    ..Parameters::default()
                };
                parameters.sets[0] = ParameterSet {
                    rice,
                    part_a: ka,
                    part_a_samples: samples,
                    part_b: kb,
                };
                common = Some((parameters, bits));
            }
        }
    }
    match common {
        Some((parameters, bits)) if bits < distinct_bits => (parameters, bits),
        _ => (distinct, distinct_bits),
    }
}

/// Bits of a segment of `set` coded under `parameters`, the header fields
/// left out, or `None` when they do not hold it.
fn reused_bits(
    set: &Set,
    segment: usize,
    segment_samples: usize,
    parameters: &Parameters,
) -> Option<u64> {
    if parameters.count == 0 {
        return None;
    }
    let range = segment * segment_samples..(segment + 1) * segment_samples;
    let mut bits = 0;
    for ch in 0..set.channels() {
        let p = parameters.of(ch);
        bits += data_bits(&set.codes[ch][range.clone()], p)?;
    }
    Some(bits)
}

/// Write one segment of `set`, choosing between fresh parameters and the
/// previous segment's (`previous`, which it updates).
fn write_segment(
    w: &mut BitWriter,
    set: &Set,
    segment: usize,
    segment_samples: usize,
    parameter_bits: u32,
    previous: &mut Parameters,
    scratch: &mut (Vec<u32>, Vec<u32>),
) {
    let (fresh, fresh_bits) =
        fresh_parameters(set, segment, segment_samples, parameter_bits, scratch);
    let reuse = segment > 0
        && reused_bits(set, segment, segment_samples, previous)
            .is_some_and(|bits| bits <= fresh_bits);
    if segment > 0 {
        w.put_bit(reuse);
    }
    if !reuse {
        *previous = fresh;
        w.put_bit(previous.common);
        for p in previous.sets() {
            w.put_bit(p.rice);
            if !previous.common && p.rice {
                // No hybrid Rice coding.
                w.put_bit(false);
            }
        }
        let field = |k: u32, rice: bool| if rice || k == 0 { k } else { k - 1 };
        for p in previous.sets() {
            if segment == 0 {
                w.put(parameter_bits, field(p.part_a, p.rice));
            }
            w.put(parameter_bits, field(p.part_b, p.rice));
        }
    }
    let range = segment * segment_samples..(segment + 1) * segment_samples;
    for ch in 0..set.channels() {
        let p = previous.of(ch);
        let codes = &set.codes[ch][range.clone()];
        let (a, b) = codes.split_at(p.part_a_samples.min(codes.len()));
        for (run, k) in [(a, p.part_a), (b, p.part_b)] {
            if p.rice {
                for &u in run {
                    put_rice(w, u, k);
                }
            } else if k > 0 {
                for &u in run {
                    w.put(k, u);
                }
            }
        }
    }
}

fn put_rice(w: &mut BitWriter, u: u32, k: u32) {
    let mut quotient = u >> k;
    while quotient >= 31 {
        w.put(31, 0);
        quotient -= 31;
    }
    // The quotient's zeroes and the stop bit.
    w.put(quotient + 1, 1);
    if k > 0 {
        w.put(k, u & ((1u32 << k) - 1));
    }
}

/// Bits a channel costs as the cheapest fresh codes per segment would code
/// it on its own: the measure the searches compare candidates by.
fn channel_bits(codes: &[u32], part_a: usize, segment_samples: usize, parameter_bits: u32) -> u64 {
    let mut total = 0;
    for (segment, run) in codes.chunks(segment_samples).enumerate() {
        let samples = if segment == 0 { part_a } else { 0 };
        let (a, b) = run.split_at(samples.min(run.len()));
        let header = u64::from(parameter_bits) * (1 + u64::from(segment == 0)) + 2;
        let best = [true, false]
            .into_iter()
            .filter_map(|rice| best_pair(a, b, parameter_bits, rice))
            .map(|(_, _, bits)| bits)
            .min()
            .expect("a Rice code holds anything");
        total += header + best;
    }
    total
}

/// The residual of fixed predictor `order`: the signal differenced `order`
/// times, the first sample of each difference kept as it is.
fn fixed_residual(x: &[i32], order: u8, out: &mut [i32]) {
    out.copy_from_slice(x);
    for _ in 0..order {
        for n in (1..out.len()).rev() {
            out[n] = out[n].wrapping_sub(out[n - 1]);
        }
    }
}

fn zigzag_all(residual: &[i32], codes: &mut [u32]) {
    for (code, &r) in codes.iter_mut().zip(residual) {
        *code = zigzag(r);
    }
}

/// Bits of `x` under its cheapest fixed predictor.
fn fixed_bits(x: &[i32], segment_samples: usize, parameter_bits: u32, search: &mut Search) -> u64 {
    (0..=3)
        .map(|order| {
            fixed_residual(x, order, &mut search.work);
            zigzag_all(&search.work, &mut search.codes);
            channel_bits(&search.codes, 0, segment_samples, parameter_bits)
        })
        .min()
        .expect("four orders")
}

/// Pair the set's channels and choose each pair's coefficient, filling the
/// set's coded channels and its band's decorrelation.
///
/// Each ordered pair is priced by what decorrelating it saves on the second
/// channel under its cheapest fixed predictor, the coefficient the one that
/// minimises the second's energy; the pairing is the one that saves most.
fn decorrelate(set: &mut Set, parameter_bits: u32, segment_samples: usize, search: &mut Search) {
    let n = set.channels();
    if n < 2 {
        set.coded[0].copy_from_slice(&set.samples[0]);
        set.band.decorrelation = None;
        return;
    }
    // gain[a][b]: bits saved predicting b from a, and the coefficient.
    let mut gain = [[(0i64, 0i8); 8]; 8];
    let mut alone = [0i64; 8];
    for (ch, slot) in alone.iter_mut().enumerate().take(n) {
        *slot = fixed_bits(&set.samples[ch], segment_samples, parameter_bits, search) as i64;
    }
    let mut decorrelated = std::mem::take(&mut search.pair);
    for (a, row) in gain.iter_mut().enumerate().take(n) {
        for b in 0..n {
            if a == b {
                continue;
            }
            let (x, y) = (&set.samples[a], &set.samples[b]);
            let energy: f64 = x.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
            if energy == 0.0 {
                continue;
            }
            let cross: f64 = x
                .iter()
                .zip(y)
                .map(|(&u, &v)| f64::from(u) * f64::from(v))
                .sum();
            let c = (8.0 * cross / energy).round().clamp(-64.0, 63.0) as i32;
            if c == 0 {
                continue;
            }
            for ((d, &u), &v) in decorrelated.iter_mut().zip(x).zip(y) {
                *d = v.wrapping_sub(u.wrapping_mul(c).wrapping_add(4) >> 3);
            }
            let bits = fixed_bits(&decorrelated, segment_samples, parameter_bits, search) as i64;
            // The coefficient's own seven bits.
            let saved = alone[b] - bits - 7;
            if saved > 0 {
                row[b] = (saved, c as i8);
            }
        }
    }
    search.pair = decorrelated;
    // The pairing that saves most, over every perfect matching.
    let mut best_order = [0u8; 8];
    let mut best_saved = 0i64;
    let mut order = [0u8; 8];
    let mut used = [false; 8];
    match_pairs(
        &gain,
        n,
        0,
        &mut used,
        &mut order,
        0,
        &mut best_order,
        &mut best_saved,
    );
    if best_saved <= 0 {
        // Pairing saves nothing beyond its own header bits.
        for (coded, samples) in set.coded.iter_mut().zip(&set.samples) {
            coded.copy_from_slice(samples);
        }
        set.band.decorrelation = None;
        return;
    }
    let order = &best_order[..n];
    let decorrelation = set.band.decorrelation.get_or_insert_with(|| Decorrelation {
        order: Vec::with_capacity(n),
        coefficients: Vec::with_capacity(n / 2),
    });
    decorrelation.order.clear();
    decorrelation.order.extend_from_slice(order);
    decorrelation.coefficients.clear();
    for pair in 0..n / 2 {
        let (a, b) = (
            usize::from(order[2 * pair]),
            usize::from(order[2 * pair + 1]),
        );
        let (saved, c) = gain[a][b];
        decorrelation.coefficients.push((saved > 0).then_some(c));
    }
    for (i, &original) in order.iter().enumerate() {
        set.coded[i].copy_from_slice(&set.samples[usize::from(original)]);
    }
    for (pair, coefficient) in decorrelation.coefficients.iter().enumerate() {
        if let Some(c) = *coefficient {
            let c = i32::from(c);
            let (first, second) = set.coded.split_at_mut(2 * pair + 1);
            for (v, &u) in second[0].iter_mut().zip(&first[2 * pair]) {
                *v = v.wrapping_sub(u.wrapping_mul(c).wrapping_add(4) >> 3);
            }
        }
    }
}

/// Search the matchings of the `n` channels into ordered pairs (the odd one
/// out last), keeping the one with the largest total gain.
#[allow(clippy::too_many_arguments)]
fn match_pairs(
    gain: &[[(i64, i8); 8]; 8],
    n: usize,
    at: usize,
    used: &mut [bool; 8],
    order: &mut [u8; 8],
    saved: i64,
    best_order: &mut [u8; 8],
    best_saved: &mut i64,
) {
    let Some(first) = (0..n).find(|&ch| !used[ch]) else {
        if saved > *best_saved {
            *best_saved = saved;
            *best_order = *order;
        }
        return;
    };
    used[first] = true;
    let mut paired = false;
    for second in first + 1..n {
        if used[second] {
            continue;
        }
        paired = true;
        used[second] = true;
        // The basis is whichever direction saves more.
        let (a, b) = if gain[first][second].0 >= gain[second][first].0 {
            (first, second)
        } else {
            (second, first)
        };
        order[at] = a as u8;
        order[at + 1] = b as u8;
        let next = saved + gain[a][b].0.max(0);
        match_pairs(gain, n, at + 2, used, order, next, best_order, best_saved);
        used[second] = false;
    }
    if !paired {
        // The odd channel out.
        order[at] = first as u8;
        if saved > *best_saved {
            *best_saved = saved;
            *best_order = *order;
        }
    }
    used[first] = false;
}

/// Reflection coefficients, by Levinson–Durbin, of `x` windowed: one per
/// order up to `max_order`, in the format's sign (the error is `x[n] +
/// Σ a·x[n − i]`). Fewer when the recursion stops on a signal it has
/// already predicted perfectly.
fn reflection(
    x: &[i32],
    max_order: usize,
    windowed: &mut [f64],
    out: &mut [f64; MAX_ORDER],
) -> usize {
    let len = x.len();
    for (n, (w, &v)) in windowed.iter_mut().zip(x).enumerate() {
        // A sine window: the frame's edges are not where the signal stops.
        let shape = (std::f64::consts::PI * (n as f64 + 0.5) / len as f64).sin();
        *w = f64::from(v) * shape;
    }
    let mut r = [0.0f64; MAX_ORDER + 1];
    for (lag, slot) in r.iter_mut().enumerate().take(max_order + 1) {
        *slot = windowed[lag..]
            .iter()
            .zip(windowed.iter())
            .map(|(a, b)| a * b)
            .sum();
    }
    if r[0] <= 0.0 {
        return 0;
    }
    // A trace of white noise keeps the recursion well conditioned.
    r[0] *= 1.0 + 1e-9;
    let mut a = [0.0f64; MAX_ORDER + 1];
    let mut error = r[0];
    for i in 1..=max_order {
        let mut acc = r[i];
        for j in 1..i {
            acc += a[j] * r[i - j];
        }
        let k = -acc / error;
        if !k.is_finite() || k.abs() >= 1.0 {
            return i - 1;
        }
        let previous = a;
        for j in 1..i {
            a[j] = previous[j] + k * previous[i - j];
        }
        a[i] = k;
        out[i - 1] = k;
        error *= 1.0 - k * k;
        if error <= 0.0 {
            return i;
        }
    }
    max_order
}

/// The table index nearest a reflection coefficient.
fn quantise_reflection(k: f64) -> i8 {
    let target = k.abs() * 65536.0;
    let at = REFLECTION_COEFFS.partition_point(|&v| f64::from(v) < target);
    let index = if at == 0 {
        0
    } else if at >= REFLECTION_COEFFS.len() {
        REFLECTION_COEFFS.len() - 1
    } else if target - f64::from(REFLECTION_COEFFS[at - 1])
        <= f64::from(REFLECTION_COEFFS[at]) - target
    {
        at - 1
    } else {
        at
    };
    let index = index as i8;
    if k < 0.0 { -index } else { index }
}

/// The direct-form coefficients the decoder makes of quantised reflection
/// coefficients (`chs_filter_band_data`), or `None` where an intermediate
/// leaves 32 bits — which the decoders' C and Rust would wrap differently.
fn direct_form(indices: &[i8], coeff: &mut [i32; MAX_ORDER]) -> Option<()> {
    for (j, &index) in indices.iter().enumerate() {
        let magnitude = i64::from(REFLECTION_COEFFS[usize::from(index.unsigned_abs())]);
        let rc = if index < 0 { -magnitude } else { magnitude };
        for k in 0..j.div_ceil(2) {
            let (t1, t2) = (i64::from(coeff[k]), i64::from(coeff[j - k - 1]));
            let a = t1 + ((rc * t2 + (1 << 15)) >> 16);
            let b = t2 + ((rc * t1 + (1 << 15)) >> 16);
            coeff[k] = i32::try_from(a).ok()?;
            coeff[j - k - 1] = i32::try_from(b).ok()?;
        }
        coeff[j] = rc as i32;
    }
    Some(())
}

/// The residual of an adaptive predictor: the first `order` samples as they
/// are, then each sample plus the clipped prediction the decoder will
/// subtract. `None` where a prediction leaves 32 bits before its clip.
fn adaptive_residual(x: &[i32], coeff: &[i32], out: &mut [i32]) -> Option<()> {
    let order = coeff.len();
    out[..order].copy_from_slice(&x[..order]);
    for n in order..x.len() {
        let mut sum = 0i64;
        for k in 0..order {
            sum += i64::from(x[n - order + k]) * i64::from(coeff[order - k - 1]);
        }
        let prediction = i32::try_from((sum + (1 << 15)) >> 16).ok()?;
        out[n] = x[n].wrapping_add(clip23(prediction));
    }
    Some(())
}

/// The cheapest predictor for `x`, fixed or adaptive: its bits — the
/// samples' codes and the predictor's own fields — its adaptive and fixed
/// orders, and its reflection coefficient indices. Its residual is left in
/// `search.best`.
fn search_prediction(
    x: &[i32],
    parameter_bits: u32,
    segment_samples: usize,
    search: &mut Search,
) -> (u64, (u8, u8), [i8; MAX_ORDER]) {
    let mut best_bits = u64::MAX;
    let mut best = (0u8, 0u8);
    for order in 0..=3u8 {
        fixed_residual(x, order, &mut search.work);
        zigzag_all(&search.work, &mut search.codes);
        let bits = channel_bits(&search.codes, 0, segment_samples, parameter_bits) + 2;
        if bits < best_bits {
            best_bits = bits;
            best = (0, order);
            search.best.copy_from_slice(&search.work);
        }
    }
    let max_order = MAX_ORDER.min(segment_samples);
    let mut k = [0.0f64; MAX_ORDER];
    let fitted = reflection(x, max_order, &mut search.windowed, &mut k);
    let mut indices = [0i8; MAX_ORDER];
    for (index, &k) in indices.iter_mut().zip(&k[..fitted]) {
        *index = quantise_reflection(k);
    }
    for order in 1..=fitted {
        let mut coeff = [0i32; MAX_ORDER];
        if direct_form(&indices[..order], &mut coeff).is_none() {
            break;
        }
        if adaptive_residual(x, &coeff[..order], &mut search.work).is_none() {
            continue;
        }
        zigzag_all(&search.work, &mut search.codes);
        let bits =
            channel_bits(&search.codes, order, segment_samples, parameter_bits) + 8 * order as u64;
        if bits < best_bits {
            best_bits = bits;
            best = (order as u8, 0);
            search.best.copy_from_slice(&search.work);
        }
    }
    (best_bits, best, indices)
}

/// Choose coded channel `ch`'s predictor and leave its residual, zigzagged,
/// in the set.
fn predict(
    set: &mut Set,
    ch: usize,
    parameter_bits: u32,
    segment_samples: usize,
    search: &mut Search,
) {
    let (_, (adaptive, fixed), indices) =
        search_prediction(&set.coded[ch], parameter_bits, segment_samples, search);
    set.band.adaptive_orders[ch] = adaptive;
    set.band.fixed_orders[ch] = fixed;
    set.band.reflection[ch].clear();
    set.band.reflection[ch].extend_from_slice(&indices[..usize::from(adaptive)]);
    set.residual[ch].copy_from_slice(&search.best);
    zigzag_all(&set.residual[ch], &mut set.codes[ch]);
}

#[cfg(test)]
mod tests;
