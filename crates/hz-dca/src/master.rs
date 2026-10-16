//! DTS-HD Master Audio: a core and a lossless layer in one stream.
//!
//! Each frame is a core frame ([`crate::encoder`]) followed by an extension
//! substream ([`crate::exss`]) carrying one audio asset whose only component
//! is the lossless layer ([`crate::lossless`]). The core is the 5.1 a
//! decoder of the lossless layer's first channel set alone presents — the
//! input itself for a 5.1, the fold for a 7.1 — so a core-only decoder and a
//! 5.1 lossless decoder play the same mix.
//!
//! The lossless layer codes each channel the core carries as its difference
//! from the core's reconstruction where that is cheaper
//! ([`lossless::Config::residual`]), so the encoder decodes every core frame
//! it sends as a decoder will ([`CoreDecoder`]). A decoder's core output is
//! a frame behind its input, so the core runs a frame ahead of the lossless
//! layer: the stream's frame *n* carries the core encoded from input frame
//! *n* + 1, which decodes to input frame *n*, beside the lossless frame of
//! input frame *n*. The core frame made from the first input frame decodes to
//! the silence before it and is not sent, and [`Encoder::finish`] sends the
//! last input frame with a core made from silence. So the stream has a frame
//! per input frame, and both layers decode in step with the input — the
//! lossless layer to the sample. With the residual off, every channel is
//! coded as itself and the layers are independent, but stay in step.
//!
//! The substream header is the reference streams' (one presentation, one
//! asset, the core backward-compatible in the core substream). The stream is
//! held under a peak bit rate ceiling, the core and every header included
//! ([`peak::Config`], 24.5 Mb/s by default). A lossless frame goes whole in
//! its own substream, so the substreams vary in size frame to frame as the
//! reference streams' do; only a frame too large for its own substream has
//! part of it sent ahead, in the substreams before it, into the decoder's
//! smoothing buffer ([`peak::Smoother`]). Where no frame the lossless layer
//! can write is too large — at the default ceiling, any — the stream states
//! no smoothing buffer and every frame goes out as soon as it is encoded.
//! Otherwise the encoder looks [`peak::Config::lookahead`] frames ahead, and
//! a frame that cannot reach the decoder in time is refused with
//! [`Refused::PeakRate`], naming it: lossless data is never coded coarser,
//! and the stream ends before it.
//!
//! A 7.1.4 is the DTS:X extension's standard profile ([`crate::dtsx`]): the
//! stream above, its 7.1 the composite the heights are folded into, and at
//! the end of each lossless frame the block that carries the heights. The
//! asset still states the 7.1; its descriptor's tail carries the navigation
//! word that locates the block. A frame the word cannot locate is refused
//! with [`Refused::Navigation`], naming it, and the stream ends there: the
//! frame is not cut to fit, and the stream cannot go on without it.

use crate::bits::Bits;
use crate::core_decode::CoreDecoder;
use crate::encoder;
use crate::encoder::FRAME_SAMPLES;
use crate::exss::{
    AssetDescriptor, AssetStatics, ChannelMapping, Coding, ExssHeader, Presentation, StaticFields,
    Xll, XllSync, component,
};
use crate::lossless::{self, Layout};
use crate::navigation::Navigation;
use crate::peak::{self, Smoother};
use std::collections::VecDeque;

pub use crate::encoder::Unsupported;
pub use crate::lossless::Refused;

/// What to encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub lossless: lossless::Config,
    /// The core: 5.1 at 48 kHz, whatever else it is.
    pub core: encoder::Config,
    /// The peak bit rate the stream is held under.
    pub peak: peak::Config,
}

impl Config {
    /// The reference streams' shape: the lossless layer as
    /// [`lossless::Config::reference`] lays it out, and a 1509 kb/s core
    /// stating the source's word length as they do — 6 for 24 bits, 0 for
    /// sixteen — under the default ceiling.
    pub fn reference(layout: Layout, bits: u8) -> Self {
        Self {
            lossless: lossless::Config::reference(layout, bits),
            core: encoder::Config {
                pcm_resolution_code: if bits > 16 { 6 } else { 0 },
                ..encoder::Config::reference_five_one()
            },
            peak: peak::Config::default(),
        }
    }
}

/// The largest substream the wide size field describes, and the largest
/// header its 12-bit header size does.
const WIDEST_SUBSTREAM: usize = 1 << 20;
const MAX_HEADER: usize = 1 << 12;

