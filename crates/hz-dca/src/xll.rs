// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/dca_xll.c — Copyright (C) 2016 foo86
//   libavcodec/dcadata.c — Copyright (C) 2004 Gildas Bazin, (c) 2006 Benjamin Larsson
// What was taken and what was changed is recorded in docs/provenance.md.

//! The lossless extension's frame headers.
//!
//! ETSI TS 102 114 §8.3. A lossless (XLL) frame is a common header, one
//! sub-header per channel set, a navigation table giving the size of every
//! channel set's data in every segment, and then that data. Every header
//! closes on a CRC16 after a byte alignment, and the common header and each
//! sub-header give their own length, so a reader can skip what it does not
//! understand — which, as for the extension substream, is held here as raw
//! [`Bits`] rather than dropped.
//!
//! The grammar is FFmpeg's reader (`dca_xll.c`), which is what streams are
//! decoded by, with the fields FFmpeg refuses — replacement sets, speaker
//! positions in place of a mask, more than two frequency bands — read as the
//! specification lays them out, so that a stream using them still reads and
//! rewrites. [`Headers::write`] writes the sizes and widths the model carries
//! and refuses one whose content does not fill them; [`Headers::fit`] sets the
//! sizes from the content. The checksums are always computed.
//!
//! The data itself — segments of entropy-coded residuals — is the encoder's
//! ([`crate::lossless`]).

use crate::bits::Bits;
use crate::crc;
use crate::exss::SAMPLE_RATES;
use crate::fields::{Out, get_wide};
use hz_core::bits_read::{BitReader, Ran};

type Result<T> = std::result::Result<T, Ran>;

/// The lossless frame's sync word.
pub const SYNC: u32 = 0x41a2_9547;

/// Channels a primary channel set downmixes to, by downmix type
/// (`ff_dca_dmix_primary_nch`): 1/0, Lo/Ro, Lt/Rt, 3/0, 2/1, 2/2, 3/1.
pub const PRIMARY_DOWNMIX_CHANNELS: [usize; 8] = [1, 2, 2, 3, 3, 4, 4, 0];

/// A frame's common header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonHeader {
    /// `nVersion` as coded: the syntax version less one, 0 for every stream
    /// a decoder takes.
    pub version: u8,
    /// Bytes from the sync word to the checksum inclusive.
    pub header_size: u32,
    /// Width of the frame size field, 1..=32.
    pub frame_size_bits: u8,
    /// Bytes in the whole lossless frame.
    pub frame_size: u32,
    /// Channel sets in the frame, 1..=16.
    pub channel_sets: u8,
    /// `log2` of the segments in the frame.
    pub segments_log2: u8,
    /// `log2` of the samples in a segment of a frequency band.
    pub segment_samples_log2: u8,
    /// Width of each navigation table entry and of an LSB section size,
    /// 1..=32.
    pub segment_size_bits: u8,
    /// `nBandDataCRCEn`: where checksums sit in the band data, 0 for none.
    pub band_crc: u8,
    /// `nuFixedLSBWidth`, present exactly when the MSB/LSB split
    /// (`bScalableLSBs`) is on.
    pub fixed_lsb_width: Option<u8>,
    /// Width of each channel set's speaker mask, 1..=32.
    pub channel_mask_bits: u8,
    /// Reserved bits and the byte alignment before the checksum.
    pub reserved: Bits,
    /// The checksum as read; written ones are computed.
    pub crc: u16,
}

/// A channel set's sub-header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSet {
    /// Bytes in the sub-header, checksum included.
    pub size: u32,
    /// Channels in the set, 1..=16.
    pub channels: u8,
    /// `nResidualChEncode`: one bit per channel, set where the channel is
    /// coded as itself rather than as a residual against the core.
    pub residual_encode: u16,
    /// `nBitResolution`: the source's bits per sample, 1..=32.
    pub pcm_bit_res: u8,
    /// `nBitWidth`: the storage width, 1..=32 (16, 20 or 24 in practice).
    pub storage_bit_res: u8,
    /// `sFreqIndex`, into [`SAMPLE_RATES`].
    pub frequency_code: u8,
    /// `nFsInterpolate`.
    pub interpolation: u8,
    /// `nReplacementSet` (1..=3) and `bActiveReplaceSet`, when the set is a
    /// replacement.
    pub replacement: Option<(u8, bool)>,
    pub mapping: Mapping,
    /// `bXtraFreqBands`, present exactly when the rate is over 96 kHz.
    pub extra_bands: Option<bool>,
    /// One per frequency band.
    pub bands: Vec<Band>,
    /// Reserved bits and the byte alignment before the checksum.
    pub reserved: Bits,
    pub crc: u16,
}

/// How a channel set's channels reach loudspeakers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapping {
    /// The asset maps channels one to one onto loudspeakers.
    Speakers(Speakers),
    /// It does not (`bOne2OneMapChannels2Speakers` false), and the set
    /// carries no mapping coefficients — Lt/Rt or Lh/Rh. Mapping
    /// coefficients are not modelled.
    Unmapped,
}

/// A one-to-one channel set's place in the hierarchy and its speakers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Speakers {
    /// `bPrimaryChSet`.
    pub primary: bool,
    /// The downmix coefficients, when the stream carries them.
    pub downmix: Option<Downmix>,
    /// `bHierChSet`.
    pub hierarchical: bool,
    pub positions: Positions,
}

