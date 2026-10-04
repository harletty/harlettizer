//! A standalone IA sequence (IAMF §5): the descriptors, then one temporal
//! unit per frame.
//!
//! What is written is one codec config, the audio elements asked for, and
//! one mix presentation that plays them all at unity, with the loudness
//! measured on 7.1.4 and on the stereo pair every sub-mix has to state.
//!
//! A single channel-based element — a bed — is the simple profile, which
//! every decoder reads. Objects are IAMF v2.0: each one an element of its
//! own, one mono substream, positioned by a parameter the mix presentation
//! declares and parameter blocks animate (see [`crate::position`]); beside
//! them, a channel-based element for what cannot be an object, the LFE.
//! Those sequences are base-advanced (objects only), advanced-1 (up to
//! eighteen channels in all) or advanced-2 (up to twenty-eight), and the
//! sequence header says which.
//!
//! The loudness is a property of the whole programme and sits in a
//! descriptor at the head of the file, so it is written as a placeholder and
//! patched when the programme ends. The fields are fixed-width, so the patch
//! changes no length and nothing after it moves.
//!
//! The same sequence can be written into Matroska instead, as the one track
//! of an `.mka` ([`Writer::matroska`], see [`crate::matroska`]): the
//! descriptors become the track's codec private data and each temporal unit
//! a block.
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
use crate::matroska::{self, Muxer};
use crate::obu::{ObuType, put_leb128, put_obu, q7_8};
#[cfg(feature = "opus")]
use crate::opus;
use crate::position::{self, PositionKind, Subblock};
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

/// One audio element.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Element {
    /// Channels on a loudspeaker layout, one layer.
    Channels(Layout),
    /// One object: a mono substream, positioned by a parameter coded as
    /// `kind`, at `default` — ADM Cartesian — wherever no parameter block
    /// says otherwise.
    Object {
        kind: PositionKind,
        default: [f64; 3],
    },
}

impl Element {
    pub fn channels(&self) -> usize {
        match self {
            Self::Channels(layout) => layout.channels(),
            Self::Object { .. } => 1,
        }
    }

    fn substreams(&self) -> usize {
        match self {
            Self::Channels(layout) => layout.substreams(),
            Self::Object { .. } => 1,
        }
    }
}

/// A run of an object's positions: what one parameter block states.
#[derive(Debug, Clone, Copy)]
pub struct PositionBlock<'a> {
    /// Which element, an object, in the order of [`Config::elements`].
    pub element: usize,
    /// Its subblocks, in order: the block starts with the unit it is pushed
    /// with and lasts as long as they do — past the unit, if they say so.
    pub subblocks: &'a [Subblock],
}

/// One substream, as the writer codes it.
#[derive(Debug, Clone)]
struct Substream {
    /// Channels it carries, as indices into a pushed frame.
    channels: Vec<usize>,
    /// Coded narrowband by a lossy codec.
    #[cfg_attr(not(feature = "opus"), allow(dead_code))]
    lfe: bool,
}

/// What the sequence is.
#[derive(Debug, Clone)]
pub struct Config {
    /// The audio elements, in the order a pushed frame's channels follow
    /// them: each element's channels in its own order.
    pub elements: Vec<Element>,
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
/// The first element's; the rest follow it.
const AUDIO_ELEMENT_ID: u64 = 1;
const MIX_PRESENTATION_ID: u64 = 2;
const OUTPUT_MIX_GAIN_ID: u64 = 101;
/// The first element's mix gain parameter. Each element has its own, above
/// the output's, so that no two parameter substreams share an id.
const ELEMENT_MIX_GAIN_ID: u64 = 1000;
/// The first object's position parameter, likewise.
const POSITION_ID: u64 = 2000;

/// The most elements and channels a v2.0 profile carries: advanced-1, then
/// advanced-2.
const ADVANCED_1: usize = 18;
const ADVANCED_2: usize = 28;

/// A `LoudnessInfo` with a true peak and nothing anchored: the type byte,
/// then three Q7.8 fields.
const LOUDNESS_FIELDS: usize = 6;

/// How the sequence is laid out in the output.
enum Framing {
    /// A standalone IA sequence: the descriptors, then every unit opened by
    /// a temporal delimiter.
    Standalone,
    /// The one track of a Matroska file.
    Matroska(Muxer),
}

/// Writes one IA sequence to `out`.
pub struct Writer<W: Write + Seek> {
    out: W,
    framing: Framing,
    config: Config,
    substreams: Vec<Substream>,
    width: usize,
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
    /// Samples the last unit trims off its end.
    end_trim: usize,
    /// Whether the substreams are closed: a unit trimmed at its end has
    /// been written, and nothing may follow it.
    ended: bool,
}

impl<W: Write + Seek> Writer<W> {
    /// Check `config` and write the descriptors: a standalone IA sequence.
    pub fn new(out: W, config: Config) -> Result<Self, Error> {
        Self::open(out, config, None)
    }