/// The most frames the encoder looks ahead: about twelve minutes.
const MAX_LOOKAHEAD: usize = 1 << 16;

/// How the stream has used its ceiling so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rate {
    /// The most bytes a frame may take under the ceiling.
    pub budget: usize,
    /// The largest frame sent, core and headers included.
    pub largest: usize,
    /// The smoothing buffer the stream states, in bytes: 0 when no frame
    /// can need it.
    pub buffer: usize,
    /// The most bytes the decoder's buffer has held at once.
    pub held: u64,
    /// Frames whose substream carried other than exactly their own
    /// lossless frame.
    pub ahead: u64,
}

/// The encoder.
pub struct Encoder {
    core: encoder::Encoder,
    lossless: lossless::Encoder,
    exss: ExssHeader,
    /// The core as a decoder of the stream reconstructs it.
    decoder: CoreDecoder,
    /// The input frame waiting for the core frame that decodes to it, and
    /// whether there is one.
    pending: Vec<i32>,
    waiting: bool,
    /// The core frame last encoded.
    core_frame: Vec<u8>,
    /// Frames encoded.
    frames: u64,
    /// Bytes of the lossless frames encoded, and how many of their channels
    /// were coded against the core.
    lossless_bytes: u64,
    against_core: u64,
    /// The furthest offset and the largest DTS:X block navigated so far.
    reach: Option<(usize, usize)>,
    /// The frames encoded and not yet sent: their cores end to end, the
    /// lossless stream from the first byte not sent, and each frame's
    /// navigation word when it has one.
    cores: VecDeque<u8>,
    lossless_stream: VecDeque<u8>,
    words: VecDeque<Option<u64>>,
    smoother: Smoother,
    /// The buffer code the substreams state.
    buffer_code: u8,
    rate: Rate,
    /// Once a frame cannot be carried, the stream has ended: the refusal.
    ended: Option<Refused>,
}

/// Fit `exss` around `size` bytes of data: narrow fields where the whole
/// substream fits them, wide otherwise, and the header padded to a 32-bit
/// word as every reference stream's is. Its bytes.
fn header(exss: &mut ExssHeader, size: u32) -> Vec<u8> {
    exss.wide = false;
    loop {
        exss.reserved = Bits::new();
        exss.fit()
            .expect("a lossless frame fits a substream's wide fields");
        // Every reference stream's header ends on a 32-bit word — 28 to 44
        // bytes, never between — so that what follows starts on one: zeroes
        // in the reserved bits to get there.
        let short = (4 - exss.header_size % 4) % 4;
        if short > 0 {
            exss.reserved.pad(8 * short as usize);
            exss.fit()
                .expect("a lossless frame fits a substream's wide fields");
        }
        exss.size = exss.header_size + size;
        match exss.write() {
            Ok(header) => return header,
            Err(_) if !exss.wide => exss.wide = true,
            Err(e) => panic!("a fitted substream header does not write: {}", e.0),
        }
    }
}

