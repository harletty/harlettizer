// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/dca_exss.c, dca.h — Copyright (C) 2016 foo86
// What was taken and what was changed is recorded in docs/provenance.md.

//! The extension substream header and its audio asset descriptors.
//!
//! ETSI TS 102 114 §7.4–7.5. The extension substream is what turns a core
//! frame into a DTS-HD one: it follows the core in every frame and says where
//! each extension component of each audio asset sits. Its header is the only
//! place a decoder learns the lossless component's size, so it is the first
//! thing an encoder of that component has to get exactly right.
//!
//! The grammar is FFmpeg's reader, turned into a model that keeps every field
//! it reads rather than skipping it — FFmpeg skips what it does not need, and
//! a rewrite needs everything. Two stretches are kept as raw [`Bits`] because
//! no decoder in use reads them: the tail of each asset descriptor, after the
//! DTS-HD stream identifier (later revisions of the specification put fields
//! there, and the DTS:X navigation word lives in it), and the reserved bits
//! between the backward-compatible core fields and the header's checksum.
//!
//! [`ExssHeader::write`] writes the sizes the model carries and refuses a
//! model whose content does not fill them exactly; [`ExssHeader::fit`] sets
//! them from the content, for a header written from scratch. The checksum is
//! always computed, never copied from the model.

use crate::bits::Bits;
use crate::crc;
use hz_core::bits::BitWriter;
use hz_core::bits_read::{BitReader, Ran};

type Result<T> = std::result::Result<T, Ran>;

/// The extension substream sync word.
pub const SYNC: u32 = 0x6458_2025;

/// Extension component bits of an asset's extension mask.
pub mod component {
    pub const CORE: u16 = 0x010;
    pub const XBR: u16 = 0x020;
    pub const XXCH: u16 = 0x040;
    pub const X96: u16 = 0x080;
    pub const LBR: u16 = 0x100;
    pub const XLL: u16 = 0x200;
    pub const RSV1: u16 = 0x400;
    pub const RSV2: u16 = 0x800;
}

/// Extension substream sample rates by code, for an asset's maximum rate.
pub const SAMPLE_RATES: [u32; 16] = [
    8000, 16000, 32000, 64000, 128000, 22050, 44100, 88200, 176400, 352800, 12000, 24000, 48000,
    96000, 192000, 384000,
];

/// Channels a loudspeaker mask stands for: a pair bit counts two
/// (`ff_dca_count_chs_for_mask`).
pub fn channels_for_mask(mask: u32) -> u32 {
    ((mask & 0xffff) | ((mask & 0xae66) << 16)).count_ones()
}

/// An extension substream header, field by field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExssHeader {
    /// `UserDefinedBits`.
    pub user_defined: u8,
    /// `nExtSSIndex`, 0..=3.
    pub index: u8,
    /// `bHeaderSizeType`: twelve-bit header and twenty-bit substream sizes
    /// rather than eight and sixteen.
    pub wide: bool,
    /// Bytes in the header, checksum included.
    pub header_size: u32,
    /// Bytes in the whole substream, header included.
    pub size: u32,
    /// The per-stream static fields, when the header carries them.
    pub statics: Option<StaticFields>,
    /// Bytes of each asset's coded data, in order after the header.
    pub asset_sizes: Vec<u32>,
    /// One descriptor per asset.
    pub assets: Vec<AssetDescriptor>,
    /// Per asset: the backward-compatible core's substream and asset index,
    /// when the asset has one.
    pub backward_core: Vec<Option<(u8, u8)>>,
    /// Reserved bits and the byte alignment before the checksum.
    pub reserved: Bits,
    /// The checksum as read. [`ExssHeader::write`] computes its own.
    pub crc: u16,
}

/// The per-stream static fields of the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticFields {
    /// `nuRefClockCode`.
    pub reference_clock: u8,
    /// `nuExSSFrameDurationCode`: the frame lasts `512 × (code + 1)` samples
    /// of the reference clock.
    pub frame_duration_code: u8,
    /// `nuTimeStamp` and `nLSB`, 36 bits, when present.
    pub timecode: Option<u64>,
    /// One per audio presentation.
    pub presentations: Vec<Presentation>,
    /// `nuNumAssets`, the number of assets in this substream.
    pub asset_count: u8,
    /// The mixing configuration, when mixing metadata are enabled.
    pub mixing: Option<MixingConfig>,
}

/// One audio presentation's active substreams and assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation {
    /// `nuActiveExSSMask`, `index + 1` bits.
    pub active_substreams: u8,
    /// `nuActiveAssetMask`, one byte per active substream.
    pub active_assets: Vec<u8>,
}

/// The mixer's output configurations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixingConfig {
    /// `nuMixMetadataAdjLevel`.
    pub adjustment_level: u8,
    /// `nuBits4MixOutMask` code: masks are `4 × (code + 1)` bits wide.
    pub mask_width_code: u8,
    /// `nuMixOutChMask`, one per configuration (1..=4 of them).
    pub layouts: Vec<u32>,
}

impl MixingConfig {
    fn mask_bits(&self) -> u32 {
        (u32::from(self.mask_width_code) + 1) * 4
    }
}

/// An audio asset descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetDescriptor {
    /// Bytes in the descriptor.
    pub size: u32,
    /// `nuAssetIndex`.
    pub index: u8,
    /// The per-stream static metadata, present exactly when the header's
    /// static fields are.
    pub statics: Option<AssetStatics>,
    /// `nuDRCCode`, when dynamic range coefficients are present.
    pub drc_code: Option<u8>,
    /// `nuDialNormCode`, when present.
    pub dialnorm_code: Option<u8>,
    /// `nuDRC2ChDmixCode`: present when `drc_code` is and the asset embeds a
    /// stereo downmix.
    pub stereo_drc_code: Option<u8>,
    /// The asset's mixing metadata, when the header enables them and the
    /// asset sends them.
    pub mixing: Option<AssetMixing>,
    /// How the asset is coded and where its components are.
    pub coding: Coding,
    /// `nuDTSHDStreamID`, present when the asset has a lossless component.
    pub hd_stream_id: Option<u8>,
    /// Everything after that, up to the descriptor's size.
    pub tail: Bits,
}