/// Which loudspeakers a channel set feeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Positions {
    /// `nChMask`, [`CommonHeader::channel_mask_bits`] wide: the set's
    /// channels in ascending bit order.
    Mask(u32),
    /// Per channel: radius delta (9 bits), theta (9) and phi (7).
    Polar(Vec<(u16, u16, u8)>),
}

/// A channel set's downmix matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downmix {
    /// `bDownmixEmbedded`: the encoder has already mixed this set into the
    /// sets below it, and a decoder of every set undoes that.
    pub embedded: bool,
    /// `nLLDownmixType`, present exactly when the set is primary.
    pub kind: Option<u8>,
    /// The nine-bit codes, in stream order. A primary set has one row per
    /// downmix channel ([`PRIMARY_DOWNMIX_CHANNELS`]), a coefficient per
    /// channel of the set; any other set has one row per channel of the sets
    /// below it in the hierarchy, a scale and then a coefficient per channel.
    /// See [`coefficient`] and [`scale`].
    pub codes: Vec<u16>,
}

/// One frequency band's coding parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// Pairwise decorrelation, when enabled.
    pub decorrelation: Option<Decorrelation>,
    /// Per channel, 0..=15; 0 is no adaptive prediction.
    pub adaptive_orders: Vec<u8>,
    /// Per channel, 0..=3; coded only where the adaptive order is 0, and 0
    /// elsewhere.
    pub fixed_orders: Vec<u8>,
    /// Per channel, one quantised reflection coefficient index per order,
    /// −127..=127, into [`REFLECTION_COEFFS`].
    pub reflection: Vec<Vec<i8>>,
    /// `bEmbDMixInExtBand`: extension bands only, when the set's downmix is
    /// embedded.
    pub embedded_downmix: Option<bool>,
    /// The LSB section: present in band 0 exactly when the MSB/LSB split is
    /// on, and in an extension band when its own flag says so.
    pub lsb: Option<Lsb>,
    /// `pnBitWidthAdjPerCh`, per channel: present in band 0 exactly when the
    /// split is on, in an extension band when its flag says so.
    pub width_adjust: Option<Vec<u8>>,
}

/// Pairwise channel decorrelation in a band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decorrelation {
    /// `nOrigChanOrder`: the original channel each coded channel is. Empty
    /// for a one-channel set, which codes the flag and nothing after it.
    pub order: Vec<u8>,
    /// Per pair of coded channels, the coefficient (−64..=63, in eighths)
    /// the second is predicted from the first with, when flagged.
    pub coefficients: Vec<Option<i8>>,
}

/// The size of a band's LSB section and its channels' widths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lsb {
    /// `nLSBFsize`: bytes in every segment's LSB section.
    pub size: u32,
    /// `pnScalableLSBs`, per channel, 0..=15.
    pub widths: Vec<u8>,
}

/// The navigation table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Navigation {
    /// Bytes of each channel set's data, band by band, segment by segment,
    /// channel set by channel set — the sets that have the band.
    pub sizes: Vec<u32>,
    /// The byte alignment before the checksum.
    pub padding: Bits,
    pub crc: u16,
}

/// Everything before a lossless frame's band data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headers {
    pub common: CommonHeader,
    pub channel_sets: Vec<ChannelSet>,
    pub navigation: Navigation,
}

/// Whether each checksum stored in a frame holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checksums {
    pub common: bool,
    /// All of the channel set sub-headers.
    pub channel_sets: bool,
    pub navigation: bool,
}

impl Checksums {
    pub fn all(&self) -> bool {
        self.common && self.channel_sets && self.navigation
    }
}

/// A downmix coefficient's value in Q15, from its nine-bit code: the high
/// bit set for positive, the low eight an index into [`DMIX_TABLE`]. `None`
/// for an index past the table.
pub fn coefficient(code: u16) -> Option<i32> {
    let magnitude = i32::from(*DMIX_TABLE.get(usize::from(code & 0xff))?);
    Some(if code & 0x100 != 0 {
        magnitude
    } else {
        -magnitude
    })
}

/// A downmix scale in Q15 and its inverse in Q16, from its nine-bit code.
/// `None` for an index outside the scales' range (−40 dB to 0 dB).
pub fn scale(code: u16) -> Option<(i32, i32)> {
    let index = usize::from(code & 0xff).checked_sub(DMIX_TABLE.len() - INV_DMIX_TABLE.len())?;
    let inverse = i32::try_from(*INV_DMIX_TABLE.get(index)?).ok()?;
    let magnitude = i32::from(DMIX_TABLE[index + DMIX_TABLE.len() - INV_DMIX_TABLE.len()]);
    Some(if code & 0x100 != 0 {
        (magnitude, inverse)
    } else {
        (-magnitude, -inverse)
    })
}

impl CommonHeader {
    /// Segments in the frame.
    pub fn segments(&self) -> usize {
        1 << self.segments_log2
    }

    /// Samples in a segment of a frequency band.
    pub fn segment_samples(&self) -> usize {
        1 << self.segment_samples_log2
    }

