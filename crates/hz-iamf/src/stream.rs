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
//!
//! # A codec with a delay
//!
//! Opus hands each sample back a lookahead late, so its timeline is shifted:
//! the first unit trims that many samples off its start (the pre-skip), and
//! the programme's last samples come out only once the encoder has been fed
//! that much more — silence, in a unit of its own if the last one has no room
//! — trimmed off the end. The lossless codecs have no delay and the same
//! arithmetic gives them a trim at the end of a short last unit and nothing
//! else.

use crate::flac;
use crate::layout::{Layout, STEREO_SOUND_SYSTEM};
use crate::obu::{ObuType, put_leb128, put_obu, q7_8};
#[cfg(feature = "opus")]
use crate::opus;
use std::fmt;
use std::io::{self, Seek, SeekFrom, Write};

/// How the substreams are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Linear PCM, little-endian: no compression, nothing to get wrong.
    Lpcm,
    /// FLAC, lossless.
    Flac,
    /// Opus, lossy, at `bitrate` bits a second for each channel: a coupled
    /// pair is coded at twice it, the LFE at a quarter. Needs the `opus`
    /// feature; without it [`Writer::new`] refuses it.
    Opus { bitrate: u32 },
}

impl Codec {
    fn four_cc(self) -> &'static [u8; 4] {
        match self {
            Self::Lpcm => b"ipcm",
            Self::Flac => b"fLaC",
            Self::Opus { .. } => b"Opus",
        }
    }
}

/// The LFE's share of a channel's bitrate. It carries a band a few hundred
/// hertz wide; coded narrowband, a quarter is generous.
#[cfg(feature = "opus")]
const LFE_SHARE: u32 = 4;

/// The fewest bits a second an Opus substream is given, however low the
/// rate asked: below this libopus stops being music-grade.
#[cfg(feature = "opus")]
const MIN_SUBSTREAM_BITRATE: u32 = 12_000;

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

#[cfg(feature = "opus")]
impl From<opus::OpusError> for Error {
    fn from(e: opus::OpusError) -> Self {
        Self::Unsupported(e.to_string())
    }
}