/// An asset's static metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetStatics {
    /// `nuAssetTypeDescriptor`, when present.
    pub asset_type: Option<u8>,
    /// `LanguageDescriptor`, three bytes, when present.
    pub language: Option<u32>,
    /// `AdditionalTextString`, when present.
    pub text: Option<Vec<u8>>,
    /// `nuBitResolution`: bits per sample, 1..=32.
    pub pcm_bit_res: u8,
    /// `nuMaxSampleRate` code, into [`SAMPLE_RATES`].
    pub max_sample_rate_code: u8,
    /// `nuTotalNumChs`: channels, 1..=256.
    pub channels: u16,
    pub mapping: ChannelMapping,
}

/// How an asset's channels reach loudspeakers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelMapping {
    /// `bOne2OneMapChannels2Speakers`.
    OneToOne {
        /// `bEmbeddedStereoFlag`; only coded when there are more than two
        /// channels.
        embedded_stereo: bool,
        /// `bEmbeddedSixChFlag`; only coded when there are more than six.
        embedded_6ch: bool,
        /// `nuNumBits4SAMask` code and `nuSpkrActivityMask`, when the mask
        /// is enabled.
        speaker_mask: Option<(u8, u32)>,
        /// The speaker remapping sets.
        remaps: Vec<Remap>,
    },
    /// `nuRepresentationType`.
    Representation(u8),
}

/// One speaker remapping set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remap {
    /// `nuStndrSpkrLayoutMask`, as wide as the speaker mask.
    pub layout: u32,
    /// `nuNumDecCh4Remap`: decoded channels, 1..=32.
    pub decoded_channels: u8,
    /// Per loudspeaker of the layout: the decoded channels it takes
    /// (`nuRemapDecChMask`) and their five-bit codes.
    pub speakers: Vec<(u32, Vec<u8>)>,
}

/// An asset's mixing metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetMixing {
    /// `bExternalMixFlag`.
    pub external: bool,
    /// `nuPostMixGainAdjCode`.
    pub post_gain: u8,
    /// `nuControlMixerDRC`; 3 means a custom code follows.
    pub drc_prior_mode: u8,
    /// `nuLimit4EmbeddedDRC` (3 bits) or `nuCustomDRCCode` (8 bits).
    pub drc_prior_value: u8,
    /// `bEnblPerChMainAudioScale`.
    pub per_channel_scaling: bool,
    /// `nuMainAudioScaleCode`, six bits each.
    pub scaling: Vec<u8>,
    /// Per configuration, per downmix channel: the output mask and its
    /// six-bit coefficients.
    pub maps: Vec<Vec<(u32, Vec<u8>)>>,
}

/// An asset's coding mode and the components it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coding {
    /// Mode 0: a set of components.
    Components {
        /// The twelve-bit extension mask, as coded.
        mask: u16,
        /// Core size and sync distance.
        core: Option<(u32, Option<u8>)>,
        xbr: Option<u32>,
        xxch: Option<u32>,
        x96: Option<u32>,
        lbr: Option<Lbr>,
        xll: Option<Xll>,
        rsv1: Option<u16>,
        rsv2: Option<u16>,
    },
    /// Mode 1: lossless without a constant-rate component.
    Lossless(Xll),
    /// Mode 2: low bit rate.
    LowBitRate(Lbr),
    /// Mode 3: auxiliary coding.
    Auxiliary {
        size: u16,
        codec: u8,
        sync_distance: Option<u8>,
    },
}

impl Coding {
    /// The lossless component, wherever the mode puts it.
    pub fn xll(&self) -> Option<&Xll> {
        match self {
            Coding::Components { xll, .. } => xll.as_ref(),
            Coding::Lossless(xll) => Some(xll),
            _ => None,
        }
    }
}

/// The low bit rate component's navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lbr {
    pub size: u32,
    pub sync_distance: Option<u8>,
}

/// The lossless component's navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xll {
    /// Bytes of the component in this frame.
    pub size: u32,
    /// Present when the component carries its own sync word.
    pub sync: Option<XllSync>,
}

/// Where a lossless frame that spans substreams resynchronises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XllSync {
    /// `nuPeakBRCntrlBuffSzkB` code.
    pub peak_buffer_code: u8,
    /// `nuBitsInitDecDly`: width of the delay, 1..=32.
    pub delay_bits: u8,
    /// `nuInitLLDecDlyFrames`.
    pub delay_frames: u32,
    /// `nuExSSXLLSyncOffset`.
    pub sync_offset: u32,
}

/// The two field widths `wide` selects.
fn widths(wide: bool) -> (u32, u32) {
    if wide { (12, 20) } else { (8, 16) }
}

fn get_option<T>(
    r: &mut BitReader,
    read: impl FnOnce(&mut BitReader) -> Result<T>,
) -> Result<Option<T>> {
    if r.get_bit()? {
        read(r).map(Some)
    } else {
        Ok(None)
    }
}

/// A bit writer that refuses, rather than truncates, a value its field cannot
/// hold. `BitWriter::put` keeps the low bits, which for a size field is a
/// header that navigates somewhere else and still writes. The first refusal
/// is kept and returned by [`Out::finish`], so the writing code can read like
/// the grammar it writes.
struct Out {
    w: BitWriter,
    error: Option<String>,
}

impl Out {
    fn new() -> Self {
        Self {
            w: BitWriter::new(),
            error: None,
        }
    }

    fn refuse(&mut self, why: impl FnOnce() -> String) {
        if self.error.is_none() {
            self.error = Some(why());
        }
    }

    fn check(&mut self, ok: bool, why: impl FnOnce() -> String) {
        if !ok {
            self.refuse(why);
        }
    }

    /// Whether writing goes on. After the first refusal nothing more is
    /// written: a later field's width can depend on the refused one, and
    /// what would be written is going to be thrown away.
    fn writing(&self) -> bool {
        self.error.is_none()
    }

    /// A field coded as it is.
    fn field(&mut self, bits: u32, value: impl Into<u64>, what: &str) {
        let value = value.into();
        if !self.writing() {
            return;
        }
        if bits > 64 {
            self.refuse(|| format!("{what} cannot be {bits} bits wide"));
        } else if bits < 64 && value >> bits != 0 {
            self.refuse(|| format!("{what} = {value} does not fit in {bits} bits"));
        } else {
            put_wide(&mut self.w, bits, value);
        }
    }

    /// A count or size coded as itself less one, so `1..=2^bits`.
    fn size(&mut self, bits: u32, value: impl Into<u64>, what: &str) {
        let value = value.into();
        if !self.writing() {
            return;
        }
        if bits >= 64 {
            self.refuse(|| format!("{what} cannot be {bits} bits wide"));
        } else if value == 0 || value > 1u64 << bits {
            self.refuse(|| format!("{what} = {value} is outside 1..={}", 1u64 << bits));
        } else {
            put_wide(&mut self.w, bits, value - 1);
        }
    }