    /// Samples in the frame per frequency band.
    pub fn frame_samples(&self) -> usize {
        self.segments() * self.segment_samples()
    }

    pub fn read(frame: &[u8]) -> Result<Self> {
        let mut r = BitReader::new(frame);
        let sync = r.get(32)?;
        if sync != SYNC {
            return Err(Ran(format!(
                "{sync:#010x} is not the lossless frame sync word"
            )));
        }
        let version = r.get(4)? as u8;
        let header_size = r.get(8)? + 1;
        let frame_size_bits = r.get(5)? as u8 + 1;
        let frame_size = get_wide(&mut r, u32::from(frame_size_bits))? as u32 + 1;
        let channel_sets = r.get(4)? as u8 + 1;
        let segments_log2 = r.get(4)? as u8;
        let segment_samples_log2 = r.get(4)? as u8;
        let segment_size_bits = r.get(5)? as u8 + 1;
        let band_crc = r.get(2)? as u8;
        let scalable = r.get_bit()?;
        let channel_mask_bits = r.get(5)? as u8 + 1;
        let fixed_lsb_width = if scalable {
            Some(r.get(4)? as u8)
        } else {
            None
        };
        let (reserved, crc) = close(&mut r, 0, header_size, "the common header")?;
        Ok(Self {
            version,
            header_size,
            frame_size_bits,
            frame_size,
            channel_sets,
            segments_log2,
            segment_samples_log2,
            segment_size_bits,
            band_crc,
            fixed_lsb_width,
            channel_mask_bits,
            reserved,
            crc,
        })
    }

    fn write_fields(&self, out: &mut Out) {
        out.field(32, SYNC, "the sync word");
        out.field(4, self.version, "the version");
        out.size(8, self.header_size, "the common header size");
        out.size(5, self.frame_size_bits, "the frame size width");
        out.size(
            u32::from(self.frame_size_bits),
            self.frame_size,
            "the frame size",
        );
        out.size(4, self.channel_sets, "the channel set count");
        out.field(4, self.segments_log2, "the segment count");
        out.field(4, self.segment_samples_log2, "the segment length");
        out.size(5, self.segment_size_bits, "the segment size width");
        out.field(2, self.band_crc, "the band checksum mode");
        out.bit(self.fixed_lsb_width.is_some());
        out.size(5, self.channel_mask_bits, "the channel mask width");
        if let Some(width) = self.fixed_lsb_width {
            out.field(4, width, "the fixed LSB width");
        }
    }
}

/// Read the reserved bits up to the checksum of a part that started at bit
/// `start` and is `size` bytes long, and the checksum.
fn close(r: &mut BitReader, start: usize, size: u32, what: &str) -> Result<(Bits, u16)> {
    let crc_at = (start + size as usize * 8).checked_sub(16);
    let here = r.bit_position();
    match crc_at {
        Some(crc_at) if crc_at >= here => {
            let reserved = Bits::read(r, crc_at - here)?;
            Ok((reserved, r.get(16)? as u16))
        }
        _ => Err(Ran(format!(
            "{what}'s fields run past its {size}-byte length"
        ))),
    }
}

/// What a channel set's sub-header depends on outside itself.
#[derive(Debug, Clone, Copy)]
struct Context {
    one_to_one: bool,
    channel_mask_bits: u32,
    segment_size_bits: u32,
    scalable: bool,
    /// Channels of the hierarchical sets before this one: the rows of a
    /// non-primary set's downmix matrix.
    below: usize,
}

