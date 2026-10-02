//! A standalone IA sequence (IAMF §5): the descriptors, then one temporal
//! unit per frame.
//!
//! What is written is the simplest sequence that carries a channel bed: one
//! codec config, one channel-based audio element of a single layer, and one
//! mix presentation that plays it as it is, with the loudness measured on
//! the element's own layout and on the stereo pair every sub-mix has to
//! state. Simple profile throughout — one element, at most sixteen channels
//! — which is the profile every decoder reads.
//!
//! The loudness is a property of the whole programme and sits in a
//! descriptor at the head of the file, so it is written as a placeholder and
//! patched when the programme ends. The fields are fixed-width, so the patch
//! changes no length and nothing after it moves.

use crate::flac;
use crate::layout::{Layout, STEREO_SOUND_SYSTEM};
use crate::obu::{ObuType, put_leb128, put_obu, q7_8};
use std::fmt;
use std::io::{self, Seek, SeekFrom, Write};

/// How the substreams are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Linear PCM, little-endian: no compression, nothing to get wrong.
    Lpcm,
    /// FLAC, lossless.
    Flac,
}

impl Codec {
    fn four_cc(self) -> &'static [u8; 4] {
        match self {
            Self::Lpcm => b"ipcm",
            Self::Flac => b"fLaC",
        }
    }
}

/// How a decoder playing to headphones renders the bed (IAMF §3.7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Headphones {
    /// Fold it to the stereo pair, as for two loudspeakers.
    Stereo,
    /// Hand it to the decoder's binaural renderer.
    Binaural,
}

/// What the sequence is.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub layout: Layout,
    pub codec: Codec,
    pub sample_rate: u32,
    /// Bits a sample, which the samples pushed are already at.
    pub bits: u32,
    /// `num_samples_per_frame`: samples a channel per temporal unit.
    pub frame: usize,
    pub headphones: Headphones,
}

/// The loudness of the programme on one layout, as the mix presentation
/// states it: BS.1770 integrated loudness in LKFS, and the sample and true
/// peaks in dBFS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub integrated: f64,
    pub digital_peak: f64,
    pub true_peak: f64,
}