    fn bit(&mut self, bit: bool) {
        if self.writing() {
            self.w.put_bit(bit);
        }
    }

    fn option<T>(&mut self, value: &Option<T>, write: impl FnOnce(&mut Self, &T)) {
        self.bit(value.is_some());
        if let Some(value) = value {
            write(self, value);
        }
    }

    fn bits(&mut self, bits: &Bits) {
        if self.writing() {
            bits.write(&mut self.w);
        }
    }

    fn position(&self) -> usize {
        self.w.bit_position()
    }

    fn finish(self) -> Result<BitWriter> {
        match self.error {
            Some(why) => Err(Ran(why)),
            None => Ok(self.w),
        }
    }
}

/// Read up to 64 bits.
fn get_wide(r: &mut BitReader, bits: u32) -> Result<u64> {
    if bits > 32 {
        let high = u64::from(r.get(bits - 32)?);
        Ok(high << 32 | u64::from(r.get(32)?))
    } else {
        Ok(u64::from(r.get(bits)?))
    }
}

fn put_wide(w: &mut BitWriter, bits: u32, value: u64) {
    if bits > 32 {
        w.put(bits - 32, (value >> 32) as u32);
        w.put(32, value as u32);
    } else {
        w.put(bits, value as u32);
    }
}

impl ExssHeader {
    /// Read the header of the substream starting at `substream`.
    pub fn read(substream: &[u8]) -> Result<Self> {
        let mut r = BitReader::new(substream);
        let sync = r.get(32)?;
        if sync != SYNC {
            return Err(Ran(format!(
                "{sync:#010x} is not the extension substream sync word"
            )));
        }
        let user_defined = r.get(8)? as u8;
        let index = r.get(2)? as u8;
        let wide = r.get_bit()?;
        let (header_bits, size_bits) = widths(wide);
        let header_size = r.get(header_bits)? + 1;
        let size = r.get(size_bits)? + 1;
        // The header has to hold the fields read so far and its checksum;
        // everything after this subtracts from its length.
        if (header_size as usize) * 8 < r.bit_position() + 1 + 16 {
            return Err(Ran(format!(
                "a {header_size}-byte header cannot hold its own fields"
            )));
        }
        if header_size as usize > substream.len() {
            return Err(Ran(format!(
                "a {header_size}-byte header in {} bytes",
                substream.len()
            )));
        }
        if header_size > size {
            return Err(Ran(format!(
                "a {header_size}-byte header in a {size}-byte substream"
            )));
        }
        let statics = get_option(&mut r, |r| StaticFields::read(r, index))?;
        let asset_count = statics.as_ref().map_or(1, |s| usize::from(s.asset_count));
        let mut asset_sizes = Vec::with_capacity(asset_count);
        let mut end = u64::from(header_size);
        for _ in 0..asset_count {
            let asset = r.get(size_bits)? + 1;
            end += u64::from(asset);
            if end > u64::from(size) {
                return Err(Ran(format!(
                    "the assets run past the {size}-byte substream"
                )));
            }
            asset_sizes.push(asset);
        }
        let mut assets = Vec::with_capacity(asset_count);
        for _ in 0..asset_count {
            assets.push(AssetDescriptor::read(&mut r, statics.as_ref(), size_bits)?);
        }
        let mut backward_core = Vec::with_capacity(asset_count);
        for _ in 0..asset_count {
            backward_core.push(get_option(&mut r, |r| {
                Ok((r.get(2)? as u8, r.get(3)? as u8))
            })?);
        }
        let crc_at = header_size as usize * 8 - 16;
        let here = r.bit_position();
        if here > crc_at {
            return Err(Ran(format!(
                "the header's fields run {} bits past its checksum",
                here - crc_at
            )));
        }
        let reserved = Bits::read(&mut r, crc_at - here)?;
        let crc = r.get(16)? as u16;
        Ok(Self {
            user_defined,
            index,
            wide,
            header_size,
            size,
            statics,
            asset_sizes,
            assets,
            backward_core,
            reserved,
            crc,
        })
    }

    /// Whether the checksum stored in `substream`'s header is the one its
    /// bytes give.
    pub fn crc_is_valid(substream: &[u8], header_size: u32) -> bool {
        substream
            .get(5..header_size as usize)
            .is_some_and(crc::check)
    }

    /// Write the header, its checksum computed over what was written.
    ///
    /// Refuses a model whose fields do not fill [`Self::header_size`] or a
    /// descriptor's size exactly, or that holds a value its field cannot —
    /// the sizes are what a decoder navigates by, so a mismatch is a stream
    /// it cannot read.
    pub fn write(&self) -> Result<Vec<u8>> {
        let mut out = Out::new();
        self.write_fields(&mut out);
        out.bits(&self.reserved);
        let crc_at = (self.header_size as usize * 8).checked_sub(16);
        let at = out.position();
        out.check(crc_at == Some(at), || {
            format!(
                "the fields end at bit {at}, a {}-byte header's checksum is elsewhere",
                self.header_size
            )
        });
        let mut w = out.finish()?;
        let crc = crc::crc16(&w.bytes()[5..]);
        w.put(16, u32::from(crc));
        Ok(w.finish())
    }

    /// Everything up to the reserved bits.
    fn write_fields(&self, out: &mut Out) {
        let (header_bits, size_bits) = widths(self.wide);
        let assets = self
            .statics
            .as_ref()
            .map_or(1, |s| usize::from(s.asset_count));
        out.check(
            self.asset_sizes.len() == assets
                && self.assets.len() == assets
                && self.backward_core.len() == assets,
            || format!("{assets} assets declared, not described"),
        );
        out.field(32, SYNC, "the sync word");
        out.field(8, self.user_defined, "the user-defined bits");
        out.field(2, self.index, "the substream index");
        out.bit(self.wide);
        out.size(header_bits, self.header_size, "the header size");
        out.size(size_bits, self.size, "the substream size");
        out.option(&self.statics, |out, s| s.write(out, self.index));
        for &size in &self.asset_sizes {
            out.size(size_bits, size, "an asset size");
        }
        for asset in &self.assets {
            asset.write(out, self.statics.as_ref(), size_bits);
        }
        for core in &self.backward_core {
            out.option(core, |out, &(substream, asset)| {
                out.field(2, substream, "the backward-compatible core's substream");
                out.field(3, asset, "the backward-compatible core's asset");
            });
        }
    }