impl ChannelSet {
    /// The set's sample rate.
    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATES[usize::from(self.frequency_code & 15)]
    }

    /// Frequency bands the set is coded in.
    pub fn band_count(&self) -> usize {
        match self.extra_bands {
            None => 1,
            Some(false) => 2,
            Some(true) => 4,
        }
    }

    /// Whether the set is part of the hierarchy (a set without a speaker
    /// mapping always is).
    pub fn hierarchical(&self) -> bool {
        match &self.mapping {
            Mapping::Speakers(s) => s.hierarchical,
            Mapping::Unmapped => true,
        }
    }

    /// The set's speaker mask, when it has one.
    pub fn mask(&self) -> Option<u32> {
        match &self.mapping {
            Mapping::Speakers(Speakers {
                positions: Positions::Mask(mask),
                ..
            }) => Some(*mask),
            _ => None,
        }
    }

    /// Whether every channel is coded as itself.
    pub fn full_coded(&self) -> bool {
        u32::from(self.residual_encode) == (1u32 << self.channels) - 1
    }

    fn downmix_codes(&self, primary: bool, kind: Option<u8>, below: usize) -> usize {
        let channels = usize::from(self.channels);
        if primary {
            kind.map_or(0, |k| PRIMARY_DOWNMIX_CHANNELS[usize::from(k & 7)]) * channels
        } else {
            below * (channels + 1)
        }
    }

    fn read(r: &mut BitReader, context: Context) -> Result<Self> {
        let start = r.bit_position();
        let size = r.get(10)? + 1;
        let channels = r.get(4)? as u8 + 1;
        let residual_encode = r.get(u32::from(channels))? as u16;
        let pcm_bit_res = r.get(5)? as u8 + 1;
        let storage_bit_res = r.get(5)? as u8 + 1;
        let frequency_code = r.get(4)? as u8;
        let interpolation = r.get(2)? as u8;
        let replacement_set = r.get(2)? as u8;
        let replacement = if replacement_set > 0 {
            Some((replacement_set, r.get_bit()?))
        } else {
            None
        };
        let mut set = Self {
            size,
            channels,
            residual_encode,
            pcm_bit_res,
            storage_bit_res,
            frequency_code,
            interpolation,
            replacement,
            mapping: Mapping::Unmapped,
            extra_bands: None,
            bands: Vec::new(),
            reserved: Bits::new(),
            crc: 0,
        };
        let n = usize::from(channels);
        set.mapping = if context.one_to_one {
            let primary = r.get_bit()?;
            let coefficients = r.get_bit()?;
            let embedded = coefficients && r.get_bit()?;
            let kind = if coefficients && primary {
                Some(r.get(3)? as u8)
            } else {
                None
            };
            let hierarchical = r.get_bit()?;
            let downmix = if coefficients {
                let count = set.downmix_codes(primary, kind, context.below);
                let mut codes = Vec::with_capacity(count);
                for _ in 0..count {
                    codes.push(r.get(9)? as u16);
                }
                Some(Downmix {
                    embedded,
                    kind,
                    codes,
                })
            } else {
                None
            };
            let positions = if r.get_bit()? {
                Positions::Mask(r.get(context.channel_mask_bits)?)
            } else {
                let mut polar = Vec::with_capacity(n);
                for _ in 0..n {
                    polar.push((r.get(9)? as u16, r.get(9)? as u16, r.get(7)? as u8));
                }
                Positions::Polar(polar)
            };
            Mapping::Speakers(Speakers {
                primary,
                downmix,
                hierarchical,
                positions,
            })
        } else {
            if r.get_bit()? {
                return Err(Ran(
                    "channel-to-speaker mapping coefficients are not modelled".into(),
                ));
            }
            Mapping::Unmapped
        };
        if set.sample_rate() > 96000 {
            set.extra_bands = Some(r.get_bit()?);
        }
        let embedded = matches!(
            &set.mapping,
            Mapping::Speakers(Speakers {
                downmix: Some(Downmix { embedded: true, .. }),
                ..
            })
        );
        let order_bits = ceil_log2(n);
        for band in 0..set.band_count() {
            let decorrelation = if r.get_bit()? {
                let mut order = Vec::new();
                let mut coefficients = Vec::new();
                if n > 1 {
                    for _ in 0..n {
                        order.push(r.get(order_bits)? as u8);
                    }
                    for _ in 0..n / 2 {
                        coefficients.push(if r.get_bit()? {
                            Some(unzigzag(r.get(7)?) as i8)
                        } else {
                            None
                        });
                    }
                }
                Some(Decorrelation {
                    order,
                    coefficients,
                })
            } else {
                None
            };
            let mut adaptive_orders = Vec::with_capacity(n);
            for _ in 0..n {
                adaptive_orders.push(r.get(4)? as u8);
            }
            let mut fixed_orders = Vec::with_capacity(n);
            for &adaptive in &adaptive_orders {
                fixed_orders.push(if adaptive == 0 { r.get(2)? as u8 } else { 0 });
            }
            let mut reflection = Vec::with_capacity(n);
            for &adaptive in &adaptive_orders {
                let mut coefficients = Vec::with_capacity(usize::from(adaptive));
                for _ in 0..adaptive {
                    coefficients.push(unzigzag(r.get(8)?) as i8);
                }
                reflection.push(coefficients);
            }
            let embedded_downmix = if band > 0 && embedded {
                Some(r.get_bit()?)
            } else {
                None
            };
            let split = if band == 0 {
                context.scalable
            } else {
                r.get_bit()?
            };
            let lsb = if split {
                let size = r.get(context.segment_size_bits)?;
                let mut widths = Vec::with_capacity(n);
                for _ in 0..n {
                    widths.push(r.get(4)? as u8);
                }
                Some(Lsb { size, widths })
            } else {
                None
            };
            let adjusted = if band == 0 {
                context.scalable
            } else {
                r.get_bit()?
            };
            let width_adjust = if adjusted {
                let mut widths = Vec::with_capacity(n);
                for _ in 0..n {
                    widths.push(r.get(4)? as u8);
                }
                Some(widths)
            } else {
                None
            };
            set.bands.push(Band {
                decorrelation,
                adaptive_orders,
                fixed_orders,
                reflection,
                embedded_downmix,
                lsb,
                width_adjust,
            });
        }
        let (reserved, crc) = close(r, start, size, "a channel set sub-header")?;
        set.reserved = reserved;
        set.crc = crc;
        Ok(set)
    }

    fn write_fields(&self, out: &mut Out, context: Context) {
        let n = usize::from(self.channels);
        out.size(10, self.size, "a channel set sub-header size");
        out.size(4, self.channels, "a channel set's channel count");
        out.field(
            u32::from(self.channels),
            self.residual_encode,
            "the residual coding mask",
        );
        out.size(5, self.pcm_bit_res, "the PCM resolution");
        out.size(5, self.storage_bit_res, "the storage width");
        out.field(4, self.frequency_code, "the sample rate code");
        out.field(2, self.interpolation, "the interpolation code");
        match self.replacement {
            Some((group, active)) => {
                out.check((1..=3).contains(&group), || {
                    format!("replacement group {group} is not 1..=3")
                });
                out.field(2, group, "the replacement group");
                out.bit(active);
            }
            None => out.field(2, 0u8, "the replacement group"),
        }
        let mut embedded = false;
        match (&self.mapping, context.one_to_one) {
            (Mapping::Speakers(s), true) => {
                out.bit(s.primary);
                out.bit(s.downmix.is_some());
                if let Some(d) = &s.downmix {
                    out.bit(d.embedded);
                    embedded = d.embedded;
                    out.check(d.kind.is_some() == s.primary, || {
                        "a downmix type is coded exactly for a primary set".into()
                    });
                    if let Some(kind) = d.kind {
                        out.field(3, kind, "the downmix type");
                    }
                }
                out.bit(s.hierarchical);
                if let Some(d) = &s.downmix {
                    let count = self.downmix_codes(s.primary, d.kind, context.below);
                    out.check(d.codes.len() == count, || {
                        format!(
                            "{} downmix codes where the matrix has {count}",
                            d.codes.len()
                        )
                    });
                    for &code in &d.codes {
                        out.field(9, code, "a downmix code");
                    }
                }
                match &s.positions {
                    Positions::Mask(mask) => {
                        out.bit(true);
                        out.field(context.channel_mask_bits, *mask, "the channel mask");
                    }
                    Positions::Polar(polar) => {
                        out.bit(false);
                        out.check(polar.len() == n, || {
                            "one speaker position per channel".into()
                        });
                        for &(radius, theta, phi) in polar {
                            out.field(9, radius, "a speaker radius");
                            out.field(9, theta, "a speaker azimuth");
                            out.field(7, phi, "a speaker elevation");
                        }
                    }
                }
            }
            (Mapping::Unmapped, false) => out.bit(false),
            _ => out.refuse(|| {
                "a channel set's mapping disagrees with the asset's one-to-one flag".into()
            }),
        }
        out.check(
            (self.sample_rate() > 96000) == self.extra_bands.is_some(),
            || "the extra band flag is coded exactly over 96 kHz".into(),
        );
        if let Some(extra) = self.extra_bands {
            out.bit(extra);
        }
        out.check(self.bands.len() == self.band_count(), || {
            format!(
                "{} bands where the set has {}",
                self.bands.len(),
                self.band_count()
            )
        });
        let order_bits = ceil_log2(n);
        for (index, band) in self.bands.iter().enumerate() {
            out.bit(band.decorrelation.is_some());
            if let Some(d) = &band.decorrelation {
                let (orders, pairs) = if n > 1 { (n, n / 2) } else { (0, 0) };
                out.check(
                    d.order.len() == orders && d.coefficients.len() == pairs,
                    || "a decorrelation order per channel and a coefficient per pair".into(),
                );
                for &channel in &d.order {
                    out.field(order_bits, channel, "an original channel");
                }
                for coefficient in &d.coefficients {
                    out.option(coefficient, |out, &c| {
                        out.check((-64..=63).contains(&c), || {
                            format!("a pair coefficient of {c}")
                        });
                        out.field(7, zigzag(i32::from(c)), "a pair coefficient");
                    });
                }
            }
            out.check(
                band.adaptive_orders.len() == n
                    && band.fixed_orders.len() == n
                    && band.reflection.len() == n,
                || "the predictor orders and coefficients, one entry per channel".into(),
            );
            for &order in &band.adaptive_orders {
                out.field(4, order, "an adaptive predictor order");
            }
            for (&adaptive, &fixed) in band.adaptive_orders.iter().zip(&band.fixed_orders) {
                if adaptive == 0 {
                    out.field(2, fixed, "a fixed predictor order");
                } else {
                    out.check(fixed == 0, || "a fixed order beside an adaptive one".into());
                }
            }
            for (&adaptive, coefficients) in band.adaptive_orders.iter().zip(&band.reflection) {
                out.check(coefficients.len() == usize::from(adaptive), || {
                    "a reflection coefficient per adaptive order".into()
                });
                for &k in coefficients {
                    out.field(8, zigzag(i32::from(k)), "a reflection coefficient");
                }
            }
            out.check(
                band.embedded_downmix.is_some() == (index > 0 && embedded),
                || {
                    "the band downmix flag is coded exactly in an extension band of an \
                 embedded downmix"
                        .into()
                },
            );
            if let Some(flag) = band.embedded_downmix {
                out.bit(flag);
            }
            if index == 0 {
                out.check(
                    band.lsb.is_some() == context.scalable
                        && band.width_adjust.is_some() == context.scalable,
                    || "band 0's LSB fields are coded exactly when the split is on".into(),
                );
            } else {
                out.bit(band.lsb.is_some());
            }
            if let Some(lsb) = &band.lsb {
                out.field(context.segment_size_bits, lsb.size, "an LSB section size");
                out.check(lsb.widths.len() == n, || "an LSB width per channel".into());
                for &width in &lsb.widths {
                    out.field(4, width, "an LSB width");
                }
            }
            if index > 0 {
                out.bit(band.width_adjust.is_some());
            }
            if let Some(adjust) = &band.width_adjust {
                out.check(adjust.len() == n, || {
                    "a width adjustment per channel".into()
                });
                for &a in adjust {
                    out.field(4, a, "a width adjustment");
                }
            }
        }
    }
}