    /// Check `config` and start a Matroska file whose one track, `A_IAMF`,
    /// is the sequence; `writing_app` names what wrote it.
    pub fn matroska(out: W, config: Config, writing_app: &str) -> Result<Self, Error> {
        Self::open(out, config, Some(writing_app))
    }

    fn open(mut out: W, config: Config, matroska: Option<&str>) -> Result<Self, Error> {
        let profile = profile(&config.elements)?;
        let substreams = substreams(&config.elements);
        if substreams.len() > 1 << 16 {
            return Err(Error::Unsupported("that many substreams".into()));
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
            Codec::Opus { bitrate } => opus_coder(&config, &substreams, bitrate)?,
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

        let (descriptors, offsets) = descriptors(&config, profile, &coder, delay);
        let (framing, start) = match matroska {
            None => {
                let start = out.stream_position()?;
                out.write_all(&descriptors)?;
                (Framing::Standalone, start)
            }
            Some(writing_app) => {
                let track = matroska::Track {
                    descriptors: &descriptors,
                    sample_rate: config.sample_rate,
                    frame: config.frame,
                    trimmed: delay,
                    roll_units: roll_units(&coder, config.frame),
                    bits: (!matches!(config.codec, Codec::Opus { .. })).then_some(config.bits),
                    writing_app,
                };
                let (muxer, start) = Muxer::start(&mut out, &track)?;
                (Framing::Matroska(muxer), start)
            }
        };
        let width = config.elements.iter().map(Element::channels).sum();
        Ok(Self {
            channels: vec![vec![0; config.frame]; width],
            out,
            framing,
            config,
            substreams,
            width,
            coder,
            delay,
            loudness_at: offsets.map(|at| start + at as u64),
            unit: Vec::new(),
            payload: Vec::new(),
            units: 0,
            end_trim: 0,
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

    /// Samples a channel the codec's output lags what was pushed: the
    /// pre-skip. A sample pushed at `t` is at `t + delay` on the sequence's
    /// timeline, which is the timeline parameter blocks count on.
    pub fn delay(&self) -> usize {
        self.delay
    }

    /// Write one temporal unit from `interleaved` frames — the elements'
    /// channels in order, each sample already at the configured depth — with
    /// the position blocks that start in it.
    ///
    /// A full frame unless it is the last: a short one is padded with
    /// silence and the padding trimmed at the end, which closes the
    /// substreams — nothing may follow it.
    pub fn push(&mut self, interleaved: &[i32], blocks: &[PositionBlock]) -> io::Result<()> {
        let width = self.width;
        for block in blocks {
            assert!(
                matches!(
                    self.config.elements.get(block.element),
                    Some(Element::Object { .. })
                ),
                "element {} has no position to animate",
                block.element
            );
        }
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
            return self.write_unit(start, 0, blocks);
        }
        // The programme ends here. What is still inside the codec has to
        // come out too, in this unit if the padding has room for it.
        if pad >= self.delay {
            self.write_unit(start, pad - self.delay, blocks)?;
        } else {
            self.write_unit(start, 0, blocks)?;
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
        self.write_unit(0, end, &[])
    }

    /// Code the channels as one unit, trimming `start` samples off its start
    /// and `end` off its end.
    fn write_unit(&mut self, start: usize, end: usize, blocks: &[PositionBlock]) -> io::Result<()> {
        let frame = self.config.frame;
        let trim = (start > 0 || end > 0).then_some((end as u32, start as u32));
        self.unit.clear();
        // A temporal delimiter opens every unit, which lets a reader find the
        // unit boundaries without knowing how many substreams there are. In
        // Matroska the block is the boundary, and the delimiter is left out.
        if let Framing::Standalone = self.framing {
            put_obu(&mut self.unit, ObuType::TemporalDelimiter, None, &[]);
        }
        // The parameter blocks start where the unit does, and come before
        // its audio (IAMF §5.1.2: in order of their implied timestamps).
        for block in blocks {
            let Element::Object { kind, .. } = self.config.elements[block.element] else {
                unreachable!("checked in push");
            };
            self.payload.clear();
            position::put_block(
                &mut self.payload,
                kind,
                POSITION_ID + block.element as u64,
                block.subblocks,
            );
            put_obu(&mut self.unit, ObuType::ParameterBlock, None, &self.payload);
        }
        for (substream, coded) in self.substreams.iter().enumerate() {
            let carried = coded.channels.as_slice();
            self.payload.clear();
            // Past the eighteen ids a frame's type can carry, the id leads
            // the payload.
            if substream > 17 {
                put_leb128(&mut self.payload, substream as u64);
            }
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
            let kind = if substream > 17 {
                ObuType::AudioFrame
            } else {
                ObuType::AudioFrameId(substream as u8)
            };
            put_obu(&mut self.unit, kind, trim, &self.payload);
        }
        match &mut self.framing {
            Framing::Standalone => self.out.write_all(&self.unit)?,
            Framing::Matroska(muxer) => muxer.block(&mut self.out, self.units, &self.unit)?,
        }
        self.units += 1;
        if end > 0 {
            self.end_trim = end;
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
        if let Framing::Matroska(muxer) = &mut self.framing {
            let coded = self.units * self.config.frame as u64;
            let presented = coded.saturating_sub((self.delay + self.end_trim) as u64);
            muxer.finish(&mut self.out, presented)?;
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

/// Units a decoder has to decode before a seek point to be right at it:
/// minus the codec config's `audio_roll_distance`.
#[cfg_attr(not(feature = "opus"), allow(unused_variables))]
fn roll_units(coder: &Coder, frame: usize) -> usize {
    match coder {
        #[cfg(feature = "opus")]
        Coder::Opus { .. } => usize::from(opus::roll_distance(frame).unsigned_abs()),
        _ => 0,
    }
}

/// An encoder per substream, at the rate each one's channels earn.
#[cfg(feature = "opus")]
fn opus_coder(config: &Config, substreams: &[Substream], bitrate: u32) -> Result<Coder, Error> {
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
    let mut encoders = Vec::with_capacity(substreams.len());
    for substream in substreams {
        let lfe = substream.lfe;
        let rate = if lfe {
            bitrate / LFE_SHARE
        } else {
            bitrate * substream.channels.len() as u32
        };
        encoders.push(opus::Encoder::new(opus::Settings {
            channels: substream.channels.len(),
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
fn opus_coder(_: &Config, _: &[Substream], _: u32) -> Result<Coder, Error> {
    Err(Error::Unsupported(
        "Opus; this build has no Opus encoder — build with the `opus` feature, which links \
         libopus"
            .into(),
    ))
}

/// The sequence header's profile for these elements, or why there is none.
///
/// A bed alone is the simple profile. With objects it is v2.0: base-advanced
/// when everything is an object, advanced-1 up to eighteen elements and
/// channels, advanced-2 up to twenty-eight.
fn profile(elements: &[Element]) -> Result<u8, Error> {
    let channels: usize = elements.iter().map(Element::channels).sum();
    let objects = elements
        .iter()
        .filter(|e| matches!(e, Element::Object { .. }))
        .count();
    if elements.is_empty() {
        return Err(Error::Unsupported("a sequence with nothing in it".into()));
    }
    if objects == 0 {
        if elements.len() == 1 && channels <= 16 {
            return Ok(0);
        }
        return Err(Error::Unsupported(format!(
            "{} channel elements and {channels} channels; a bed is one element of at most sixteen",
            elements.len()
        )));
    }
    let size = elements.len().max(channels);
    if size <= ADVANCED_1 {
        Ok(if objects == elements.len() { 3 } else { 4 })
    } else if size <= ADVANCED_2 {
        Ok(5)
    } else {
        Err(Error::Unsupported(format!(
            "{} elements and {channels} channels; IAMF's largest profile, advanced-2, carries \
             {ADVANCED_2}",
            elements.len()
        )))
    }
}

/// Every element's substreams, in order, with the channels each carries.
fn substreams(elements: &[Element]) -> Vec<Substream> {
    let mut out = Vec::new();
    let mut first = 0;
    for element in elements {
        match element {
            Element::Channels(layout) => {
                for substream in 0..layout.substreams() {
                    out.push(Substream {
                        channels: layout
                            .substream_channels(substream)
                            .iter()
                            .map(|c| first + c)
                            .collect(),
                        lfe: layout.is_lfe(substream),
                    });
                }
            }
            Element::Object { .. } => out.push(Substream {
                channels: vec![first],
                lfe: false,
            }),
        }
        first += element.channels();
    }
    out
}

/// The descriptors, and where in them each layout's loudness fields start.
/// `delay` is the pre-skip, which only Opus states.
#[cfg_attr(not(feature = "opus"), allow(unused_variables))]
fn descriptors(config: &Config, profile: u8, coder: &Coder, delay: usize) -> (Vec<u8>, [usize; 2]) {
    let mut out = Vec::with_capacity(256);
    let mut payload = Vec::with_capacity(128);

    // IA sequence header: the one profile it complies with, twice.
    payload.extend_from_slice(b"iamf");
    payload.extend_from_slice(&[profile, profile]);
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

    // The audio elements, their substreams numbered in order across them.
    let mut substream_id = 0u64;
    for (index, element) in config.elements.iter().enumerate() {
        payload.clear();
        put_leb128(&mut payload, AUDIO_ELEMENT_ID + index as u64);
        let element_type = match element {
            Element::Channels(_) => 0,
            Element::Object { .. } => 2,
        };
        payload.push(element_type << 5);
        put_leb128(&mut payload, CODEC_CONFIG_ID);
        put_leb128(&mut payload, element.substreams() as u64);
        for _ in 0..element.substreams() {
            put_leb128(&mut payload, substream_id);
            substream_id += 1;
        }
        // No parameters: a single layer has nothing to demix and nothing to
        // recon, and an object's position belongs to the mix presentation.
        put_leb128(&mut payload, 0);
        match element {
            Element::Channels(layout) => {
                payload.push(1 << 5); // num_layers
                // loudspeaker_layout, no output gain, no recon gain.
                payload.push(layout.loudspeaker_layout << 4);
                payload.push(layout.substreams() as u8);
                payload.push(layout.coupled as u8);
                if let Some(expanded) = layout.expanded {
                    payload.push(expanded);
                }
            }
            Element::Object { .. } => {
                // ObjectsConfig: its size, then one object.
                put_leb128(&mut payload, 1);
                payload.push(1);
            }
        }
        put_obu(&mut out, ObuType::AudioElement, None, &payload);
    }

    // Mix presentation: every element at unity, measured on stereo and on
    // the layout the programme was rendered to for it.
    payload.clear();
    put_leb128(&mut payload, MIX_PRESENTATION_ID);
    put_leb128(&mut payload, 0); // count_label: no annotations
    put_leb128(&mut payload, 1); // num_sub_mixes
    put_leb128(&mut payload, config.elements.len() as u64);
    let headphones = match config.headphones {
        Headphones::Stereo => 0u8,
        Headphones::Binaural => 1,
    };
    let mut extension = Vec::with_capacity(16);
    for (index, element) in config.elements.iter().enumerate() {
        put_leb128(&mut payload, AUDIO_ELEMENT_ID + index as u64);
        // headphones_rendering_mode; no element gain offset; the ambient
        // binaural filter profile; reserved.
        payload.push(headphones << 6);
        // The rendering config's extension, which is where v2.0 put an
        // object's position so that a v1.1 parser steps over it.
        extension.clear();
        if let Element::Object { kind, default } = element {
            put_leb128(&mut extension, 1); // num_parameters
            put_leb128(&mut extension, kind.param_definition_type());
            position::put_definition(
                &mut extension,
                *kind,
                POSITION_ID + index as u64,
                config.sample_rate,
                *default,
            );
        }
        put_leb128(&mut payload, extension.len() as u64);
        payload.extend_from_slice(&extension);
        put_mix_gain(
            &mut payload,
            ELEMENT_MIX_GAIN_ID + index as u64,
            config.sample_rate,
        );
    }
    put_mix_gain(&mut payload, OUTPUT_MIX_GAIN_ID, config.sample_rate);
    put_leb128(&mut payload, 2); // num_layouts
    // A bed is measured on its own layout; anything with objects on the
    // 7.1.4 it was rendered to for the measurement.
    let measured = match config.elements.as_slice() {
        [Element::Channels(layout)] => layout.sound_system,
        _ => crate::layout::SEVEN_ONE_FOUR.sound_system,
    };
    let mut offsets = [0usize; 2];
    for (offset, sound_system) in offsets.iter_mut().zip([STEREO_SOUND_SYSTEM, measured]) {
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
            elements: vec![Element::Channels(SEVEN_ONE_FOUR)],
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
        let channels = 12;
        let mut writer = Writer::new(Cursor::new(Vec::new()), config.clone()).unwrap();
        let full: Vec<i32> = (0..256 * channels as i32).collect();
        writer.push(&full, &[]).unwrap();
        writer.push(&full[..100 * channels], &[]).unwrap();
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
        let mut writer = Writer::new(Cursor::new(Vec::new()), config.clone()).unwrap();
        writer.push(&vec![0; 256 * 12], &[]).unwrap();
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
        let width = 12;
        for length in [960 * 3 + 100, 960 * 3 + 900, 960 * 3, 500] {
            let mut writer = Writer::new(Cursor::new(Vec::new()), config.clone()).unwrap();
            let delay = writer.delay;
            let mut left = length;
            while left > 0 {
                let n = left.min(960);
                writer.push(&vec![1000; n * width], &[]).unwrap();
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

    fn object(kind: PositionKind) -> Element {
        Element::Object {
            kind,
            default: [0.0, 1.0, 0.0],
        }
    }

    /// The header names the profile the elements need: a bed alone is
    /// simple, objects alone base-advanced, objects beside a channel element
    /// advanced-1 up to eighteen channels and advanced-2 up to twenty-eight,
    /// and past that there is none.
    #[test]
    fn the_profile_follows_the_elements() {
        use crate::layout::LFE;
        let objects = |n: usize| vec![object(PositionKind::Cart8); n];
        assert_eq!(profile(&[Element::Channels(SEVEN_ONE_FOUR)]).unwrap(), 0);
        assert_eq!(profile(&objects(18)).unwrap(), 3);
        let mut mixed = vec![Element::Channels(LFE)];
        mixed.extend(objects(17));
        assert_eq!(profile(&mixed).unwrap(), 4);
        mixed.push(object(PositionKind::Cart8));
        assert_eq!(profile(&mixed).unwrap(), 5);
        mixed.extend(objects(9));
        assert_eq!(mixed.len(), 28);
        assert_eq!(profile(&mixed).unwrap(), 5);
        mixed.push(object(PositionKind::Cart8));
        assert!(profile(&mixed).is_err());
        // A 7.1.4 bed counts its twelve channels: sixteen objects beside it
        // fill advanced-2, a seventeenth does not fit.
        let mut bed = vec![Element::Channels(SEVEN_ONE_FOUR)];
        bed.extend(objects(16));
        assert_eq!(profile(&bed).unwrap(), 5);
        bed.push(object(PositionKind::Cart8));
        assert!(profile(&bed).is_err());
    }

    /// An LFE and nineteen objects: twenty substreams, so the last two
    /// state their ids; position blocks before the audio of the unit they
    /// start in; the LFE element an expanded layout.
    #[test]
    fn objects_beside_an_lfe_are_written_as_v2_elements() {
        use crate::layout::LFE;
        let mut elements = vec![Element::Channels(LFE)];
        elements.extend(vec![object(PositionKind::Cart16); 19]);
        let config = Config {
            elements,
            ..config(Codec::Lpcm)
        };
        let mut writer = Writer::new(Cursor::new(Vec::new()), config).unwrap();
        let subblocks = [Subblock {
            duration: 256,
            animation: crate::position::Animation::Linear([-1.0, 1.0, 0.0], [1.0, 1.0, 0.0]),
        }];
        writer
            .push(
                &vec![7; 256 * 20],
                &[PositionBlock {
                    element: 3,
                    subblocks: &subblocks,
                }],
            )
            .unwrap();
        let bytes = writer
            .finish(
                [Loudness {
                    integrated: -20.0,
                    digital_peak: -1.0,
                    true_peak: -1.0,
                }; 2],
            )
            .unwrap()
            .into_inner();
        let obus = obus(&bytes);
        // Header (profile advanced-2), codec, 20 elements, the mix.
        assert_eq!(obus[0].0, 31);
        assert_eq!(&obus[0].2[4..], &[5, 5]);
        assert!(obus[2..22].iter().all(|o| o.0 == 1));
        // The LFE element: channel-based, one layer, layout 15, one mono
        // substream, expanded layout 0.
        let lfe = &obus[2].2;
        assert_eq!(&lfe[lfe.len() - 5..], &[1 << 5, 15 << 4, 1, 0, 0]);
        // An object element: type 2, then ObjectsConfig of size 1 holding 1.
        let first_object = &obus[3].2;
        assert_eq!(first_object[1], 2 << 5);
        assert_eq!(&first_object[first_object.len() - 2..], &[1, 1]);
        // The unit: a delimiter, the block, then twenty frames, ids 0 to 17
        // in the type and the last two in the payload.
        let unit = &obus[23..];
        assert_eq!(unit[0].0, 4);
        assert_eq!(unit[1].0, 3);
        let block = &unit[1].2;
        // POSITION_ID + 3 = 2003, a two-byte LEB128.
        assert_eq!(&block[..2], &[0xd3, 0x0f]);
        let types: Vec<u8> = unit[2..].iter().map(|o| o.0).collect();
        let mut expected: Vec<u8> = (6..24).collect();
        expected.extend([5, 5]);
        assert_eq!(types, expected);
        assert_eq!(unit[20].2[0], 18);
        assert_eq!(unit[21].2[0], 19);
    }
}