    /// Set every size from the content. Each descriptor keeps its tail, padded
    /// with zeroes to its next byte, and the reserved bits keep theirs, padded
    /// to the byte before the checksum; fitting twice changes nothing. A
    /// header the narrow field widths cannot describe — its own size, the
    /// substream's, an asset's or a component's — is fitted with the wide
    /// ones. `size`, the whole substream's, is the caller's to set. On a
    /// refusal the model is left as it was.
    pub fn fit(&mut self) -> Result<()> {
        let original = self.clone();
        if !self.wide && self.fit_at_width().is_ok() {
            return Ok(());
        }
        *self = original.clone();
        self.wide = true;
        self.fit_at_width().inspect_err(|_| *self = original)
    }

    fn fit_at_width(&mut self) -> Result<()> {
        let (header_bits, size_bits) = widths(self.wide);
        for asset in &mut self.assets {
            // The size field is nine bits whatever it holds; measure with one
            // that writes.
            asset.size = 1;
            let mut out = Out::new();
            asset.write_fields(&mut out, self.statics.as_ref(), size_bits);
            let used = out.position();
            out.finish()?;
            let bytes = (used + asset.tail.len()).div_ceil(8);
            asset.tail.pad(bytes * 8 - used - asset.tail.len());
            asset.size = bytes as u32;
        }
        // Measure with sizes that write; the substream's is the caller's and
        // may not be set yet.
        self.header_size = 1;
        let size = self.size;
        self.size = self.size.max(1);
        let mut out = Out::new();
        self.write_fields(&mut out);
        self.size = size;
        let position = out.position();
        out.finish()?;
        let bytes = (position + self.reserved.len()).div_ceil(8);
        self.reserved
            .pad(bytes * 8 - position - self.reserved.len());
        self.header_size = bytes as u32 + 2;
        if self.header_size > 1 << header_bits {
            return Err(Ran(format!(
                "a {}-byte header does not fit in {header_bits} bits",
                self.header_size
            )));
        }
        Ok(())
    }
}

impl StaticFields {
    fn read(r: &mut BitReader, index: u8) -> Result<Self> {
        let reference_clock = r.get(2)? as u8;
        let frame_duration_code = r.get(3)? as u8;
        let timecode = get_option(r, |r| get_wide(r, 36))?;
        let presentation_count = r.get(3)? as usize + 1;
        let asset_count = r.get(3)? as u8 + 1;
        let mut masks = Vec::with_capacity(presentation_count);
        for _ in 0..presentation_count {
            masks.push(r.get(u32::from(index) + 1)? as u8);
        }
        let mut presentations = Vec::with_capacity(presentation_count);
        for active_substreams in masks {
            let mut active_assets = Vec::new();
            for _ in 0..active_substreams.count_ones() {
                active_assets.push(r.get(8)? as u8);
            }
            presentations.push(Presentation {
                active_substreams,
                active_assets,
            });
        }
        let mixing = get_option(r, |r| {
            let adjustment_level = r.get(2)? as u8;
            let mask_width_code = r.get(2)? as u8;
            let count = r.get(2)? as usize + 1;
            let bits = (u32::from(mask_width_code) + 1) * 4;
            let mut layouts = Vec::with_capacity(count);
            for _ in 0..count {
                layouts.push(r.get(bits)?);
            }
            Ok(MixingConfig {
                adjustment_level,
                mask_width_code,
                layouts,
            })
        })?;
        Ok(Self {
            reference_clock,
            frame_duration_code,
            timecode,
            presentations,
            asset_count,
            mixing,
        })
    }

    fn write(&self, out: &mut Out, index: u8) {
        out.field(2, self.reference_clock, "the reference clock code");
        out.field(3, self.frame_duration_code, "the frame duration code");
        out.option(&self.timecode, |out, &t| out.field(36, t, "the timecode"));
        out.size(3, self.presentations.len() as u64, "the presentation count");
        out.size(3, self.asset_count, "the asset count");
        for p in &self.presentations {
            out.field(
                u32::from(index) + 1,
                p.active_substreams,
                "an active substream mask",
            );
        }
        for p in &self.presentations {
            out.check(
                p.active_assets.len() == p.active_substreams.count_ones() as usize,
                || "one active asset mask per active substream".into(),
            );
            for &mask in &p.active_assets {
                out.field(8, mask, "an active asset mask");
            }
        }
        out.option(&self.mixing, |out, m| {
            out.field(2, m.adjustment_level, "the mixing adjustment level");
            out.field(2, m.mask_width_code, "the mixing mask width code");
            out.size(2, m.layouts.len() as u64, "the mixing configuration count");
            for &layout in &m.layouts {
                out.field(m.mask_bits(), layout, "a mixing output layout");
            }
        });
    }
}

impl AssetDescriptor {
    fn read(r: &mut BitReader, statics: Option<&StaticFields>, size_bits: u32) -> Result<Self> {
        let start = r.bit_position();
        let size = r.get(9)? + 1;
        let index = r.get(3)? as u8;
        let asset_statics = match statics {
            Some(_) => Some(AssetStatics::read(r)?),
            None => None,
        };
        let drc = r.get_bit()?;
        let drc_code = if drc { Some(r.get(8)? as u8) } else { None };
        let dialnorm_code = get_option(r, |r| Ok(r.get(5)? as u8))?;
        let embedded_stereo = asset_statics
            .as_ref()
            .is_some_and(AssetStatics::embedded_stereo);
        let stereo_drc_code = if drc && embedded_stereo {
            Some(r.get(8)? as u8)
        } else {
            None
        };
        let mixing = match statics.and_then(|s| s.mixing.as_ref()) {
            Some(config) => get_option(r, |r| {
                let channels = asset_statics
                    .as_ref()
                    .map_or(0, AssetStatics::downmix_channels);
                AssetMixing::read(r, config, channels)
            })?,
            None => None,
        };
        let coding = Coding::read(r, size_bits)?;
        let hd_stream_id = if coding.xll().is_some() {
            Some(r.get(3)? as u8)
        } else {
            None
        };
        let end = start + size as usize * 8;
        let here = r.bit_position();
        if here > end {
            return Err(Ran(format!(
                "the asset descriptor's fields run {} bits past its size",
                here - end
            )));
        }
        let tail = Bits::read(r, end - here)?;
        Ok(Self {
            size,
            index,
            statics: asset_statics,
            drc_code,
            dialnorm_code,
            stereo_drc_code,
            mixing,
            coding,
            hd_stream_id,
            tail,
        })
    }