impl Headers {
    /// Read the headers of the lossless frame starting at `frame`.
    /// `one_to_one` is the asset's `bOne2OneMapChannels2Speakers`, which the
    /// channel set sub-headers depend on.
    pub fn read(frame: &[u8], one_to_one: bool) -> Result<Self> {
        let common = CommonHeader::read(frame)?;
        let mut r = BitReader::at_bit(frame, common.header_size as usize * 8);
        let mut channel_sets: Vec<ChannelSet> = Vec::with_capacity(common.channel_sets.into());
        for _ in 0..common.channel_sets {
            let context = common.context(one_to_one, &channel_sets);
            let set = ChannelSet::read(&mut r, context)?;
            channel_sets.push(set);
        }
        let entries = navigation_entries(&common, &channel_sets);
        let mut sizes = Vec::with_capacity(entries);
        for _ in 0..entries {
            sizes.push(get_wide(&mut r, u32::from(common.segment_size_bits))? as u32 + 1);
        }
        let align = (8 - r.bit_position() % 8) % 8;
        let padding = Bits::read(&mut r, align)?;
        let crc = r.get(16)? as u16;
        Ok(Self {
            common,
            channel_sets,
            navigation: Navigation {
                sizes,
                padding,
                crc,
            },
        })
    }

    /// Bytes from the frame's start to its band data.
    pub fn len(&self) -> usize {
        self.common.header_size as usize
            + self
                .channel_sets
                .iter()
                .map(|c| c.size as usize)
                .sum::<usize>()
            + self.navigation_len()
    }

