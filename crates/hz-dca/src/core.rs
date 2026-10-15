// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/dca.c, dcadata.c, dca_sample_rate_tab.h — Copyright (C) 2016 foo86
// What was taken and what was changed is recorded in docs/provenance.md.

//! The core frame header.
//!
//! The first thirteen bytes of every core frame (fifteen with the header
//! checksum), ETSI TS 102 114 §5.3.1. Every field is kept as the code the
//! stream carries, not as the value it stands for: a header read from a
//! reference stream has to be written back bit for bit, and a value that two
//! codes could stand for (the sample-rate and bit-rate tables both have
//! unused entries) would not say which one to write.
//!
//! The reader refuses what FFmpeg's refuses — a deficit other than a whole
//! block, a block count that is not a multiple of eight, a frame under 96
//! bytes, an unknown channel arrangement, sample rate, word length or low
//! frequency flag, the reserved bit set — so a header this reads is one that
//! decoder would read too.

use hz_core::bits::BitWriter;
use hz_core::bits_read::{BitReader, Ran};

type Result<T> = std::result::Result<T, Ran>;

/// The core frame sync word, big-endian 16-bit form.
pub const SYNC: u32 = 0x7ffe_8001;

/// Samples per PCM block: the deficit field is always this, less one.
pub const BLOCK_SAMPLES: u32 = 32;

/// Core sample rates by code (`sfreq`); zero is an unused code.
pub const SAMPLE_RATES: [u32; 16] = [
    0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 96000, 192000,
];

/// Transmission bit rates by code (`rate`). The last three codes are not
/// rates: open, variable and lossless.
pub const BIT_RATES: [u32; 32] = [
    32000, 56000, 64000, 96000, 112000, 128000, 192000, 224000, 256000, 320000, 384000, 448000,
    512000, 576000, 640000, 768000, 896000, 1024000, 1152000, 1280000, 1344000, 1408000, 1411200,
    1472000, 1536000, 1920000, 2048000, 3072000, 3840000, 1, 2, 3,
];

/// Channels in each core channel arrangement (`amode`), LFE excluded.
pub const CHANNELS: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];

/// Source word lengths by code (`pcmr`); zero is an unused code.
pub const BITS_PER_SAMPLE: [u8; 8] = [16, 16, 20, 20, 0, 24, 24, 0];

/// Bits in a header without its checksum.
pub const HEADER_BITS: usize = 104;

/// A core frame header, field by field, as coded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreHeader {
    /// `ftype`: a normal frame rather than a termination frame.
    pub normal_frame: bool,
    /// `cpf`: whether the header carries its checksum.
    pub crc_present: bool,
    /// `nblks + 1`: PCM blocks of 32 samples in the frame.
    pub pcm_blocks: u32,
    /// `fsize + 1`: bytes in the frame, header included.
    pub frame_size: u32,
    /// `amode`: the channel arrangement.
    pub audio_mode: u8,
    /// `sfreq`: the sample-rate code.
    pub sample_rate_code: u8,
    /// `rate`: the bit-rate code.
    pub bit_rate_code: u8,
    /// `dynf`: dynamic range coefficients follow in the audio.
    pub drc_present: bool,
    /// `timef`: a time stamp follows.
    pub timestamp_present: bool,
    /// `auxf`: auxiliary data follow.
    pub aux_present: bool,
    /// `hdcd`.
    pub hdcd_master: bool,
    /// `ext_audio_id`: which extension the frame carries.
    pub ext_audio_type: u8,
    /// `ext_audio`: whether it carries one.
    pub ext_audio_present: bool,
    /// `aspf`: audio sync word insertion.
    pub sync_ssf: bool,
    /// `lff`: no LFE, 128× or 64× decimated.
    pub lfe: u8,
    /// `hflag`: predictor history carries over from the last frame.
    pub predictor_history: bool,
    /// `hcrc`, present when [`Self::crc_present`] is.
    pub header_crc: Option<u16>,
    /// `filts`: the multirate interpolator is perfect-reconstruction.
    pub filter_perfect: bool,
    /// `vernum`: the encoder software revision.
    pub encoder_revision: u8,
    /// `chist`: copy history.
    pub copy_history: u8,
    /// `pcmr`: the source word-length code.
    pub pcm_resolution_code: u8,
    /// `sumf`: front channels sum/difference coded.
    pub sumdiff_front: bool,
    /// `sums`: surround channels sum/difference coded.
    pub sumdiff_surround: bool,
    /// `dialnorm` (`dn_code`).
    pub dialnorm_code: u8,
}