    fn write(&self, out: &mut Out, statics: Option<&StaticFields>, size_bits: u32) {
        let start = out.position();
        self.write_fields(out, statics, size_bits);
        out.bits(&self.tail);
        let written = out.position() - start;
        out.check(written == self.size as usize * 8, || {
            format!(
                "a {}-byte asset descriptor written in {written} bits",
                self.size
            )
        });
    }

    fn write_fields(&self, out: &mut Out, statics: Option<&StaticFields>, size_bits: u32) {
        out.check(statics.is_some() == self.statics.is_some(), || {
            "an asset's static metadata come and go with the header's".into()
        });
        out.size(9, self.size, "an asset descriptor's size");
        out.field(3, self.index, "the asset index");
        if let Some(s) = &self.statics {
            s.write(out);
        }
        out.option(&self.drc_code, |out, &c| out.field(8, c, "the DRC code"));
        out.option(&self.dialnorm_code, |out, &c| {
            out.field(5, c, "the dialogue normalisation code")
        });
        let embedded_stereo = self
            .statics
            .as_ref()
            .is_some_and(AssetStatics::embedded_stereo);
        out.check(
            self.stereo_drc_code.is_some() == (self.drc_code.is_some() && embedded_stereo),
            || {
                "a stereo downmix DRC code is coded exactly when DRC is and the asset \
                 embeds stereo"
                    .into()
            },
        );
        if let Some(code) = self.stereo_drc_code {
            out.field(8, code, "the stereo downmix DRC code");
        }
        match statics.and_then(|s| s.mixing.as_ref()) {
            Some(config) => {
                let channels = self
                    .statics
                    .as_ref()
                    .map_or(0, AssetStatics::downmix_channels);
                out.option(&self.mixing, |out, m| m.write(out, config, channels));
            }
            None => out.check(self.mixing.is_none(), || {
                "mixing metadata without a mixing configuration".into()
            }),
        }
        self.coding.write(out, size_bits);
        out.check(
            self.hd_stream_id.is_some() == self.coding.xll().is_some(),
            || "the DTS-HD stream identifier is coded exactly when the asset is lossless".into(),
        );
        if let Some(id) = self.hd_stream_id {
            out.field(3, id, "the DTS-HD stream identifier");
        }
    }
}

impl AssetStatics {
    fn embedded_stereo(&self) -> bool {
        matches!(
            self.mapping,
            ChannelMapping::OneToOne {
                embedded_stereo: true,
                ..
            }
        )
    }

    /// Channels the mixing metadata describe: the asset's, plus any embedded
    /// downmix.
    fn downmix_channels(&self) -> usize {
        let mut channels = usize::from(self.channels);
        if let ChannelMapping::OneToOne {
            embedded_stereo,
            embedded_6ch,
            ..
        } = self.mapping
        {
            channels += 6 * usize::from(embedded_6ch) + 2 * usize::from(embedded_stereo);
        }
        channels
    }

    fn read(r: &mut BitReader) -> Result<Self> {
        let asset_type = get_option(r, |r| Ok(r.get(4)? as u8))?;
        let language = get_option(r, |r| r.get(24))?;
        let text = get_option(r, |r| {
            let len = r.get(10)? as usize + 1;
            let mut text = Vec::with_capacity(len);
            for _ in 0..len {
                text.push(r.get(8)? as u8);
            }
            Ok(text)
        })?;
        let pcm_bit_res = r.get(5)? as u8 + 1;
        let max_sample_rate_code = r.get(4)? as u8;
        let channels = r.get(8)? as u16 + 1;
        let mapping = if r.get_bit()? {
            let embedded_stereo = channels > 2 && r.get_bit()?;
            let embedded_6ch = channels > 6 && r.get_bit()?;
            let speaker_mask = get_option(r, |r| {
                let code = r.get(2)? as u8;
                let mask = r.get((u32::from(code) + 1) * 4)?;
                Ok((code, mask))
            })?;
            let mask_bits = speaker_mask.map_or(0, |(code, _)| (u32::from(code) + 1) * 4);
            let sets = r.get(3)? as usize;
            if sets > 0 && mask_bits == 0 {
                return Err(Ran("speaker remapping without a speaker mask".into()));
            }
            let mut layouts = Vec::with_capacity(sets);
            for _ in 0..sets {
                layouts.push(r.get(mask_bits)?);
            }
            let mut remaps = Vec::with_capacity(sets);
            for layout in layouts {
                let decoded_channels = r.get(5)? as u8 + 1;
                let mut speakers = Vec::new();
                for _ in 0..channels_for_mask(layout) {
                    let mask = r.get(u32::from(decoded_channels))?;
                    let mut codes = Vec::with_capacity(mask.count_ones() as usize);
                    for _ in 0..mask.count_ones() {
                        codes.push(r.get(5)? as u8);
                    }
                    speakers.push((mask, codes));
                }
                remaps.push(Remap {
                    layout,
                    decoded_channels,
                    speakers,
                });
            }
            ChannelMapping::OneToOne {
                embedded_stereo,
                embedded_6ch,
                speaker_mask,
                remaps,
            }
        } else {
            ChannelMapping::Representation(r.get(3)? as u8)
        };
        Ok(Self {
            asset_type,
            language,
            text,
            pcm_bit_res,
            max_sample_rate_code,
            channels,
            mapping,
        })
    }