    /// The headers are never empty; for the lint.
    pub fn is_empty(&self) -> bool {
        false
    }

    fn navigation_len(&self) -> usize {
        let bits = self.navigation.sizes.len() * usize::from(self.common.segment_size_bits)
            + self.navigation.padding.len();
        bits / 8 + 2
    }

    /// Frequency bands in the frame: the most any set has.
    pub fn band_count(&self) -> usize {
        self.channel_sets
            .iter()
            .map(ChannelSet::band_count)
            .max()
            .unwrap_or(0)
    }

    /// Bytes of band data the navigation table describes.
    pub fn band_data_len(&self) -> usize {
        self.navigation.sizes.iter().map(|&s| s as usize).sum()
    }

    /// Whether the checksums stored in `frame`, whose headers these are,
    /// hold.
    pub fn checksums(&self, frame: &[u8]) -> Checksums {
        let common = self.common.header_size as usize;
        let mut at = common;
        let mut sets = true;
        for set in &self.channel_sets {
            let end = at + set.size as usize;
            sets &= frame.get(at..end).is_some_and(crc::check);
            at = end;
        }
        Checksums {
            common: frame.get(4..common).is_some_and(crc::check),
            channel_sets: sets,
            navigation: frame
                .get(at..at + self.navigation_len())
                .is_some_and(crc::check),
        }
    }

    /// Write the headers, every checksum computed over what was written.
    ///
    /// Refuses a model whose fields do not fill a part's stated size, whose
    /// navigation table does not have an entry per set per segment per band,
    /// whose channel set count disagrees with its sets, or that holds a value
    /// its field cannot.
    pub fn write(&self, one_to_one: bool) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(self.len());
        if usize::from(self.common.channel_sets) != self.channel_sets.len() {
            return Err(Ran(format!(
                "{} channel sets declared, {} described",
                self.common.channel_sets,
                self.channel_sets.len()
            )));
        }
        let mut out = Out::new();
        self.common.write_fields(&mut out);
        out.bits(&self.common.reserved);
        bytes.extend(seal(out, 4, self.common.header_size, "the common header")?);
        for (index, set) in self.channel_sets.iter().enumerate() {
            let context = self.common.context(one_to_one, &self.channel_sets[..index]);
            let mut out = Out::new();
            set.write_fields(&mut out, context);
            out.bits(&set.reserved);
            bytes.extend(seal(out, 0, set.size, "a channel set sub-header")?);
        }
        let entries = navigation_entries(&self.common, &self.channel_sets);
        let mut out = Out::new();
        out.check(self.navigation.sizes.len() == entries, || {
            format!(
                "{} navigation entries where the frame has {entries}",
                self.navigation.sizes.len()
            )
        });
        for &size in &self.navigation.sizes {
            out.size(
                u32::from(self.common.segment_size_bits),
                size,
                "a navigation entry",
            );
        }
        out.bits(&self.navigation.padding);
        let size = (out.position() / 8 + 2) as u32;
        bytes.extend(seal(out, 0, size, "the navigation table")?);
        Ok(bytes)
    }

    /// Set the common header's and each sub-header's size, and the
    /// navigation table's padding, from the content: the reserved bits are
    /// kept and padded with zeroes to the byte before the checksum, so
    /// fitting twice changes nothing. The field widths, the frame size and
    /// the navigation entries are the caller's. On a refusal the model is
    /// left as it was.
    pub fn fit(&mut self, one_to_one: bool) -> Result<()> {
        let original = self.clone();
        let result = self.fit_parts(one_to_one);
        if result.is_err() {
            *self = original;
        }
        result
    }

    fn fit_parts(&mut self, one_to_one: bool) -> Result<()> {
        self.common.header_size = 1;
        let mut out = Out::new();
        self.common.write_fields(&mut out);
        let used = out.position();
        out.finish()?;
        let bytes = pad_to_byte(&mut self.common.reserved, used);
        self.common.header_size = bytes + 2;
        if self.common.header_size > 256 {
            return Err(Ran(format!(
                "a {}-byte common header does not fit its size field",
                self.common.header_size
            )));
        }
        for index in 0..self.channel_sets.len() {
            let context = self.common.context(one_to_one, &self.channel_sets[..index]);
            let set = &mut self.channel_sets[index];
            set.size = 1;
            let mut out = Out::new();
            set.write_fields(&mut out, context);
            let used = out.position();
            out.finish()?;
            let bytes = pad_to_byte(&mut set.reserved, used);
            set.size = bytes + 2;
            if set.size > 1024 {
                return Err(Ran(format!(
                    "a {}-byte channel set sub-header does not fit its size field",
                    set.size
                )));
            }
        }
        let used = self.navigation.sizes.len() * usize::from(self.common.segment_size_bits);
        self.navigation.padding = Bits::zeroes((8 - used % 8) % 8);
        Ok(())
    }
}