/// The coder behind each codec, with whatever state it keeps.
enum Coder {
    Lpcm,
    Flac(flac::Encoder),
    #[cfg(feature = "opus")]
    Opus {
        /// One per substream: Opus is stateful.
        encoders: Vec<opus::Encoder>,
        /// One substream's samples, interleaved and scaled to ±1.
        scratch: Vec<f32>,
    },
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
    coder: Coder,
    /// Samples a channel the codec's output lags its input by.
    delay: usize,
    /// Where, in `out`, each layout's loudness fields start: stereo, then
    /// the element's own layout.
    loudness_at: [u64; 2],
    /// One channel of the unit being written, deinterleaved.
    channels: Vec<Vec<i32>>,
    unit: Vec<u8>,
    payload: Vec<u8>,
    units: u64,
    /// Whether the substreams are closed: a unit trimmed at its end has
    /// been written, and nothing may follow it.
    ended: bool,
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
        let coder = match config.codec {
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
                Coder::Lpcm
            }
            Codec::Flac => Coder::Flac(flac::Encoder::new(
                config.sample_rate,
                config.bits,
                config.frame,
            )?),
            Codec::Opus { bitrate } => opus_coder(&config, bitrate)?,
        };
        let delay = match &coder {
            #[cfg(feature = "opus")]
            Coder::Opus { encoders, .. } => encoders[0].lookahead(),
            _ => 0,
        };
        if delay >= config.frame {
            return Err(Error::Unsupported(format!(
                "{}-sample frames under a codec delay of {delay}; a frame has to be longer than \
                 the samples its first one trims",
                config.frame
            )));
        }

        let start = out.stream_position()?;
        let (descriptors, offsets) = descriptors(&config, &coder, delay);
        out.write_all(&descriptors)?;
        Ok(Self {
            out,
            config,
            coder,
            delay,
            loudness_at: offsets.map(|at| start + at as u64),
            channels: vec![vec![0; config.frame]; config.layout.channels()],
            unit: Vec::new(),
            payload: Vec::new(),
            units: 0,
            ended: false,
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
            !self.ended,
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
        let start = if self.units == 0 { self.delay } else { 0 };
        let pad = frame - frames;
        if pad == 0 {
            return self.write_unit(start, 0);
        }
        // The programme ends here. What is still inside the codec has to
        // come out too, in this unit if the padding has room for it.
        if pad >= self.delay {
            self.write_unit(start, pad - self.delay)?;
        } else {
            self.write_unit(start, 0)?;
            self.write_silent_unit(frame - (self.delay - pad))?;
        }
        self.ended = true;
        Ok(())
    }

    /// A unit of silence, trimmed by `end` at its end: what flushes a
    /// codec's delay out after the programme.
    fn write_silent_unit(&mut self, end: usize) -> io::Result<()> {
        for channel in &mut self.channels {
            channel.fill(0);
        }
        self.write_unit(0, end)
    }

    /// Code the channels as one unit, trimming `start` samples off its start
    /// and `end` off its end.
    fn write_unit(&mut self, start: usize, end: usize) -> io::Result<()> {
        let frame = self.config.frame;
        let trim = (start > 0 || end > 0).then_some((end as u32, start as u32));
        self.unit.clear();
        // A temporal delimiter opens every unit, which lets a reader find the
        // unit boundaries without knowing how many substreams there are.
        put_obu(&mut self.unit, ObuType::TemporalDelimiter, None, &[]);
        let layout = self.config.layout;
        for substream in 0..layout.substreams() {
            let carried = layout.substream_channels(substream);
            self.payload.clear();
            match &mut self.coder {
                Coder::Lpcm => {
                    let width = self.config.bits as usize / 8;
                    for i in 0..frame {
                        for &c in carried {
                            let bytes = self.channels[c][i].to_le_bytes();
                            self.payload.extend_from_slice(&bytes[..width]);
                        }
                    }
                }
                Coder::Flac(encoder) => {
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
                #[cfg(feature = "opus")]
                Coder::Opus { encoders, scratch } => {
                    let scale = 1.0 / f64::from(1u32 << (self.config.bits - 1));
                    scratch.clear();
                    for i in 0..frame {
                        for &c in carried {
                            scratch.push((f64::from(self.channels[c][i]) * scale) as f32);
                        }
                    }
                    let packet = encoders[substream]
                        .encode(scratch)
                        .map_err(io::Error::other)?;
                    self.payload.extend_from_slice(packet);
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
        if end > 0 {
            self.ended = true;
        }
        Ok(())
    }

    /// State the programme's loudness — on the stereo pair, then on the
    /// element's own layout — and hand back the output.
    pub fn finish(mut self, loudness: [Loudness; 2]) -> io::Result<W> {
        // A programme that ended on a whole unit still has the codec's delay
        // to flush.
        if !self.ended && self.units > 0 && self.delay > 0 {
            self.write_silent_unit(self.config.frame - self.delay)?;
        }
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

/// An encoder per substream, at the rate each one's channels earn.
#[cfg(feature = "opus")]
fn opus_coder(config: &Config, bitrate: u32) -> Result<Coder, Error> {
    if config.sample_rate != opus::SAMPLE_RATE {
        return Err(Error::Unsupported(format!(
            "{} Hz into Opus; IAMF runs Opus at 48 kHz and this does not resample",
            config.sample_rate
        )));
    }
    if !opus::is_frame_size(config.frame) {
        return Err(Error::Unsupported(format!(
            "{}-sample Opus frames; a packet is 120, 240, 480, 960, 1920 or 2880 samples",
            config.frame
        )));
    }
    if ![16, 24, 32].contains(&config.bits) {
        return Err(Error::Unsupported(format!(
            "{}-bit samples into Opus; 16, 24 or 32",
            config.bits
        )));
    }
    let layout = config.layout;
    let mut encoders = Vec::with_capacity(layout.substreams());
    for substream in 0..layout.substreams() {
        let carried = layout.substream_channels(substream);
        let lfe = carried.len() == 1 && layout.labels[carried[0]].starts_with("LFE");
        let rate = if lfe {
            bitrate / LFE_SHARE
        } else {
            bitrate * carried.len() as u32
        };
        encoders.push(opus::Encoder::new(opus::Settings {
            channels: carried.len(),
            bitrate: rate.max(MIN_SUBSTREAM_BITRATE),
            lfe,
        })?);
    }
    // Every substream trims the same pre-skip, so every encoder has to have
    // the same delay; libopus's depends only on the rate and application.
    let delay = encoders[0].lookahead();
    if encoders.iter().any(|e| e.lookahead() != delay) {
        return Err(Error::Unsupported(
            "Opus encoders with different delays in one element".into(),
        ));
    }
    Ok(Coder::Opus {
        encoders,
        scratch: Vec::with_capacity(config.frame * 2),
    })
}

#[cfg(not(feature = "opus"))]
fn opus_coder(_: &Config, _: u32) -> Result<Coder, Error> {
    Err(Error::Unsupported(
        "Opus; this build has no Opus encoder — build with the `opus` feature, which links \
         libopus"
            .into(),
    ))
}

/// The descriptors, and where in them each layout's loudness fields start.
/// `delay` is the pre-skip, which only Opus states.
#[cfg_attr(not(feature = "opus"), allow(unused_variables))]
fn descriptors(config: &Config, coder: &Coder, delay: usize) -> (Vec<u8>, [usize; 2]) {
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
    match coder {
        Coder::Lpcm => {
            // audio_roll_distance: nought for a lossless codec (IAMF §3.11).
            payload.extend_from_slice(&0i16.to_be_bytes());
            // Little-endian, the depth, the rate.
            payload.push(1);
            payload.push(config.bits as u8);
            payload.extend_from_slice(&config.sample_rate.to_be_bytes());
        }
        Coder::Flac(encoder) => {
            payload.extend_from_slice(&0i16.to_be_bytes());
            payload.extend_from_slice(&encoder.decoder_config());
        }
        #[cfg(feature = "opus")]
        Coder::Opus { .. } => {
            payload.extend_from_slice(&opus::roll_distance(config.frame).to_be_bytes());
            payload.extend_from_slice(&opus::decoder_config(delay as u16, config.sample_rate));
        }
    }
    put_obu(&mut out, ObuType::CodecConfig, None, &payload);

    // Audio element: channel-based, one layer, no parameters — a single
    // layer has nothing to demix and nothing to recon.
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

    /// Opus's timeline: the first unit trims the pre-skip off its start, the
    /// last trims whatever overhangs the programme off its end, and what is
    /// left is exactly the programme — whether it ends with room in its last
    /// unit for the delay, without room, or on a whole unit.
    #[cfg(feature = "opus")]
    #[test]
    fn opus_trims_its_delay_at_both_ends() {
        let config = Config {
            codec: Codec::Opus { bitrate: 64_000 },
            frame: 960,
            ..config(Codec::Lpcm)
        };
        let width = config.layout.channels();
        for length in [960 * 3 + 100, 960 * 3 + 900, 960 * 3, 500] {
            let mut writer = Writer::new(Cursor::new(Vec::new()), config).unwrap();
            let delay = writer.delay;
            let mut left = length;
            while left > 0 {
                let n = left.min(960);
                writer.push(&vec![1000; n * width]).unwrap();
                left -= n;
            }
            let silence = Loudness {
                integrated: -70.0,
                digital_peak: -70.0,
                true_peak: -70.0,
            };
            let bytes = writer.finish([silence; 2]).unwrap().into_inner();
            let frames: Vec<_> = obus(&bytes).into_iter().filter(|o| o.0 == 6).collect();
            let kept: u64 = frames
                .iter()
                .map(|(_, trim, _)| {
                    let (end, start) = trim.unwrap_or((0, 0));
                    960 - end - start
                })
                .sum();
            assert_eq!(kept, length as u64, "{length}");
            assert_eq!(frames[0].1.map(|t| t.1), Some(delay as u64), "{length}");
            // Only the last unit trims its end.
            for (i, (_, trim, _)) in frames.iter().enumerate() {
                let end = trim.map_or(0, |t| t.0);
                assert_eq!(end > 0, i + 1 == frames.len(), "{length}: unit {i}");
            }
        }
    }
}