    fn write(&self, out: &mut Out) {
        out.option(&self.asset_type, |out, &t| {
            out.field(4, t, "the asset type")
        });
        out.option(&self.language, |out, &l| out.field(24, l, "the language"));
        out.option(&self.text, |out, text| {
            out.size(10, text.len() as u64, "the text length");
            for &byte in text {
                out.field(8, byte, "a text byte");
            }
        });
        out.size(5, self.pcm_bit_res, "the bit resolution");
        out.field(4, self.max_sample_rate_code, "the sample rate code");
        out.size(8, self.channels, "the channel count");
        match &self.mapping {
            ChannelMapping::OneToOne {
                embedded_stereo,
                embedded_6ch,
                speaker_mask,
                remaps,
            } => {
                out.bit(true);
                if self.channels > 2 {
                    out.bit(*embedded_stereo);
                } else {
                    out.check(!embedded_stereo, || {
                        "an embedded stereo downmix of two channels or fewer".into()
                    });
                }
                if self.channels > 6 {
                    out.bit(*embedded_6ch);
                } else {
                    out.check(!embedded_6ch, || {
                        "an embedded six-channel downmix of six channels or fewer".into()
                    });
                }
                out.option(speaker_mask, |out, &(code, mask)| {
                    out.field(2, code, "the speaker mask width code");
                    out.field((u32::from(code) + 1) * 4, mask, "the speaker mask");
                });
                let mask_bits = speaker_mask.map_or(0, |(code, _)| (u32::from(code) + 1) * 4);
                out.field(3, remaps.len() as u64, "the remapping set count");
                out.check(remaps.is_empty() || mask_bits > 0, || {
                    "speaker remapping without a speaker mask".into()
                });
                for remap in remaps {
                    out.field(mask_bits, remap.layout, "a remapping layout");
                }
                for remap in remaps {
                    out.size(5, remap.decoded_channels, "a remapping's decoded channels");
                    out.check(
                        remap.speakers.len() == channels_for_mask(remap.layout) as usize,
                        || "one remapping entry per speaker of its layout".into(),
                    );
                    for (mask, codes) in &remap.speakers {
                        out.field(u32::from(remap.decoded_channels), *mask, "a remapping mask");
                        out.check(codes.len() == mask.count_ones() as usize, || {
                            "one remapping code per channel of its mask".into()
                        });
                        for &code in codes {
                            out.field(5, code, "a remapping code");
                        }
                    }
                }
            }
            ChannelMapping::Representation(kind) => {
                out.bit(false);
                out.field(3, *kind, "the representation type");
            }
        }
    }
}

impl AssetMixing {
    fn read(r: &mut BitReader, config: &MixingConfig, channels: usize) -> Result<Self> {
        let external = r.get_bit()?;
        let post_gain = r.get(6)? as u8;
        let drc_prior_mode = r.get(2)? as u8;
        let drc_prior_value = r.get(if drc_prior_mode == 3 { 8 } else { 3 })? as u8;
        let per_channel_scaling = r.get_bit()?;
        let outputs: Vec<u32> = config
            .layouts
            .iter()
            .map(|&l| channels_for_mask(l))
            .collect();
        let codes = if per_channel_scaling {
            outputs.iter().sum::<u32>() as usize
        } else {
            outputs.len()
        };
        let mut scaling = Vec::with_capacity(codes);
        for _ in 0..codes {
            scaling.push(r.get(6)? as u8);
        }
        let mut maps = Vec::with_capacity(outputs.len());
        for &out in &outputs {
            if out == 0 {
                return Err(Ran("a mixing configuration with no output channel".into()));
            }
            let mut config_maps = Vec::with_capacity(channels);
            for _ in 0..channels {
                let mask = r.get(out)?;
                let mut coefficients = Vec::with_capacity(mask.count_ones() as usize);
                for _ in 0..mask.count_ones() {
                    coefficients.push(r.get(6)? as u8);
                }
                config_maps.push((mask, coefficients));
            }
            maps.push(config_maps);
        }
        Ok(Self {
            external,
            post_gain,
            drc_prior_mode,
            drc_prior_value,
            per_channel_scaling,
            scaling,
            maps,
        })
    }

    fn write(&self, out: &mut Out, config: &MixingConfig, channels: usize) {
        out.bit(self.external);
        out.field(6, self.post_gain, "the post-mix gain");
        out.field(2, self.drc_prior_mode, "the mixer DRC mode");
        out.field(
            if self.drc_prior_mode == 3 { 8 } else { 3 },
            self.drc_prior_value,
            "the mixer DRC value",
        );
        out.bit(self.per_channel_scaling);
        let outputs: Vec<u32> = config
            .layouts
            .iter()
            .map(|&l| channels_for_mask(l))
            .collect();
        let codes = if self.per_channel_scaling {
            outputs.iter().sum::<u32>() as usize
        } else {
            outputs.len()
        };
        out.check(self.scaling.len() == codes, || {
            format!("{codes} scaling codes, not {}", self.scaling.len())
        });
        for &code in &self.scaling {
            out.field(6, code, "a scaling code");
        }
        out.check(
            self.maps.len() == outputs.len() && self.maps.iter().all(|m| m.len() == channels),
            || format!("one mixing map per configuration and per each of {channels} channels"),
        );
        for (&width, config_maps) in outputs.iter().zip(&self.maps) {
            for (mask, coefficients) in config_maps {
                out.field(width, *mask, "a mixing map");
                out.check(coefficients.len() == mask.count_ones() as usize, || {
                    "one mixing coefficient per output of its map".into()
                });
                for &c in coefficients {
                    out.field(6, c, "a mixing coefficient");
                }
            }
        }
    }
}

impl Coding {
    fn read(r: &mut BitReader, size_bits: u32) -> Result<Self> {
        Ok(match r.get(2)? {
            0 => {
                let mask = r.get(12)? as u16;
                let has = |bit| mask & bit != 0;
                let core = if has(component::CORE) {
                    let size = r.get(14)? + 1;
                    Some((size, get_option(r, |r| Ok(r.get(2)? as u8))?))
                } else {
                    None
                };
                let xbr = if has(component::XBR) {
                    Some(r.get(14)? + 1)
                } else {
                    None
                };
                let xxch = if has(component::XXCH) {
                    Some(r.get(14)? + 1)
                } else {
                    None
                };
                let x96 = if has(component::X96) {
                    Some(r.get(12)? + 1)
                } else {
                    None
                };
                let lbr = if has(component::LBR) {
                    Some(Lbr::read(r)?)
                } else {
                    None
                };
                let xll = if has(component::XLL) {
                    Some(Xll::read(r, size_bits)?)
                } else {
                    None
                };
                let rsv1 = if has(component::RSV1) {
                    Some(r.get(16)? as u16)
                } else {
                    None
                };
                let rsv2 = if has(component::RSV2) {
                    Some(r.get(16)? as u16)
                } else {
                    None
                };
                Coding::Components {
                    mask,
                    core,
                    xbr,
                    xxch,
                    x96,
                    lbr,
                    xll,
                    rsv1,
                    rsv2,
                }
            }
            1 => Coding::Lossless(Xll::read(r, size_bits)?),
            2 => Coding::LowBitRate(Lbr::read(r)?),
            _ => {
                let size = r.get(14)? as u16;
                let codec = r.get(8)? as u8;
                let sync_distance = get_option(r, |r| Ok(r.get(3)? as u8))?;
                Coding::Auxiliary {
                    size,
                    codec,
                    sync_distance,
                }
            }
        })
    }