impl CommonHeader {
    fn context(&self, one_to_one: bool, before: &[ChannelSet]) -> Context {
        Context {
            one_to_one,
            channel_mask_bits: u32::from(self.channel_mask_bits),
            segment_size_bits: u32::from(self.segment_size_bits),
            scalable: self.fixed_lsb_width.is_some(),
            below: before
                .iter()
                .filter(|c| c.hierarchical())
                .map(|c| usize::from(c.channels))
                .sum(),
        }
    }
}

/// Entries in the navigation table: one per set per segment in each band
/// the set has.
fn navigation_entries(common: &CommonHeader, sets: &[ChannelSet]) -> usize {
    common.segments() * sets.iter().map(ChannelSet::band_count).sum::<usize>()
}

/// Pad `reserved` so that, after `used` bits of fields, it ends on a byte;
/// the bytes the fields and it then take.
fn pad_to_byte(reserved: &mut Bits, used: usize) -> u32 {
    let bytes = (used + reserved.len()).div_ceil(8);
    reserved.pad(bytes * 8 - used - reserved.len());
    bytes as u32
}

/// Close a part written into `out`: check it ends where its size puts the
/// checksum, and append the checksum over the bytes from `crc_from`.
fn seal(out: Out, crc_from: usize, size: u32, what: &str) -> Result<Vec<u8>> {
    let at = out.position();
    let mut w = out.finish()?;
    if Some(at) != (size as usize * 8).checked_sub(16) {
        return Err(Ran(format!(
            "{what}'s fields end at bit {at}, its {size}-byte length puts the checksum elsewhere"
        )));
    }
    let crc = crc::crc16(&w.bytes()[crc_from..]);
    w.put(16, u32::from(crc));
    Ok(w.finish())
}

/// The bits a field naming one of `n` channels takes.
fn ceil_log2(n: usize) -> u32 {
    usize::BITS - n.saturating_sub(1).leading_zeros()
}

/// A signed value as the format codes it unsigned: `2v` for `v ≥ 0`,
/// `−2v − 1` below.
pub fn zigzag(value: i32) -> u32 {
    ((value << 1) ^ (value >> 31)) as u32
}

/// The inverse of [`zigzag`].
pub fn unzigzag(code: u32) -> i32 {
    (code >> 1) as i32 ^ -((code & 1) as i32)
}

/// Downmix coefficient magnitudes in Q15 by code index, 0 dB down to −60 dB
/// and −∞ (`ff_dca_dmixtable`; ETSI TS 102 114 table D.11).
#[rustfmt::skip]
pub const DMIX_TABLE: [u16; 242] = [
    0, 33, 35, 37, 39, 41, 44, 46,
    49, 52, 55, 58, 62, 65, 69, 73,
    78, 82, 87, 92, 98, 104, 110, 116,
    123, 130, 138, 146, 155, 164, 174, 184,
    195, 207, 219, 232, 246, 260, 276, 292,
    309, 328, 347, 368, 389, 413, 437, 463,
    490, 519, 550, 583, 617, 654, 693, 734,
    777, 823, 872, 924, 978, 1036, 1066, 1098,
    1130, 1163, 1197, 1232, 1268, 1305, 1343, 1382,
    1422, 1464, 1506, 1550, 1596, 1642, 1690, 1740,
    1790, 1843, 1896, 1952, 2009, 2068, 2128, 2190,
    2254, 2320, 2388, 2457, 2529, 2603, 2679, 2757,
    2838, 2920, 3006, 3093, 3184, 3277, 3372, 3471,
    3572, 3677, 3784, 3894, 4008, 4125, 4246, 4370,
    4497, 4629, 4764, 4903, 5046, 5193, 5345, 5501,
    5662, 5827, 5912, 5997, 6084, 6172, 6262, 6353,
    6445, 6538, 6633, 6729, 6827, 6925, 7026, 7128,
    7231, 7336, 7442, 7550, 7659, 7771, 7883, 7997,
    8113, 8231, 8350, 8471, 8594, 8719, 8845, 8973,
    9103, 9235, 9369, 9505, 9643, 9783, 9924, 10068,
    10214, 10362, 10512, 10665, 10819, 10976, 11135, 11297,
    11460, 11627, 11795, 11966, 12139, 12315, 12494, 12675,
    12859, 13045, 13234, 13426, 13621, 13818, 14018, 14222,
    14428, 14637, 14849, 15064, 15283, 15504, 15729, 15957,
    16188, 16423, 16661, 16902, 17147, 17396, 17648, 17904,
    18164, 18427, 18694, 18965, 19240, 19519, 19802, 20089,
    20380, 20675, 20975, 21279, 21587, 21900, 22218, 22540,
    22867, 23170, 23534, 23875, 24221, 24573, 24929, 25290,
    25657, 26029, 26406, 26789, 27177, 27571, 27970, 28376,
    28787, 29205, 29628, 30057, 30493, 30935, 31383, 31838,
    32300, 32768,
];