impl CoreHeader {
    /// Read a header from the start of a core frame.
    pub fn read(frame: &[u8]) -> Result<Self> {
        let mut r = BitReader::new(frame);
        let sync = r.get(32)?;
        if sync != SYNC {
            return Err(Ran(format!("{sync:#010x} is not the core sync word")));
        }
        let normal_frame = r.get_bit()?;
        let deficit = r.get(5)? + 1;
        if deficit != BLOCK_SAMPLES {
            return Err(Ran(format!(
                "a deficit of {deficit} samples is not a whole block"
            )));
        }
        let crc_present = r.get_bit()?;
        let pcm_blocks = r.get(7)? + 1;
        if !pcm_blocks.is_multiple_of(8) {
            return Err(Ran(format!(
                "{pcm_blocks} PCM blocks is not a whole number of subband blocks"
            )));
        }
        let frame_size = r.get(14)? + 1;
        if frame_size < 96 {
            return Err(Ran(format!("a {frame_size}-byte core frame is too short")));
        }
        let audio_mode = r.get(6)? as u8;
        if audio_mode >= 16 {
            return Err(Ran(format!(
                "channel arrangement {audio_mode} is user-defined"
            )));
        }
        let sample_rate_code = r.get(4)? as u8;
        if SAMPLE_RATES[usize::from(sample_rate_code)] == 0 {
            return Err(Ran(format!(
                "sample-rate code {sample_rate_code} is unused"
            )));
        }
        let bit_rate_code = r.get(5)? as u8;
        if r.get_bit()? {
            return Err(Ran("the reserved bit is set".into()));
        }
        let drc_present = r.get_bit()?;
        let timestamp_present = r.get_bit()?;
        let aux_present = r.get_bit()?;
        let hdcd_master = r.get_bit()?;
        let ext_audio_type = r.get(3)? as u8;
        let ext_audio_present = r.get_bit()?;
        let sync_ssf = r.get_bit()?;
        let lfe = r.get(2)? as u8;
        if lfe == 3 {
            return Err(Ran("LFE flag 3 is invalid".into()));
        }
        let predictor_history = r.get_bit()?;
        let header_crc = if crc_present {
            Some(r.get(16)? as u16)
        } else {
            None
        };
        let filter_perfect = r.get_bit()?;
        let encoder_revision = r.get(4)? as u8;
        let copy_history = r.get(2)? as u8;
        let pcm_resolution_code = r.get(3)? as u8;
        if BITS_PER_SAMPLE[usize::from(pcm_resolution_code)] == 0 {
            return Err(Ran(format!(
                "word-length code {pcm_resolution_code} is unused"
            )));
        }
        let sumdiff_front = r.get_bit()?;
        let sumdiff_surround = r.get_bit()?;
        let dialnorm_code = r.get(4)? as u8;
        Ok(Self {
            normal_frame,
            crc_present,
            pcm_blocks,
            frame_size,
            audio_mode,
            sample_rate_code,
            bit_rate_code,
            drc_present,
            timestamp_present,
            aux_present,
            hdcd_master,
            ext_audio_type,
            ext_audio_present,
            sync_ssf,
            lfe,
            predictor_history,
            header_crc,
            filter_perfect,
            encoder_revision,
            copy_history,
            pcm_resolution_code,
            sumdiff_front,
            sumdiff_surround,
            dialnorm_code,
        })
    }