    fn write(&self, out: &mut Out, size_bits: u32) {
        match self {
            Coding::Components {
                mask,
                core,
                xbr,
                xxch,
                x96,
                lbr,
                xll,
                rsv1,
                rsv2,
            } => {
                let named = [
                    (component::CORE, core.is_some()),
                    (component::XBR, xbr.is_some()),
                    (component::XXCH, xxch.is_some()),
                    (component::X96, x96.is_some()),
                    (component::LBR, lbr.is_some()),
                    (component::XLL, xll.is_some()),
                    (component::RSV1, rsv1.is_some()),
                    (component::RSV2, rsv2.is_some()),
                ];
                if let Some((bit, _)) = named.iter().find(|&&(bit, some)| (mask & bit != 0) != some)
                {
                    out.refuse(|| {
                        format!("extension mask {mask:#05x} and component {bit:#05x} disagree")
                    });
                }
                out.field(2, 0u8, "the coding mode");
                out.field(12, *mask, "the extension mask");
                if let Some((size, sync)) = core {
                    out.size(14, *size, "the core size");
                    out.option(sync, |out, &d| out.field(2, d, "the core sync distance"));
                }
                if let Some(size) = xbr {
                    out.size(14, *size, "the XBR size");
                }
                if let Some(size) = xxch {
                    out.size(14, *size, "the XXCH size");
                }
                if let Some(size) = x96 {
                    out.size(12, *size, "the X96 size");
                }
                if let Some(lbr) = lbr {
                    lbr.write(out);
                }
                if let Some(xll) = xll {
                    xll.write(out, size_bits);
                }
                if let Some(v) = rsv1 {
                    out.field(16, *v, "the first reserved component");
                }
                if let Some(v) = rsv2 {
                    out.field(16, *v, "the second reserved component");
                }
            }
            Coding::Lossless(xll) => {
                out.field(2, 1u8, "the coding mode");
                xll.write(out, size_bits);
            }
            Coding::LowBitRate(lbr) => {
                out.field(2, 2u8, "the coding mode");
                lbr.write(out);
            }
            Coding::Auxiliary {
                size,
                codec,
                sync_distance,
            } => {
                out.field(2, 3u8, "the coding mode");
                out.field(14, *size, "the auxiliary size");
                out.field(8, *codec, "the auxiliary codec");
                out.option(sync_distance, |out, &d| {
                    out.field(3, d, "the auxiliary sync distance")
                });
            }
        }
    }
}

impl Lbr {
    fn read(r: &mut BitReader) -> Result<Self> {
        let size = r.get(14)? + 1;
        let sync_distance = get_option(r, |r| Ok(r.get(2)? as u8))?;
        Ok(Self {
            size,
            sync_distance,
        })
    }

    fn write(&self, out: &mut Out) {
        out.size(14, self.size, "the LBR size");
        out.option(&self.sync_distance, |out, &d| {
            out.field(2, d, "the LBR sync distance")
        });
    }
}

impl Xll {
    fn read(r: &mut BitReader, size_bits: u32) -> Result<Self> {
        let size = r.get(size_bits)? + 1;
        let sync = get_option(r, |r| {
            let peak_buffer_code = r.get(4)? as u8;
            let delay_bits = r.get(5)? as u8 + 1;
            let delay_frames = r.get(u32::from(delay_bits))?;
            let sync_offset = r.get(size_bits)?;
            Ok(XllSync {
                peak_buffer_code,
                delay_bits,
                delay_frames,
                sync_offset,
            })
        })?;
        Ok(Self { size, sync })
    }

    fn write(&self, out: &mut Out, size_bits: u32) {
        out.size(size_bits, self.size, "the XLL size");
        out.option(&self.sync, |out, s| {
            out.field(4, s.peak_buffer_code, "the peak buffer code");
            out.size(5, s.delay_bits, "the delay width");
            out.field(u32::from(s.delay_bits), s.delay_frames, "the initial delay");
            out.field(size_bits, s.sync_offset, "the XLL sync offset");
        });
    }
}