/// Inverse downmix scale magnitudes in Q16, for the scale codes from −40 dB
/// to 0 dB (`ff_dca_inv_dmixtable`; table D.11): entry `i` is the inverse of
/// [`DMIX_TABLE`]`[i + 41]`.
#[rustfmt::skip]
pub const INV_DMIX_TABLE: [u32; 201] = [
    6553600, 6186997, 5840902, 5514167, 5205710, 4914507, 4639593, 4380059,
    4135042, 3903731, 3685360, 3479204, 3284581, 3100844, 2927386, 2763630,
    2609035, 2463088, 2325305, 2195230, 2072430, 2013631, 1956500, 1900990,
    1847055, 1794651, 1743733, 1694260, 1646190, 1599484, 1554103, 1510010,
    1467168, 1425542, 1385096, 1345798, 1307615, 1270515, 1234468, 1199444,
    1165413, 1132348, 1100221, 1069005, 1038676, 1009206, 980573, 952752,
    925721, 899456, 873937, 849141, 825049, 801641, 778897, 756798,
    735326, 714463, 694193, 674497, 655360, 636766, 618700, 601146,
    584090, 567518, 551417, 535772, 520571, 505801, 491451, 477507,
    463959, 450796, 438006, 425579, 413504, 401772, 390373, 379297,
    368536, 363270, 358080, 352964, 347920, 342949, 338049, 333219,
    328458, 323765, 319139, 314579, 310084, 305654, 301287, 296982,
    292739, 288556, 284433, 280369, 276363, 272414, 268522, 264685,
    260904, 257176, 253501, 249879, 246309, 242790, 239321, 235901,
    232531, 229208, 225933, 222705, 219523, 216386, 213295, 210247,
    207243, 204282, 201363, 198486, 195650, 192855, 190099, 187383,
    184706, 182066, 179465, 176901, 174373, 171882, 169426, 167005,
    164619, 162267, 159948, 157663, 155410, 153190, 151001, 148844,
    146717, 144621, 142554, 140517, 138510, 136531, 134580, 132657,
    130762, 128893, 127052, 125236, 123447, 121683, 119944, 118231,
    116541, 114876, 113235, 111617, 110022, 108450, 106901, 105373,
    103868, 102383, 100921, 99479, 98057, 96656, 95275, 93914,
    92682, 91249, 89946, 88660, 87394, 86145, 84914, 83701,
    82505, 81326, 80164, 79019, 77890, 76777, 75680, 74598,
    73533, 72482, 71446, 70425, 69419, 68427, 67450, 66486,
    65536,
];

/// Quantised reflection coefficient magnitudes in Q16 by index
/// (`ff_dca_xll_refl_coeff`; the TABLE of ETSI TS 102 114 §8.5.4).
#[rustfmt::skip]
pub const REFLECTION_COEFFS: [u16; 128] = [
    0, 3070, 5110, 7140, 9156, 11154, 13132, 15085,
    17010, 18904, 20764, 22588, 24373, 26117, 27818, 29474,
    31085, 32648, 34164, 35631, 37049, 38418, 39738, 41008,
    42230, 43404, 44530, 45609, 46642, 47630, 48575, 49477,
    50337, 51157, 51937, 52681, 53387, 54059, 54697, 55302,
    55876, 56421, 56937, 57426, 57888, 58326, 58741, 59132,
    59502, 59852, 60182, 60494, 60789, 61066, 61328, 61576,
    61809, 62029, 62236, 62431, 62615, 62788, 62951, 63105,
    63250, 63386, 63514, 63635, 63749, 63855, 63956, 64051,
    64140, 64224, 64302, 64376, 64446, 64512, 64573, 64631,
    64686, 64737, 64785, 64830, 64873, 64913, 64950, 64986,
    65019, 65050, 65079, 65107, 65133, 65157, 65180, 65202,
    65222, 65241, 65259, 65275, 65291, 65306, 65320, 65333,
    65345, 65357, 65368, 65378, 65387, 65396, 65405, 65413,
    65420, 65427, 65434, 65440, 65446, 65451, 65456, 65461,
    65466, 65470, 65474, 65478, 65481, 65485, 65488, 65491,
];