    /// Write the header.
    ///
    /// # Panics
    /// If a field does not fit its width, or the checksum flag and the
    /// checksum disagree: a header that is wrong here is a frame no decoder
    /// will find the end of.
    pub fn write(&self, w: &mut BitWriter) {
        assert!(
            (1..=128).contains(&self.pcm_blocks) && self.pcm_blocks.is_multiple_of(8),
            "{} PCM blocks",
            self.pcm_blocks
        );
        assert!(
            (96..=16384).contains(&self.frame_size),
            "a {}-byte frame",
            self.frame_size
        );
        assert!(self.audio_mode < 16 && self.lfe < 3);
        assert_eq!(self.crc_present, self.header_crc.is_some());
        w.put(32, SYNC);
        w.put_bit(self.normal_frame);
        w.put(5, BLOCK_SAMPLES - 1);
        w.put_bit(self.crc_present);
        w.put(7, self.pcm_blocks - 1);
        w.put(14, self.frame_size - 1);
        w.put(6, u32::from(self.audio_mode));
        w.put(4, u32::from(self.sample_rate_code));
        w.put(5, u32::from(self.bit_rate_code));
        w.put_bit(false);
        w.put_bit(self.drc_present);
        w.put_bit(self.timestamp_present);
        w.put_bit(self.aux_present);
        w.put_bit(self.hdcd_master);
        w.put(3, u32::from(self.ext_audio_type));
        w.put_bit(self.ext_audio_present);
        w.put_bit(self.sync_ssf);
        w.put(2, u32::from(self.lfe));
        w.put_bit(self.predictor_history);
        if let Some(crc) = self.header_crc {
            w.put(16, u32::from(crc));
        }
        w.put_bit(self.filter_perfect);
        w.put(4, u32::from(self.encoder_revision));
        w.put(2, u32::from(self.copy_history));
        w.put(3, u32::from(self.pcm_resolution_code));
        w.put_bit(self.sumdiff_front);
        w.put_bit(self.sumdiff_surround);
        w.put(4, u32::from(self.dialnorm_code));
    }

    /// Bytes the header occupies: it always ends on a byte boundary.
    pub fn len(&self) -> usize {
        (HEADER_BITS + if self.crc_present { 16 } else { 0 }) / 8
    }

    /// Always false: a header is never empty. For the `len` lint.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Samples per channel in the frame.
    pub fn samples(&self) -> u32 {
        self.pcm_blocks * BLOCK_SAMPLES
    }

    /// The sample rate, in hertz.
    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATES[usize::from(self.sample_rate_code)]
    }

    /// Full-band channels, LFE excluded.
    pub fn channels(&self) -> u8 {
        CHANNELS[usize::from(self.audio_mode)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 5.1 core at 48 kHz in 2012-byte frames, the reference streams' shape:
    /// 1509 kb/s actual, under the 1536 kb/s its rate code names.
    pub(crate) fn five_one() -> CoreHeader {
        CoreHeader {
            normal_frame: true,
            crc_present: false,
            pcm_blocks: 16,
            frame_size: 2012,
            audio_mode: 9,
            sample_rate_code: 13,
            bit_rate_code: 24,
            drc_present: false,
            timestamp_present: false,
            aux_present: false,
            hdcd_master: false,
            ext_audio_type: 0,
            ext_audio_present: false,
            sync_ssf: true,
            lfe: 2,
            predictor_history: true,
            header_crc: None,
            filter_perfect: false,
            encoder_revision: 7,
            copy_history: 0,
            pcm_resolution_code: 6,
            sumdiff_front: false,
            sumdiff_surround: false,
            dialnorm_code: 0,
        }
    }

    #[test]
    fn a_header_round_trips_and_ends_on_a_byte() {
        for header in [
            five_one(),
            CoreHeader {
                crc_present: true,
                header_crc: Some(0xbeef),
                ..five_one()
            },
        ] {
            let mut w = BitWriter::new();
            header.write(&mut w);
            assert!(w.is_aligned());
            assert_eq!(w.byte_position(), header.len());
            assert_eq!(CoreHeader::read(w.bytes()).unwrap(), header);
        }
    }

    #[test]
    fn what_ffmpeg_refuses_is_refused() {
        let mut w = BitWriter::new();
        five_one().write(&mut w);
        let good = w.finish();
        // Sample-rate code 0: bits 66..70, the middle of byte 8.
        let mut bad = good.clone();
        bad[8] &= !0x3c;
        assert!(CoreHeader::read(&bad).is_err());
        // The reserved bit, bit 75.
        let mut bad = good.clone();
        bad[9] |= 0x10;
        assert!(CoreHeader::read(&bad).is_err());
        assert!(CoreHeader::read(&good[..10]).is_err());
    }

    #[test]
    fn the_values_behind_the_codes() {
        let header = five_one();
        assert_eq!(header.sample_rate(), 48000);
        assert_eq!(header.channels(), 5);
        assert_eq!(header.samples(), 512);
        assert_eq!(BIT_RATES[usize::from(header.bit_rate_code)], 1_536_000);
    }
}