/// A sequence that could not be written.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A configuration IAMF, the profile or the codec cannot carry.
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Unsupported(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<flac::Unsupported> for Error {
    fn from(e: flac::Unsupported) -> Self {
        Self::Unsupported(e.0)
    }
}

/// Identifiers. One of each, so any distinct values would do; these are
/// kept apart from each other so that a dump reads unambiguously.
const CODEC_CONFIG_ID: u64 = 0;
const AUDIO_ELEMENT_ID: u64 = 1;
const MIX_PRESENTATION_ID: u64 = 2;
const ELEMENT_MIX_GAIN_ID: u64 = 100;
const OUTPUT_MIX_GAIN_ID: u64 = 101;

/// A `LoudnessInfo` with a true peak and nothing anchored: the type byte,
/// then three Q7.8 fields.
const LOUDNESS_FIELDS: usize = 6;

/// Writes one IA sequence to `out`.
pub struct Writer<W: Write + Seek> {
    out: W,
    config: Config,
    flac: Option<flac::Encoder>,
    /// Where, in `out`, each layout's loudness fields start: stereo, then
    /// the element's own layout.
    loudness_at: [u64; 2],
    /// One channel of the unit being written, deinterleaved.
    channels: Vec<Vec<i32>>,
    unit: Vec<u8>,
    payload: Vec<u8>,
    units: u64,
    /// Samples a channel the last unit was padded with, once there is one.
    ended: Option<usize>,
}

impl<W: Write + Seek> Writer<W> {
    /// Check `config` and write the descriptors.
    pub fn new(mut out: W, config: Config) -> Result<Self, Error> {
        if config.layout.channels() > 16 {
            return Err(Error::Unsupported(format!(
                "{} channels in one element; the simple profile carries sixteen",
                config.layout.channels()
            )));
        }
        if config.frame == 0 {
            return Err(Error::Unsupported("an empty frame".into()));
        }
        let flac = match config.codec {
            Codec::Lpcm => {
                if ![16, 24, 32].contains(&config.bits) {
                    return Err(Error::Unsupported(format!(
                        "{}-bit LPCM; IAMF takes 16, 24 or 32",
                        config.bits
                    )));
                }
                if ![16_000, 32_000, 44_100, 48_000, 96_000].contains(&config.sample_rate) {
                    return Err(Error::Unsupported(format!(
                        "{} Hz LPCM; IAMF takes 16, 32, 44.1, 48 or 96 kHz",
                        config.sample_rate
                    )));
                }
                None
            }
            Codec::Flac => Some(flac::Encoder::new(
                config.sample_rate,
                config.bits,
                config.frame,
            )?),
        };

        let start = out.stream_position()?;
        let (descriptors, offsets) = descriptors(&config, flac.as_ref());
        out.write_all(&descriptors)?;
        Ok(Self {
            out,
            config,
            flac,
            loudness_at: offsets.map(|at| start + at as u64),
            channels: vec![vec![0; config.frame]; config.layout.channels()],
            unit: Vec::new(),
            payload: Vec::new(),
            units: 0,
            ended: None,
        })
    }

    /// Samples a channel each push takes.
    pub fn frame(&self) -> usize {
        self.config.frame
    }

    /// Temporal units written so far.
    pub fn units(&self) -> u64 {
        self.units
    }

    /// Write one temporal unit from `interleaved` frames in the layout's
    /// channel order, each sample already at the configured depth.
    ///
    /// A full frame unless it is the last: a short one is padded with
    /// silence and the padding trimmed at the end, which closes the
    /// substreams — nothing may follow it.
    pub fn push(&mut self, interleaved: &[i32]) -> io::Result<()> {
        let width = self.config.layout.channels();
        let frame = self.config.frame;
        assert!(
            self.ended.is_none(),
            "a short frame ends the sequence; nothing follows it"
        );
        assert_eq!(interleaved.len() % width, 0, "whole frames only");
        let frames = interleaved.len() / width;
        assert!(
            (1..=frame).contains(&frames),
            "a unit is one to {frame} frames"
        );

        for (c, channel) in self.channels.iter_mut().enumerate() {
            for (out, chunk) in channel.iter_mut().zip(interleaved.chunks_exact(width)) {
                *out = chunk[c];
            }
            channel[frames..].fill(0);
        }
        let pad = frame - frames;
        let trim = (pad > 0).then_some((pad as u32, 0));

        self.unit.clear();
        // A temporal delimiter opens every unit, which lets a reader find the
        // unit boundaries without knowing how many substreams there are.
        put_obu(&mut self.unit, ObuType::TemporalDelimiter, None, &[]);
        let layout = self.config.layout;
        for substream in 0..layout.substreams() {
            let carried = layout.substream_channels(substream);
            self.payload.clear();
            match &mut self.flac {
                None => {
                    for i in 0..frame {
                        for &c in carried {
                            let bytes = self.channels[c][i].to_le_bytes();
                            let width = self.config.bits as usize / 8;
                            self.payload.extend_from_slice(&bytes[..width]);
                        }
                    }
                }
                Some(encoder) => {
                    let slices: [&[i32]; 2];
                    let channels: &[&[i32]] = if carried.len() == 2 {
                        slices = [&self.channels[carried[0]], &self.channels[carried[1]]];
                        &slices
                    } else {
                        slices = [&self.channels[carried[0]], &[]];
                        &slices[..1]
                    };
                    self.payload
                        .extend_from_slice(encoder.encode(self.units, channels));
                }
            }
            put_obu(
                &mut self.unit,
                ObuType::AudioFrameId(substream as u8),
                trim,
                &self.payload,
            );
        }
        self.out.write_all(&self.unit)?;
        self.units += 1;
        if pad > 0 {
            self.ended = Some(pad);
        }
        Ok(())
    }

    /// State the programme's loudness — on the stereo pair, then on the
    /// element's own layout — and hand back the output.
    pub fn finish(mut self, loudness: [Loudness; 2]) -> io::Result<W> {
        let end = self.out.stream_position()?;
        for (at, loudness) in self.loudness_at.iter().zip(loudness) {
            let mut fields = [0u8; LOUDNESS_FIELDS];
            for (slot, db) in fields.chunks_exact_mut(2).zip([
                loudness.integrated,
                loudness.digital_peak,
                loudness.true_peak,
            ]) {
                slot.copy_from_slice(&q7_8(db).to_be_bytes());
            }
            self.out.seek(SeekFrom::Start(*at))?;
            self.out.write_all(&fields)?;
        }
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// The descriptors, and where in them each layout's loudness fields start.
fn descriptors(config: &Config, flac: Option<&flac::Encoder>) -> (Vec<u8>, [usize; 2]) {
    let mut out = Vec::with_capacity(256);
    let mut payload = Vec::with_capacity(128);

    // IA sequence header: simple profile, and nothing more.
    payload.extend_from_slice(b"iamf");
    payload.extend_from_slice(&[0, 0]);
    put_obu(&mut out, ObuType::SequenceHeader, None, &payload);

    // Codec config.
    payload.clear();
    put_leb128(&mut payload, CODEC_CONFIG_ID);
    payload.extend_from_slice(config.codec.four_cc());
    put_leb128(&mut payload, config.frame as u64);
    // audio_roll_distance: nought for both lossless codecs (IAMF §3.11).
    payload.extend_from_slice(&0i16.to_be_bytes());
    match flac {
        None => {
            // Little-endian, the depth, the rate.
            payload.push(1);
            payload.push(config.bits as u8);
            payload.extend_from_slice(&config.sample_rate.to_be_bytes());
        }
        Some(encoder) => payload.extend_from_slice(&encoder.decoder_config()),
    }
    put_obu(&mut out, ObuType::CodecConfig, None, &payload);

    // Audio element: channel-based, one layer, no parameters — a single
    // layer has nothing to demix, and a lossless codec nothing to recon.
    let layout = config.layout;
    payload.clear();
    put_leb128(&mut payload, AUDIO_ELEMENT_ID);
    payload.push(0); // audio_element_type CHANNEL_BASED, reserved
    put_leb128(&mut payload, CODEC_CONFIG_ID);
    put_leb128(&mut payload, layout.substreams() as u64);
    for substream in 0..layout.substreams() {
        put_leb128(&mut payload, substream as u64);
    }
    put_leb128(&mut payload, 0); // num_parameters
    payload.push(1 << 5); // num_layers
    // loudspeaker_layout, no output gain, no recon gain, reserved.
    payload.push(layout.loudspeaker_layout << 4);
    payload.push(layout.substreams() as u8);
    payload.push(layout.coupled as u8);
    put_obu(&mut out, ObuType::AudioElement, None, &payload);

    // Mix presentation: the one element, at unity, measured on stereo and on
    // its own layout.
    payload.clear();
    put_leb128(&mut payload, MIX_PRESENTATION_ID);
    put_leb128(&mut payload, 0); // count_label: no annotations
    put_leb128(&mut payload, 1); // num_sub_mixes
    put_leb128(&mut payload, 1); // num_audio_elements
    put_leb128(&mut payload, AUDIO_ELEMENT_ID);
    let headphones = match config.headphones {
        Headphones::Stereo => 0u8,
        Headphones::Binaural => 1,
    };
    payload.push(headphones << 6);
    put_leb128(&mut payload, 0); // rendering_config_extension_size
    put_mix_gain(&mut payload, ELEMENT_MIX_GAIN_ID, config.sample_rate);
    put_mix_gain(&mut payload, OUTPUT_MIX_GAIN_ID, config.sample_rate);
    put_leb128(&mut payload, 2); // num_layouts
    let mut offsets = [0usize; 2];
    for (offset, sound_system) in offsets
        .iter_mut()
        .zip([STEREO_SOUND_SYSTEM, layout.sound_system])
    {
        // layout_type LOUDSPEAKERS_SS_CONVENTION, the sound system, reserved.
        payload.push((2 << 6) | (sound_system << 2));
        payload.push(1); // info_type: a true peak, nothing anchored
        *offset = payload.len();
        payload.extend_from_slice(&[0; LOUDNESS_FIELDS]);
    }
    // No mix presentation tags. v1.1 added them at the end of the OBU, a
    // v1.0 parser — FFmpeg's, among others — reports the byte it did not
    // expect, and a v1.1 parser has to read a v1.0 stream without them. An
    // empty list says nothing that absence does not.
    let before = out.len();
    put_obu(&mut out, ObuType::MixPresentation, None, &payload);
    // The payload sits after the header byte and its size.
    let payload_start = before + (out.len() - before - payload.len());
    (out, offsets.map(|at| payload_start + at))
}

/// A mix gain that never changes: its parameter blocks are never sent, so
/// the definition states no durations (mode 1) and the default is all a
/// decoder applies — unity.
fn put_mix_gain(out: &mut Vec<u8>, id: u64, sample_rate: u32) {
    put_leb128(out, id);
    put_leb128(out, u64::from(sample_rate));
    out.push(1 << 7); // param_definition_mode 1
    out.extend_from_slice(&0i16.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SEVEN_ONE_FOUR;
    use std::io::Cursor;

    fn config(codec: Codec) -> Config {
        Config {
            layout: SEVEN_ONE_FOUR,
            codec,
            sample_rate: 48_000,
            bits: 24,
            frame: 256,
            headphones: Headphones::Stereo,
        }
    }

    /// One OBU as read back: its type, its trim, its payload.
    type Read = (u8, Option<(u64, u64)>, Vec<u8>);

    /// Split a sequence into its OBUs.
    fn obus(bytes: &[u8]) -> Vec<Read> {
        fn leb(bytes: &[u8], at: &mut usize) -> u64 {
            let mut value = 0;
            let mut shift = 0;
            loop {
                let b = bytes[*at];
                *at += 1;
                value |= u64::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        }
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let header = bytes[at];
            at += 1;
            let size = leb(bytes, &mut at) as usize;
            let end = at + size;
            let trim = (header & 0b10 != 0).then(|| {
                let e = leb(bytes, &mut at);
                let s = leb(bytes, &mut at);
                (e, s)
            });
            out.push((header >> 3, trim, bytes[at..end].to_vec()));
            at = end;
        }
        out
    }

    /// Descriptors in the order §5.1.1 requires, then units of a delimiter
    /// and one frame per substream, the last one trimmed.
    #[test]
    fn the_sequence_has_the_shape_the_spec_requires() {
        let config = config(Codec::Lpcm);
        let mut writer = Writer::new(Cursor::new(Vec::new()), config).unwrap();
        let channels = config.layout.channels();
        let full: Vec<i32> = (0..256 * channels as i32).collect();
        writer.push(&full).unwrap();
        writer.push(&full[..100 * channels]).unwrap();
        let loudness = Loudness {
            integrated: -23.0,
            digital_peak: -1.0,
            true_peak: -0.5,
        };
        let bytes = writer.finish([loudness; 2]).unwrap().into_inner();

        let obus = obus(&bytes);
        let types: Vec<u8> = obus.iter().map(|o| o.0).collect();
        let unit: Vec<u8> = std::iter::once(4).chain(6..13).collect();
        let mut expected = vec![31, 0, 1, 2];
        expected.extend(&unit);
        expected.extend(&unit);
        assert_eq!(types, expected);

        // Every frame of the last unit trims the padding off its end.
        for (_, trim, payload) in &obus[obus.len() - 7..] {
            assert_eq!(*trim, Some((156, 0)));
            assert_eq!(payload.len() % 3, 0);
        }
        // The first substream is the front pair, interleaved: channel 0 and
        // channel 1 of the first frame, three bytes each, little-endian.
        let first = &obus[5].2;
        assert_eq!(&first[..6], &[0, 0, 0, 1, 0, 0]);
        // The LFE substream, the last one, is mono: channel 3.
        let lfe = &obus[11].2;
        assert_eq!(lfe.len(), 256 * 3);
        assert_eq!(&lfe[..3], &[3, 0, 0]);
    }

    /// The patched loudness lands in the mix presentation's fields.
    #[test]
    fn the_loudness_is_patched_into_the_mix_presentation() {
        let config = config(Codec::Flac);
        let mut writer = Writer::new(Cursor::new(Vec::new()), config).unwrap();
        writer.push(&vec![0; 256 * 12]).unwrap();
        let stereo = Loudness {
            integrated: -24.5,
            digital_peak: -2.0,
            true_peak: -1.75,
        };
        let native = Loudness {
            integrated: -23.0,
            ..stereo
        };
        let bytes = writer.finish([stereo, native]).unwrap().into_inner();
        let mix = &obus(&bytes)[3];
        assert_eq!(mix.0, 2);
        let tail = &mix.2[mix.2.len() - 2 * 8..];
        let q = |b: &[u8]| i16::from_be_bytes([b[0], b[1]]);
        // Stereo first: its layout byte, the type, three fields.
        assert_eq!(tail[0], 2 << 6);
        assert_eq!(tail[1], 1);
        assert_eq!(q(&tail[2..]), -24 * 256 - 128);
        assert_eq!(q(&tail[6..]), -448);
        assert_eq!(tail[8], (2 << 6) | (9 << 2));
        assert_eq!(q(&tail[10..]), -23 * 256);
    }
}