impl Encoder {
    pub fn new(config: Config) -> Result<Self, Unsupported> {
        let bits = config.lossless.bits;
        if config.core.layout != encoder::Layout::FivePointOne || config.core.sample_rate != 48000 {
            return Err(Unsupported(format!(
                "a {:?} core at {} Hz: the lossless layer sits beside a 5.1 core at 48 kHz",
                config.core.layout, config.core.sample_rate
            )));
        }
        let lossless = lossless::Encoder::new(config.lossless).map_err(|e| Unsupported(e.0))?;
        let core = encoder::Encoder::new(config.core)?;
        let layout = lossless.config().layout;
        let exss = ExssHeader {
            user_defined: 0,
            index: 0,
            wide: false,
            header_size: 0,
            size: 0,
            statics: Some(StaticFields {
                // 48 kHz; frames of 512 samples of it.
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
            asset_sizes: vec![1],
            assets: vec![AssetDescriptor {
                size: 0,
                index: 0,
                statics: Some(AssetStatics {
                    asset_type: None,
                    language: None,
                    text: None,
                    pcm_bit_res: bits,
                    max_sample_rate_code: 12,
                    channels: layout.bed_channels() as u16,
                    mapping: ChannelMapping::OneToOne {
                        embedded_stereo: false,
                        embedded_6ch: layout != Layout::FivePointOne,
                        speaker_mask: Some(lossless.speaker_activity()),
                        remaps: vec![],
                    },
                }),
                drc_code: None,
                dialnorm_code: None,
                stereo_drc_code: None,
                mixing: None,
                coding: Coding::Components {
                    // The core in the core substream, the lossless layer here.
                    mask: 0x001 | component::XLL,
                    core: None,
                    xbr: None,
                    xxch: None,
                    x96: None,
                    lbr: None,
                    xll: Some(Xll {
                        size: 1,
                        sync: Some(XllSync {
                            peak_buffer_code: 0,
                            delay_bits: 1,
                            delay_frames: 0,
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

        // The room a substream has for lossless data under the ceiling: the
        // frame's budget, less the core and the largest header the stream
        // can need — sync fields at their widest, the navigation word.
        let peak = config.peak;
        if peak.buffer_code > 15 {
            return Err(Unsupported(format!(
                "a smoothing buffer code of {}: the field holds 0 to 15",
                peak.buffer_code
            )));
        }
        if peak.lookahead > MAX_LOOKAHEAD {
            return Err(Unsupported(format!(
                "a lookahead of {} frames: {MAX_LOOKAHEAD} at most",
                peak.lookahead
            )));
        }
        let budget = peak::frame_budget(peak.ceiling, FRAME_SAMPLES as u32, 48000);
        let unsupported = |overhead: usize| {
            Unsupported(format!(
                "a ceiling of {} b/s: {budget} bytes a frame, which the core and the headers \
                 ({overhead} bytes) leave no room in for the lossless layer",
                peak.ceiling
            ))
        };
        if budget < core.frame_size() + 2 {
            return Err(unsupported(core.frame_size()));
        }
        // The most lossless data a substream can carry: what the budget
        // leaves beside the core, and no more than the wide size field
        // describes with the largest header around it.
        let largest = (budget - core.frame_size()).min(WIDEST_SUBSTREAM - MAX_HEADER) as u32;
        let mut widest = exss.clone();
        widest.asset_sizes[0] = largest;
        if let Coding::Components { xll: Some(x), .. } = &mut widest.assets[0].coding {
            x.size = largest;
            let delay = peak.lookahead as u32 + 1;
            x.sync = Some(XllSync {
                peak_buffer_code: peak.buffer_code,
                delay_bits: peak::delay_bits(delay),
                delay_frames: delay,
                sync_offset: largest - 1,
            });
        }
        if layout == Layout::SevenPointOneFour {
            widest.assets[0].tail = tail(0);
        }
        let overhead = core.frame_size() + header(&mut widest, largest).len();
        let room = budget.saturating_sub(overhead).min(largest as usize);
        if room < 2 {
            return Err(unsupported(overhead));
        }
        // Smoothing only where a frame can need it.
        let smoothing = peak.buffer_code > 0 && lossless.frame_bound() > room;
        let buffer_code = if smoothing { peak.buffer_code } else { 0 };
        let smoother = Smoother::new(
            room,
            peak::buffer_bytes(buffer_code),
            if smoothing { peak.lookahead } else { 0 },
        );
        let rate = Rate {
            budget,
            buffer: peak::buffer_bytes(buffer_code),
            ..Rate::default()
        };
        Ok(Self {
            pending: vec![0; FRAME_SAMPLES * layout.channels()],
            waiting: false,
            core_frame: Vec::with_capacity(core.frame_size()),
            frames: 0,
            lossless_bytes: 0,
            against_core: 0,
            reach: None,
            decoder: CoreDecoder::new(),
            cores: VecDeque::new(),
            lossless_stream: VecDeque::new(),
            words: VecDeque::new(),
            smoother,
            buffer_code,
            rate,
            ended: None,
            core,
            lossless,
            exss,
        })
    }

    /// Take one frame — [`FRAME_SAMPLES`] samples of every channel,
    /// interleaved in WAV order as 32-bit samples whose bits below the
    /// configured resolution are zero — and append to `out` the stream's
    /// frames that are ready: the frame before this one, since the core runs
    /// a frame ahead (nothing on the first call), and, where the stream may
    /// smooth, only once the encoder has looked far enough past it.
    /// [`Encoder::finish`] sends the rest.
    ///
    /// After [`Refused::PeakRate`] the stream has ended: every later call
    /// returns the same refusal.
    ///
    /// # Panics
    /// If `pcm` is not exactly one frame.
    pub fn encode(&mut self, pcm: &[i32], out: &mut Vec<u8>) -> Result<(), Refused> {
        assert_eq!(
            pcm.len(),
            self.pending.len(),
            "a frame is {FRAME_SAMPLES} samples of each channel"
        );
        if let Some(refused) = &self.ended {
            return Err(refused.clone());
        }
        // Refused before anything moves on, naming the input frame: the
        // lossless layer has coded the frames sent, not the one waiting.
        self.lossless
            .check(pcm)
            .map_err(|e| e.of_frame(self.frames + u64::from(self.waiting)))?;
        self.core_frame.clear();
        self.core
            .encode(self.lossless.core_input(pcm), &mut self.core_frame);
        if !self.waiting {
            // What this core frame decodes to is the silence before the
            // input: it is not sent, and the stream's decoder starts its
            // subband prediction on the next from silence.
            self.core.restart_prediction();
        } else {
            self.send(out)?;
        }
        self.pending.copy_from_slice(pcm);
        self.waiting = true;
        Ok(())
    }

    /// Encode the frame still waiting, with a core made from silence, and
    /// send every frame not yet sent.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
        if let Some(refused) = &self.ended {
            return Err(refused.clone());
        }
        if self.waiting {
            let silence = vec![0; self.pending.len()];
            self.core_frame.clear();
            self.core
                .encode(self.lossless.core_input(&silence), &mut self.core_frame);
            self.send(out)?;
            self.waiting = false;
        }
        self.emit(out, true)
    }

    /// Encode the stream's frame for the pending input — the core frame last
    /// encoded and the lossless frame — put it in the queue, and send what
    /// is ready.
    fn send(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
        self.decoder.decode(&self.core);
        // FFmpeg's decoder treats the first frame of a stream that codes
        // channels against the core in more than one channel set as one to
        // recover from, and presents it from the core alone (as it does
        // after a seek). A first frame that codes every channel as itself
        // has nothing to recover, so the stream decodes losslessly from its
        // first sample.
        let several_sets = self.lossless.config().layout != Layout::FivePointOne;
        let core = (self.frames > 0 || !several_sets).then_some(&self.decoder);
        let frame = self.lossless.encode(&self.pending, core)?;
        let size = frame.len();
        self.lossless_stream.extend(frame);
        let word = self.lossless.extension().map(|navigation: Navigation| {
            let (offset, size) = self.reach.unwrap_or_default();
            self.reach = Some((offset.max(navigation.offset), size.max(navigation.size)));
            navigation
                .word()
                .expect("the lossless encoder refuses a block it cannot navigate")
        });
        self.words.push_back(word);
        self.cores.extend(&self.core_frame);
        self.smoother.push(size);
        self.frames += 1;
        self.lossless_bytes += size as u64;
        self.against_core += u64::from(self.lossless.against_core());
        self.emit(out, false)
    }

    /// Send the frames whose substreams can be decided — all of them when
    /// `flush` — each as its core frame, the substream header and the bytes
    /// of the lossless stream the smoother gives it.
    fn emit(&mut self, out: &mut Vec<u8>, flush: bool) -> Result<(), Refused> {
        while self.smoother.ready() || (flush && self.smoother.waiting() > 0) {
            let carry = match self.smoother.carry().expect("a frame is waiting") {
                Ok(carry) => carry,
                Err((frame, cause)) => {
                    let refused = Refused::PeakRate { frame, cause };
                    self.ended = Some(refused.clone());
                    return Err(refused);
                }
            };
            let bytes = carry.bytes as u32;
            self.exss.asset_sizes[0] = bytes;
            if let Coding::Components { xll: Some(x), .. } = &mut self.exss.assets[0].coding {
                x.size = bytes;
                x.sync = carry.sync.map(|s| XllSync {
                    peak_buffer_code: self.buffer_code,
                    delay_bits: peak::delay_bits(s.delay),
                    delay_frames: s.delay,
                    sync_offset: s.offset,
                });
            }
            // The word, when the frame has one, describes the frame the
            // decoder takes out of its buffer with this substream.
            self.exss.assets[0].tail = match self.words.pop_front().flatten() {
                Some(word) => tail(word),
                None => Bits::new(),
            };
            let header = header(&mut self.exss, bytes);
            let start = out.len();
            let core = self.core.frame_size();
            out.extend(self.cores.drain(..core));
            out.extend_from_slice(&header);
            out.extend(self.lossless_stream.drain(..carry.bytes));
            let sent = out.len() - start;
            debug_assert!(
                sent <= self.rate.budget,
                "{sent} bytes over the {}-byte budget",
                self.rate.budget
            );
            self.rate.largest = self.rate.largest.max(sent);
        }
        self.rate.held = self.smoother.peak();
        self.rate.ahead = self.smoother.ahead();
        Ok(())
    }

    /// How the stream has used its ceiling so far.
    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// Bytes of the lossless frames encoded so far, headers included.
    pub fn lossless_bytes(&self) -> u64 {
        self.lossless_bytes
    }

    /// Channels of the frames sent so far that were coded against the core,
    /// summed over the frames.
    pub fn against_core(&self) -> u64 {
        self.against_core
    }

    /// For a 7.1.4, the furthest the navigation word has had to point and
    /// the largest block it has had to describe, in bytes: how close the
    /// stream has come to [`crate::navigation::MAX_OFFSET`] and
    /// [`crate::navigation::MAX_SIZE`].
    pub fn extension_reach(&self) -> Option<(usize, usize)> {
        self.reach
    }

    /// Bytes of the core in every frame.
    pub fn core_frame_size(&self) -> usize {
        self.core.frame_size()
    }
}

/// The lossless asset descriptor's tail when the asset carries the DTS:X
/// extension: two zero bits and the navigation word, as every frame of the
/// standard-profile reference streams has it. What the two bits are is not
/// established. [`ExssHeader::fit`] pads it to the descriptor's byte.
fn tail(word: u64) -> Bits {
    let mut w = hz_core::bits::BitWriter::new();
    w.put(2, 0);
    w.put(32, (word >> 32) as u32);
    w.put(32, word as u32);
    let bytes = w.padded();
    Bits::read(&mut hz_core::bits_read::BitReader::new(&bytes), 66).expect("66 bits were written")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{split, xll};

    /// Tones, so that the core is close and the difference from it cheap.
    fn tones(channels: usize, frames: usize) -> Vec<i32> {
        (0..frames * FRAME_SAMPLES * channels)
            .map(|i| {
                let (n, ch) = (i / channels, i % channels);
                let phase = std::f64::consts::TAU * 110.0 * (ch + 1) as f64 * n as f64 / 48000.0;
                ((phase.sin() * 0.4 * f64::from(1 << 23)) as i32) << 8
            })
            .collect()
    }

    /// The residual masks of every lossless frame's first channel set.
    fn residual_masks(stream: &[u8]) -> Vec<u16> {
        split::frames(stream)
            .map(|frame| {
                let exss = frame.unwrap().exss.unwrap();
                let header = ExssHeader::read(exss).unwrap();
                let at = header.header_size as usize;
                let xll = &exss[at..at + header.asset_sizes[0] as usize];
                xll::Headers::read(xll, true).unwrap().channel_sets[0].residual_encode
            })
            .collect()
    }

    #[test]
    fn a_frame_goes_out_per_frame_in_the_first_coded_alone_in_a_7_1() {
        for layout in [Layout::FivePointOne, Layout::SevenPointOne] {
            let channels = layout.channels();
            let mut encoder = Encoder::new(Config::reference(layout, 24)).unwrap();
            let mut stream = Vec::new();
            for (k, frame) in tones(channels, 5)
                .chunks(FRAME_SAMPLES * channels)
                .enumerate()
            {
                encoder.encode(frame, &mut stream).unwrap();
                assert_eq!(
                    stream.is_empty(),
                    k == 0,
                    "{layout:?}: the core runs a frame ahead"
                );
            }
            encoder.finish(&mut stream).unwrap();
            let masks = residual_masks(&stream);
            assert_eq!(masks.len(), 5, "{layout:?}");
            // FFmpeg would recover from a 7.1's first frame coded against the
            // core; a 5.1's it decodes as it is.
            assert_eq!(
                masks[0] == 0x3f,
                layout == Layout::SevenPointOne,
                "{layout:?}"
            );
            assert!(
                masks[1..].iter().all(|&m| m != 0x3f),
                "{layout:?}: {masks:x?}"
            );
            // Nothing is left to send.
            let sent = stream.len();
            encoder.finish(&mut stream).unwrap();
            assert_eq!(stream.len(), sent);
        }
    }

    #[test]
    fn full_coding_codes_no_channel_against_the_core() {
        let mut config = Config::reference(Layout::SevenPointOne, 24);
        config.lossless.residual = false;
        let mut encoder = Encoder::new(config).unwrap();
        let mut stream = Vec::new();
        for frame in tones(8, 3).chunks(FRAME_SAMPLES * 8) {
            encoder.encode(frame, &mut stream).unwrap();
        }
        encoder.finish(&mut stream).unwrap();
        assert_eq!(residual_masks(&stream), [0x3f; 3]);
        assert_eq!(encoder.against_core(), 0);
    }

    #[test]
    fn a_refused_frame_moves_nothing_on() {
        let mut encoder = Encoder::new(Config::reference(Layout::FivePointOne, 16)).unwrap();
        let mut stream = Vec::new();
        let mut pcm: Vec<i32> = tones(6, 1).iter().map(|s| s & !0xffff).collect();
        encoder.encode(&pcm, &mut stream).unwrap();
        pcm[7] |= 0x100;
        assert!(encoder.encode(&pcm, &mut stream).is_err());
        assert!(stream.is_empty());
        pcm[7] &= !0xffff;
        encoder.encode(&pcm, &mut stream).unwrap();
        encoder.finish(&mut stream).unwrap();
        assert_eq!(residual_masks(&stream).len(), 2);
    }

    /// A 7.1.4 is the standard DTS:X profile: the asset states the 7.1, its
    /// descriptor's tail is two zero bits, the navigation word and the
    /// padding to the byte, and the word points at the block that ends the
    /// lossless frame. Every substream header ends on a 32-bit word, as the
    /// reference streams' do.
    #[test]
    fn a_7_1_4_carries_the_word_that_locates_its_heights() {
        for layout in [
            Layout::FivePointOne,
            Layout::SevenPointOne,
            Layout::SevenPointOneFour,
        ] {
            let channels = layout.channels();
            let mut encoder = Encoder::new(Config::reference(layout, 24)).unwrap();
            let mut stream = Vec::new();
            let pcm: Vec<i32> = tones(channels, 4).iter().map(|&s| s >> 1 & !0xff).collect();
            for frame in pcm.chunks(FRAME_SAMPLES * channels) {
                encoder.encode(frame, &mut stream).unwrap();
            }
            encoder.finish(&mut stream).unwrap();
            let mut frames = 0;
            for frame in split::frames(&stream) {
                let exss = frame.unwrap().exss.unwrap();
                let header = ExssHeader::read(exss).unwrap();
                assert_eq!(header.header_size % 4, 0, "{layout:?}");
                let asset = &header.assets[0];
                let statics = asset.statics.as_ref().unwrap();
                assert_eq!(statics.channels, 8.min(channels as u16));
                let tail = &asset.tail;
                if layout != Layout::SevenPointOneFour {
                    // The padding to the descriptor's byte, and nothing else.
                    assert!(tail.len() < 8 && tail.is_zero(), "{layout:?}");
                    continue;
                }
                assert_eq!((tail.len(), tail.bytes()[0] >> 6), (69, 0));
                let nav = crate::navigation::Navigation::read(tail.word64(2).unwrap()).unwrap();
                let at = header.header_size as usize;
                let xll = &exss[at..at + header.asset_sizes[0] as usize];
                assert_eq!(nav.offset + nav.size, xll.len());
                assert_eq!(
                    u32::from_be_bytes(xll[nav.offset..nav.offset + 4].try_into().unwrap()),
                    crate::dtsx::SYNC
                );
                frames += 1;
            }
            if layout == Layout::SevenPointOneFour {
                assert_eq!(frames, 4);
                let (offset, size) = encoder.extension_reach().unwrap();
                assert!(offset <= crate::navigation::MAX_OFFSET && size > 22);
            }
        }
    }

    /// Tones with a burst of full-scale white noise in every channel from
    /// frame 10 to 13, all `shift` bits down, 24 bits.
    fn bursts(channels: usize, frames: usize, shift: u32) -> Vec<i32> {
        let mut state = 0x2545_f491u32;
        let tones = tones(channels, frames);
        tones
            .iter()
            .enumerate()
            .map(|(i, &tone)| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let frame = i / channels / FRAME_SAMPLES;
                if (10..14).contains(&frame) {
                    (state as i32) >> shift >> 8 << 8
                } else {
                    tone >> shift >> 8 << 8
                }
            })
            .collect()
    }

    fn encode(config: Config, pcm: &[i32]) -> (Vec<u8>, Result<(), Refused>, Rate) {
        let channels = config.lossless.layout.channels();
        let mut encoder = Encoder::new(config).unwrap();
        let mut stream = Vec::new();
        for frame in pcm.chunks(FRAME_SAMPLES * channels) {
            if let Err(e) = encoder.encode(frame, &mut stream) {
                return (stream, Err(e), encoder.rate());
            }
        }
        let result = encoder.finish(&mut stream);
        (stream, result, encoder.rate())
    }

    /// Each frame's bytes, and its substream's lossless data, sync fields
    /// and descriptor tail.
    fn substreams(stream: &[u8]) -> Vec<(usize, Vec<u8>, Option<XllSync>, Bits)> {
        split::frames(stream)
            .map(|frame| {
                let frame = frame.unwrap();
                let exss = frame.exss.unwrap();
                let header = ExssHeader::read(exss).unwrap();
                let at = header.header_size as usize;
                let data = exss[at..at + header.asset_sizes[0] as usize].to_vec();
                let asset = &header.assets[0];
                let sync = asset.coding.xll().unwrap().sync.clone();
                let bytes = frame.core.unwrap().len() + exss.len();
                (bytes, data, sync, asset.tail.clone())
            })
            .collect()
    }

    /// Under a ceiling the noise does not fit, its frames are sent partly
    /// ahead, and the decoder's buffer gets back exactly the lossless frames
    /// a stream that never smooths carries, each with its own substream —
    /// and, for a 7.1.4, the navigation word of each substream points at
    /// the block of the frame decoded with it. No frame passes the budget,
    /// the stream states the buffer it uses and holds no more, and a
    /// decoder that starts at any substream with a sync takes out the frame
    /// its delay names.
    #[test]
    fn a_lowered_ceiling_sends_frames_ahead_and_every_frame_comes_back_whole() {
        for (layout, shift, ceiling) in [
            (Layout::SevenPointOne, 0, 8_500_000),
            (Layout::SevenPointOneFour, 10, 8_000_000),
        ] {
            let pcm = bursts(layout.channels(), 40, shift);
            let (plain, result, rate) = encode(Config::reference(layout, 24), &pcm);
            result.unwrap();
            assert_eq!((rate.buffer, rate.ahead), (0, 0), "{layout:?}");
            let frames: Vec<Vec<u8>> = substreams(&plain).into_iter().map(|s| s.1).collect();
            let largest = frames.iter().map(Vec::len).max().unwrap();

            let mut config = Config::reference(layout, 24);
            config.peak.ceiling = ceiling;
            config.peak.lookahead = 16;
            let (smoothed, result, rate) = encode(config, &pcm);
            result.unwrap_or_else(|e| panic!("{layout:?}: {e}"));
            let budget = peak::frame_budget(ceiling, 512, 48000);
            assert!(
                largest + 2012 > budget,
                "{layout:?}: the noise has to need it"
            );
            assert!(
                rate.ahead > 0 && rate.largest <= budget,
                "{layout:?}: {rate:?}"
            );
            assert_eq!(rate.buffer, 128 << 10);

            let substreams = substreams(&smoothed);
            assert_eq!(substreams.len(), frames.len());
            let mut buffer = peak::DecoderBuffer::new();
            for (n, (bytes, data, sync, tail)) in substreams.iter().enumerate() {
                assert!(*bytes <= budget, "{layout:?}: frame {n} is {bytes} bytes");
                if let Some(s) = sync {
                    assert_eq!(s.peak_buffer_code, 8);
                    assert_eq!(s.delay_bits, peak::delay_bits(s.delay_frames));
                }
                assert_eq!(
                    buffer.push(data, sync.as_ref()),
                    Ok(peak::Taken::Frame(frames[n].len())),
                    "{layout:?}: substream {n}"
                );
                assert_eq!(buffer.frame(), &frames[n][..], "{layout:?}: substream {n}");
                if layout == Layout::SevenPointOneFour {
                    let nav = crate::navigation::Navigation::read(tail.word64(2).unwrap()).unwrap();
                    assert_eq!(nav.offset + nav.size, frames[n].len());
                    assert_eq!(
                        &frames[n][nav.offset..nav.offset + 4],
                        &crate::dtsx::SYNC.to_be_bytes()
                    );
                }
            }
            assert!(buffer.peak() <= rate.buffer && buffer.peak() as u64 == rate.held);
            for (n, (_, data, sync, _)) in substreams.iter().enumerate() {
                let Some(s) = sync else { continue };
                let due = n + s.delay_frames as usize;
                let mut joining = peak::DecoderBuffer::new();
                let mut taken = joining.push(data, Some(s));
                for (_, data, sync, _) in &substreams[n + 1..=due] {
                    taken = joining.push(data, sync.as_ref());
                }
                assert_eq!(taken, Ok(peak::Taken::Frame(frames[due].len())));
                assert_eq!(
                    joining.frame(),
                    &frames[due][..],
                    "{layout:?}: joined at {n}"
                );
            }
        }
    }

    /// A frame that cannot reach the decoder in time is refused, named, and
    /// the stream ends there: what was sent stays, nothing more is.
    #[test]
    fn a_frame_past_the_ceiling_ends_the_stream() {
        let pcm = bursts(8, 30, 0);
        let mut config = Config::reference(Layout::SevenPointOne, 24);
        config.peak.ceiling = 7_000_000;
        config.peak.buffer_code = 1;
        config.peak.lookahead = 4;
        let mut encoder = Encoder::new(config).unwrap();
        let mut stream = Vec::new();
        let mut refused = None;
        for (k, frame) in pcm.chunks(FRAME_SAMPLES * 8).enumerate() {
            if let Err(e) = encoder.encode(frame, &mut stream) {
                refused = Some((k, e));
                break;
            }
        }
        let (k, e) = refused.expect("the noise does not fit");
        let Refused::PeakRate { frame, .. } = e else {
            panic!("{e}")
        };
        assert!(
            (10..14).contains(&frame) && k as u64 <= frame + 6,
            "{k}, {e}"
        );
        let sent = stream.len();
        assert!(matches!(
            encoder.encode(&pcm[..FRAME_SAMPLES * 8], &mut stream),
            Err(Refused::PeakRate { .. })
        ));
        assert!(encoder.finish(&mut stream).is_err());
        assert_eq!(stream.len(), sent);
        assert!((substreams(&stream).len() as u64) < frame);
    }

    /// At the default ceiling no frame the lossless layer can write is too
    /// large for its substream: the stream states no buffer, and every frame
    /// goes out as soon as it is encoded, as before.
    #[test]
    fn the_default_ceiling_needs_no_buffer() {
        for layout in [
            Layout::FivePointOne,
            Layout::SevenPointOne,
            Layout::SevenPointOneFour,
        ] {
            let encoder = Encoder::new(Config::reference(layout, 24)).unwrap();
            assert_eq!(encoder.rate().buffer, 0, "{layout:?}");
            assert!(encoder.lossless.frame_bound() + 2012 + 48 < encoder.rate().budget);
        }
        // No room beside the core — none at all, or not even for the
        // headers — is refused, not panicked on.
        for ceiling in [0, 1, 749, 750, 1_500_000, 1_520_000] {
            let mut config = Config::reference(Layout::SevenPointOneFour, 24);
            config.peak.ceiling = ceiling;
            assert!(Encoder::new(config).is_err(), "{ceiling} b/s");
        }
        // A ceiling past what a substream's size field describes caps the
        // room at what it does.
        for ceiling in [800_000_000, u32::MAX] {
            let mut config = Config::reference(Layout::SevenPointOne, 24);
            config.peak.ceiling = ceiling;
            let encoder = Encoder::new(config).expect("a ceiling past any frame");
            assert_eq!(encoder.rate().buffer, 0, "{ceiling} b/s");
        }
        let mut config = Config::reference(Layout::SevenPointOne, 24);
        config.peak.lookahead = usize::MAX;
        assert!(Encoder::new(config).is_err(), "an endless lookahead");
    }

    /// A corner its height takes past 24 bits is refused, naming the input
    /// frame it is in — not the frame the lossless layer last coded, which
    /// is the one before, still waiting for its core.
    #[test]
    fn a_fold_past_24_bits_names_its_input_frame() {
        let mut encoder = Encoder::new(Config::reference(Layout::SevenPointOneFour, 24)).unwrap();
        let mut stream = Vec::new();
        let mut pcm = vec![0i32; FRAME_SAMPLES * 12];
        for _ in 0..2 {
            encoder.encode(&pcm, &mut stream).unwrap();
        }
        // L at full scale and TFL at full scale: 1.707.
        pcm[7 * 12] = ((1 << 23) - 1) << 8;
        pcm[7 * 12 + 8] = ((1 << 23) - 1) << 8;
        match encoder.encode(&pcm, &mut stream) {
            Err(Refused::Fold {
                frame: 2,
                channel: 0,
                sample: 7,
                ..
            }) => {}
            other => panic!("{other:?}"),
        }
    }
}