/// Bytes in the substream starting at `substream`, read from its header
/// without the rest of it: for splitting a stream into frames.
pub fn substream_size(substream: &[u8]) -> Result<usize> {
    let mut r = BitReader::new(substream);
    if r.get(32)? != SYNC {
        return Err(Ran("not an extension substream".into()));
    }
    r.skip(8 + 2)?;
    let (header_bits, size_bits) = widths(r.get_bit()?);
    r.skip(header_bits as usize)?;
    Ok(r.get(size_bits)? as usize + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One lossless 7.1 asset behind a core, the shape of a disc's.
    pub(crate) fn seven_one() -> ExssHeader {
        let mut header = ExssHeader {
            user_defined: 0,
            index: 0,
            wide: true,
            header_size: 0,
            size: 0,
            statics: Some(StaticFields {
                reference_clock: 2,
                frame_duration_code: 0,
                timecode: None,
                presentations: vec![Presentation {
                    active_substreams: 1,
                    active_assets: vec![1],
                }],
                asset_count: 1,
                mixing: None,
            }),
            asset_sizes: vec![6000],
            assets: vec![AssetDescriptor {
                size: 0,
                index: 0,
                statics: Some(AssetStatics {
                    asset_type: None,
                    language: None,
                    text: None,
                    pcm_bit_res: 24,
                    max_sample_rate_code: 12,
                    channels: 8,
                    mapping: ChannelMapping::OneToOne {
                        embedded_stereo: false,
                        embedded_6ch: true,
                        speaker_mask: Some((3, 0x084b)),
                        remaps: vec![],
                    },
                }),
                drc_code: None,
                dialnorm_code: None,
                stereo_drc_code: None,
                mixing: None,
                coding: Coding::Components {
                    mask: component::XLL,
                    core: None,
                    xbr: None,
                    xxch: None,
                    x96: None,
                    lbr: None,
                    xll: Some(Xll {
                        size: 6000,
                        sync: Some(XllSync {
                            peak_buffer_code: 8,
                            delay_bits: 3,
                            delay_frames: 1,
                            sync_offset: 0,
                        }),
                    }),
                    rsv1: None,
                    rsv2: None,
                },
                hd_stream_id: Some(0),
                tail: Bits::new(),
            }],
            backward_core: vec![Some((0, 0))],
            reserved: Bits::new(),
            crc: 0,
        };
        header.fit().unwrap();
        header.size = header.header_size + 6000;
        header
    }

    #[test]
    fn a_fitted_header_round_trips_with_a_valid_checksum() {
        let header = seven_one();
        let bytes = header.write().unwrap();
        assert_eq!(bytes.len(), header.header_size as usize);
        assert!(ExssHeader::crc_is_valid(&bytes, header.header_size));
        let back = ExssHeader::read(&bytes).unwrap();
        assert_eq!(back.write().unwrap(), bytes);
        assert_eq!(
            ExssHeader {
                crc: 0,
                ..back.clone()
            },
            header
        );
        assert_eq!(substream_size(&bytes).unwrap(), header.size as usize);
    }

    /// The fixture with a navigation word appended to its descriptor tail,
    /// refitted.
    fn with_navigation_word() -> (ExssHeader, u64) {
        let mut header = seven_one();
        let word = crate::navigation::Navigation {
            offset: 6428,
            size: 4128,
        }
        .word()
        .unwrap();
        let mut w = BitWriter::new();
        w.put(2, 0);
        w.put(32, (word >> 32) as u32);
        w.put(32, word as u32);
        let bytes = w.finish();
        header.assets[0].tail = Bits::read(&mut BitReader::new(&bytes), 66).unwrap();
        header.fit().unwrap();
        header.size = header.header_size + 6000;
        (header, word)
    }

    #[test]
    fn fitting_keeps_the_descriptor_tail() {
        let (header, word) = with_navigation_word();
        assert_eq!(header.assets[0].tail.word64(2), Some(word));
        let bytes = header.write().unwrap();
        let back = ExssHeader::read(&bytes).unwrap();
        assert_eq!(back.assets[0].tail, header.assets[0].tail);
        // Fitting twice changes nothing.
        let mut again = header.clone();
        again.fit().unwrap();
        assert_eq!(again, header);
    }

    #[test]
    fn a_descriptor_size_that_disagrees_with_its_content_is_refused() {
        let (mut header, _) = with_navigation_word();
        header.assets[0].size -= 1;
        assert!(header.write().is_err());
    }

    #[test]
    fn a_value_too_wide_for_its_field_is_refused_not_truncated() {
        // 300 bytes of text push a narrow header past 256 bytes: written
        // as is, its eight-bit size would say 77.
        let mut header = seven_one();
        header.wide = false;
        if let Some(s) = &mut header.assets[0].statics {
            s.text = Some(vec![b'x'; 300]);
        }
        header.header_size = 333;
        header.assets[0].size = 1;
        assert!(header.write().is_err());
        // Fitting moves it to the wide widths instead.
        header.fit().unwrap();
        assert!(header.wide);
        header.size = header.header_size + 6000;
        let bytes = header.write().unwrap();
        assert_eq!(ExssHeader::read(&bytes).unwrap().write().unwrap(), bytes);
        // A component bigger than its field can say is refused.
        if let Coding::Components { xll: Some(xll), .. } = &mut header.assets[0].coding {
            xll.size = (1 << 20) + 1;
        }
        assert!(header.write().is_err());
    }

    #[test]
    fn a_large_asset_is_fitted_with_the_wide_widths() {
        let mut header = seven_one();
        header.wide = false;
        header.fit().unwrap();
        assert!(!header.wide, "a small header stays narrow");
        header.asset_sizes[0] = 70_000;
        if let Coding::Components { xll: Some(xll), .. } = &mut header.assets[0].coding {
            xll.size = 70_000;
        }
        header.fit().unwrap();
        assert!(header.wide);
        header.size = header.header_size + 70_000;
        let bytes = header.write().unwrap();
        assert_eq!(ExssHeader::read(&bytes).unwrap().write().unwrap(), bytes);
    }

    #[test]
    fn an_invalid_width_is_refused_not_panicked_on() {
        let mut header = seven_one();
        if let Coding::Components { xll: Some(xll), .. } = &mut header.assets[0].coding {
            xll.sync.as_mut().unwrap().delay_bits = 65;
        }
        assert!(header.write().is_err());
        let mut header = seven_one();
        if let Some(AssetStatics {
            mapping: ChannelMapping::OneToOne { speaker_mask, .. },
            ..
        }) = &mut header.assets[0].statics
        {
            *speaker_mask = Some((200, 0x084b));
        }
        assert!(header.write().is_err());
        let before = header.clone();
        assert!(header.fit().is_err());
        assert_eq!(header, before, "a refused fit leaves the model alone");
    }

    #[test]
    fn no_damage_to_a_header_makes_the_reader_panic() {
        let bytes = seven_one().write().unwrap();
        for len in 0..bytes.len() {
            let _ = ExssHeader::read(&bytes[..len]);
        }
        for bit in 0..bytes.len() * 8 {
            let mut damaged = bytes.clone();
            damaged[bit / 8] ^= 0x80 >> (bit % 8);
            if let Ok(header) = ExssHeader::read(&damaged) {
                let _ = header.write();
            }
        }
        // The case reported: the wide header-size field, bits 43..55,
        // cleared, so the header claims to be one byte long.
        let mut r = BitReader::new(&bytes);
        let head = Bits::read(&mut r, 43).unwrap();
        r.skip(12).unwrap();
        let rest = Bits::read(&mut r, bytes.len() * 8 - 55).unwrap();
        let mut w = BitWriter::new();
        head.write(&mut w);
        w.put(12, 0);
        rest.write(&mut w);
        let damaged = w.finish();
        assert!(ExssHeader::read(&damaged).is_err());
    }

    #[test]
    fn a_damaged_header_fails_its_checksum() {
        let header = seven_one();
        let mut bytes = header.write().unwrap();
        assert!(ExssHeader::crc_is_valid(&bytes, header.header_size));
        bytes[7] ^= 0x04;
        assert!(!ExssHeader::crc_is_valid(&bytes, header.header_size));
    }

    #[test]
    fn a_mask_without_its_component_is_refused() {
        let mut header = seven_one();
        if let Coding::Components { mask, .. } = &mut header.assets[0].coding {
            *mask |= component::CORE;
        }
        assert!(header.write().is_err());
    }

    #[test]
    fn pairs_count_twice() {
        // C, L/R, LFE, Lsr/Rsr, Lss/Rss: five bits, three of them pairs.
        assert_eq!(channels_for_mask(0x084b), 8);
        assert_eq!(channels_for_mask(0x0001), 1);
        assert_eq!(channels_for_mask(0x0002), 2);
    }
}
