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

fn put_option<T>(w: &mut BitWriter, value: &Option<T>, write: impl FnOnce(&mut BitWriter, &T)) {
    w.put_bit(value.is_some());
    if let Some(value) = value {
        write(w, value);
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
        if header_size as usize > substream.len() {
            return Err(Ran(format!(
                "a {header_size}-byte header in {} bytes",
                substream.len()
            )));
        }
        let statics = get_option(&mut r, |r| StaticFields::read(r, index))?;
        let asset_count = statics.as_ref().map_or(1, |s| usize::from(s.asset_count));
        let mut asset_sizes = Vec::with_capacity(asset_count);
        for _ in 0..asset_count {
            asset_sizes.push(r.get(size_bits)? + 1);
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
    /// descriptor's size exactly — the sizes are what a decoder navigates
    /// by, so a mismatch is a stream it cannot read.
    pub fn write(&self) -> Result<Vec<u8>> {
        let (header_bits, size_bits) = widths(self.wide);
        let assets = self
            .statics
            .as_ref()
            .map_or(1, |s| usize::from(s.asset_count));
        if self.asset_sizes.len() != assets
            || self.assets.len() != assets
            || self.backward_core.len() != assets
        {
            return Err(Ran(format!("{assets} assets declared, not described")));
        }
        let mut w = BitWriter::new();
        w.put(32, SYNC);
        w.put(8, u32::from(self.user_defined));
        w.put(2, u32::from(self.index));
        w.put_bit(self.wide);
        w.put(header_bits, self.header_size - 1);
        w.put(size_bits, self.size - 1);
        put_option(&mut w, &self.statics, |w, s| s.write(w, self.index));
        for &size in &self.asset_sizes {
            w.put(size_bits, size - 1);
        }
        for asset in &self.assets {
            asset.write(&mut w, self.statics.as_ref(), size_bits)?;
        }
        for core in &self.backward_core {
            put_option(&mut w, core, |w, &(substream, asset)| {
                w.put(2, u32::from(substream));
                w.put(3, u32::from(asset));
            });
        }
        self.reserved.write(&mut w);
        let crc_at = self.header_size as usize * 8 - 16;
        if w.bit_position() != crc_at {
            return Err(Ran(format!(
                "the fields end at bit {}, the checksum is at bit {crc_at}",
                w.bit_position()
            )));
        }
        let crc = crc::crc16(&w.bytes()[5..]);
        w.put(16, u32::from(crc));
        Ok(w.finish())
    }

    /// Set every size from the content: each descriptor's tail padded with
    /// zeroes to its next byte, and the reserved bits to the byte before the
    /// checksum. `size`, the whole substream's, is the caller's to set.
    pub fn fit(&mut self) {
        let (header_bits, size_bits) = widths(self.wide);
        let mut position = 32 + 8 + 2 + 1 + header_bits as usize + size_bits as usize + 1;
        if let Some(statics) = &self.statics {
            position += statics.bits(self.index);
        }
        position += self.asset_sizes.len() * size_bits as usize;
        for asset in &mut self.assets {
            // The size field is nine bits whatever it holds; measure with one
            // that writes.
            asset.size = 1;
            let used = asset.bits_before_tail(self.statics.as_ref(), size_bits);
            let bytes = used.div_ceil(8);
            asset.tail = Bits::zeroes(bytes * 8 - used);
            asset.size = bytes as u32;
            position += bytes * 8;
        }
        position += self
            .backward_core
            .iter()
            .map(|core| if core.is_some() { 6 } else { 1 })
            .sum::<usize>();
        let bytes = position.div_ceil(8);
        self.reserved = Bits::zeroes(bytes * 8 - position);
        self.header_size = bytes as u32 + 2;
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

    fn write(&self, w: &mut BitWriter, index: u8) {
        w.put(2, u32::from(self.reference_clock));
        w.put(3, u32::from(self.frame_duration_code));
        put_option(w, &self.timecode, |w, &t| put_wide(w, 36, t));
        w.put(3, self.presentations.len() as u32 - 1);
        w.put(3, u32::from(self.asset_count) - 1);
        for p in &self.presentations {
            w.put(u32::from(index) + 1, u32::from(p.active_substreams));
        }
        for p in &self.presentations {
            for &mask in &p.active_assets {
                w.put(8, u32::from(mask));
            }
        }
        put_option(w, &self.mixing, |w, m| {
            w.put(2, u32::from(m.adjustment_level));
            w.put(2, u32::from(m.mask_width_code));
            w.put(2, m.layouts.len() as u32 - 1);
            for &layout in &m.layouts {
                w.put(m.mask_bits(), layout);
            }
        });
    }

    fn bits(&self, index: u8) -> usize {
        let mut w = BitWriter::new();
        self.write(&mut w, index);
        w.bit_position()
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

    fn write(
        &self,
        w: &mut BitWriter,
        statics: Option<&StaticFields>,
        size_bits: u32,
    ) -> Result<()> {
        let start = w.bit_position();
        self.write_fields(w, statics, size_bits)?;
        self.tail.write(w);
        let written = w.bit_position() - start;
        if written != self.size as usize * 8 {
            return Err(Ran(format!(
                "a {}-byte asset descriptor written in {written} bits",
                self.size
            )));
        }
        Ok(())
    }

    fn write_fields(
        &self,
        w: &mut BitWriter,
        statics: Option<&StaticFields>,
        size_bits: u32,
    ) -> Result<()> {
        if statics.is_some() != self.statics.is_some() {
            return Err(Ran(
                "an asset's static metadata come and go with the header's".into(),
            ));
        }
        w.put(9, self.size - 1);
        w.put(3, u32::from(self.index));
        if let Some(s) = &self.statics {
            s.write(w);
        }
        w.put_bit(self.drc_code.is_some());
        if let Some(code) = self.drc_code {
            w.put(8, u32::from(code));
        }
        put_option(w, &self.dialnorm_code, |w, &c| w.put(5, u32::from(c)));
        let embedded_stereo = self
            .statics
            .as_ref()
            .is_some_and(AssetStatics::embedded_stereo);
        if self.stereo_drc_code.is_some() != (self.drc_code.is_some() && embedded_stereo) {
            return Err(Ran(
                "a stereo downmix DRC code is coded exactly when DRC is and the asset embeds stereo"
                    .into(),
            ));
        }
        if let Some(code) = self.stereo_drc_code {
            w.put(8, u32::from(code));
        }
        match statics.and_then(|s| s.mixing.as_ref()) {
            Some(config) => put_option(w, &self.mixing, |w, m| m.write(w, config)),
            None if self.mixing.is_some() => {
                return Err(Ran("mixing metadata without a mixing configuration".into()));
            }
            None => {}
        }
        self.coding.write(w, size_bits)?;
        if self.hd_stream_id.is_some() != self.coding.xll().is_some() {
            return Err(Ran(
                "the DTS-HD stream identifier is coded exactly when the asset is lossless".into(),
            ));
        }
        if let Some(id) = self.hd_stream_id {
            w.put(3, u32::from(id));
        }
        Ok(())
    }

    /// Bits up to the tail, which is what [`ExssHeader::fit`] sizes from.
    fn bits_before_tail(&self, statics: Option<&StaticFields>, size_bits: u32) -> usize {
        let mut w = BitWriter::new();
        // `fit` is for a model built in code; one this refuses to write has
        // no size, and writing it is where the refusal will say why.
        let _ = self.write_fields(&mut w, statics, size_bits);
        w.bit_position()
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

    fn write(&self, w: &mut BitWriter) {
        put_option(w, &self.asset_type, |w, &t| w.put(4, u32::from(t)));
        put_option(w, &self.language, |w, &l| w.put(24, l));
        put_option(w, &self.text, |w, text| {
            w.put(10, text.len() as u32 - 1);
            for &byte in text {
                w.put(8, u32::from(byte));
            }
        });
        w.put(5, u32::from(self.pcm_bit_res) - 1);
        w.put(4, u32::from(self.max_sample_rate_code));
        w.put(8, u32::from(self.channels) - 1);
        match &self.mapping {
            ChannelMapping::OneToOne {
                embedded_stereo,
                embedded_6ch,
                speaker_mask,
                remaps,
            } => {
                w.put_bit(true);
                if self.channels > 2 {
                    w.put_bit(*embedded_stereo);
                }
                if self.channels > 6 {
                    w.put_bit(*embedded_6ch);
                }
                put_option(w, speaker_mask, |w, &(code, mask)| {
                    w.put(2, u32::from(code));
                    w.put((u32::from(code) + 1) * 4, mask);
                });
                let mask_bits = speaker_mask.map_or(0, |(code, _)| (u32::from(code) + 1) * 4);
                w.put(3, remaps.len() as u32);
                for remap in remaps {
                    w.put(mask_bits, remap.layout);
                }
                for remap in remaps {
                    w.put(5, u32::from(remap.decoded_channels) - 1);
                    for (mask, codes) in &remap.speakers {
                        w.put(u32::from(remap.decoded_channels), *mask);
                        for &code in codes {
                            w.put(5, u32::from(code));
                        }
                    }
                }
            }
            ChannelMapping::Representation(kind) => {
                w.put_bit(false);
                w.put(3, u32::from(*kind));
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

    fn write(&self, w: &mut BitWriter, config: &MixingConfig) {
        w.put_bit(self.external);
        w.put(6, u32::from(self.post_gain));
        w.put(2, u32::from(self.drc_prior_mode));
        w.put(
            if self.drc_prior_mode == 3 { 8 } else { 3 },
            u32::from(self.drc_prior_value),
        );
        w.put_bit(self.per_channel_scaling);
        for &code in &self.scaling {
            w.put(6, u32::from(code));
        }
        for (layout, config_maps) in config.layouts.iter().zip(&self.maps) {
            let out = channels_for_mask(*layout);
            for (mask, coefficients) in config_maps {
                w.put(out, *mask);
                for &c in coefficients {
                    w.put(6, u32::from(c));
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

    fn write(&self, w: &mut BitWriter, size_bits: u32) -> Result<()> {
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
                    return Err(Ran(format!(
                        "extension mask {mask:#05x} and component {bit:#05x} disagree"
                    )));
                }
                w.put(2, 0);
                w.put(12, u32::from(*mask));
                if let Some((size, sync)) = core {
                    w.put(14, size - 1);
                    put_option(w, sync, |w, &d| w.put(2, u32::from(d)));
                }
                if let Some(size) = xbr {
                    w.put(14, size - 1);
                }
                if let Some(size) = xxch {
                    w.put(14, size - 1);
                }
                if let Some(size) = x96 {
                    w.put(12, size - 1);
                }
                if let Some(lbr) = lbr {
                    lbr.write(w);
                }
                if let Some(xll) = xll {
                    xll.write(w, size_bits);
                }
                if let Some(v) = rsv1 {
                    w.put(16, u32::from(*v));
                }
                if let Some(v) = rsv2 {
                    w.put(16, u32::from(*v));
                }
            }
            Coding::Lossless(xll) => {
                w.put(2, 1);
                xll.write(w, size_bits);
            }
            Coding::LowBitRate(lbr) => {
                w.put(2, 2);
                lbr.write(w);
            }
            Coding::Auxiliary {
                size,
                codec,
                sync_distance,
            } => {
                w.put(2, 3);
                w.put(14, u32::from(*size));
                w.put(8, u32::from(*codec));
                put_option(w, sync_distance, |w, &d| w.put(3, u32::from(d)));
            }
        }
        Ok(())
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

    fn write(&self, w: &mut BitWriter) {
        w.put(14, self.size - 1);
        put_option(w, &self.sync_distance, |w, &d| w.put(2, u32::from(d)));
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

    fn write(&self, w: &mut BitWriter, size_bits: u32) {
        w.put(size_bits, self.size - 1);
        put_option(w, &self.sync, |w, s| {
            w.put(4, u32::from(s.peak_buffer_code));
            w.put(5, u32::from(s.delay_bits) - 1);
            w.put(u32::from(s.delay_bits), s.delay_frames);
            w.put(size_bits, s.sync_offset);
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
        header.fit();
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

    #[test]
    fn a_descriptor_tail_is_carried_and_its_size_enforced() {
        let mut header = seven_one();
        // A tail of whole bytes after the padding `fit` chose.
        let mut tail = BitWriter::new();
        tail.put_bits(header.assets[0].tail.bytes(), header.assets[0].tail.len());
        tail.put(32, 0x1800_2000);
        tail.put(32, 0x8a28_1000);
        let len = tail.bit_position();
        let bytes = tail.finish();
        let mut r = BitReader::new(&bytes);
        header.assets[0].tail = Bits::read(&mut r, len).unwrap();
        header.assets[0].size += 8;
        header.header_size += 8;
        let bytes = header.write().unwrap();
        let back = ExssHeader::read(&bytes).unwrap();
        assert_eq!(back.assets[0].tail, header.assets[0].tail);

        header.assets[0].size -= 1;
        assert!(header.write().is_err());
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
