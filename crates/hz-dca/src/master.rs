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
//! asset, the core backward-compatible in the core substream), with the
//! smoothing buffer stated as none: every lossless frame sits whole in its
//! own substream, so the substreams vary in size frame to frame. Smoothing
//! that peak is phase C4.
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

pub use crate::encoder::Unsupported;
pub use crate::lossless::Refused;

/// What to encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub lossless: lossless::Config,
    /// The core: 5.1 at 48 kHz, whatever else it is.
    pub core: encoder::Config,
}

impl Config {
    /// The reference streams' shape: the lossless layer as
    /// [`lossless::Config::reference`] lays it out, and a 1509 kb/s core
    /// stating the source's word length as they do — 6 for 24 bits, 0 for
    /// sixteen.
    pub fn reference(layout: Layout, bits: u8) -> Self {
        Self {
            lossless: lossless::Config::reference(layout, bits),
            core: encoder::Config {
                pcm_resolution_code: if bits > 16 { 6 } else { 0 },
                ..encoder::Config::reference_five_one()
            },
        }
    }
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
    /// Frames sent.
    frames: u64,
    /// Bytes of the lossless frames sent, and how many of their channels
    /// were coded against the core.
    lossless_bytes: u64,
    against_core: u64,
    /// The furthest offset and the largest DTS:X block navigated so far.
    reach: Option<(usize, usize)>,
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
        Ok(Self {
            pending: vec![0; FRAME_SAMPLES * layout.channels()],
            waiting: false,
            core_frame: Vec::with_capacity(core.frame_size()),
            frames: 0,
            lossless_bytes: 0,
            against_core: 0,
            reach: None,
            decoder: CoreDecoder::new(),
            core,
            lossless,
            exss,
        })
    }

    /// Take one frame — [`FRAME_SAMPLES`] samples of every channel,
    /// interleaved in WAV order as 32-bit samples whose bits below the
    /// configured resolution are zero — and append the stream's frame for
    /// the frame before it to `out`: nothing on the first call, since the
    /// core runs a frame ahead. [`Encoder::finish`] sends the last.
    ///
    /// # Panics
    /// If `pcm` is not exactly one frame.
    pub fn encode(&mut self, pcm: &[i32], out: &mut Vec<u8>) -> Result<(), Refused> {
        assert_eq!(
            pcm.len(),
            self.pending.len(),
            "a frame is {FRAME_SAMPLES} samples of each channel"
        );
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

    /// Send the frame still waiting, with a core made from silence. Nothing
    /// to send without one.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
        if !self.waiting {
            return Ok(());
        }
        let silence = vec![0; self.pending.len()];
        self.core_frame.clear();
        self.core
            .encode(self.lossless.core_input(&silence), &mut self.core_frame);
        self.send(out)?;
        self.waiting = false;
        Ok(())
    }

    /// The stream's frame for the pending input: the core frame last
    /// encoded, the substream header, the lossless frame.
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
        let size = self.lossless.encode(&self.pending, core)?.len() as u32;
        self.exss.asset_sizes[0] = size;
        if let Coding::Components { xll: Some(x), .. } = &mut self.exss.assets[0].coding {
            x.size = size;
        }
        if let Some(navigation) = self.lossless.extension() {
            let (offset, size) = self.reach.unwrap_or_default();
            self.reach = Some((offset.max(navigation.offset), size.max(navigation.size)));
            self.exss.assets[0].tail = tail(
                navigation
                    .word()
                    .expect("the lossless encoder refuses a block it cannot navigate"),
            );
        }
        // Narrow fields where the whole substream fits them, wide otherwise.
        self.exss.wide = false;
        let header = loop {
            self.exss.reserved = Bits::new();
            self.exss
                .fit()
                .expect("a lossless frame fits a substream's wide fields");
            // Every reference stream's header ends on a 32-bit word — 28 to
            // 44 bytes, never between — so that what follows starts on one:
            // zeroes in the reserved bits to get there.
            let short = (4 - self.exss.header_size % 4) % 4;
            if short > 0 {
                self.exss.reserved.pad(8 * short as usize);
                self.exss
                    .fit()
                    .expect("a lossless frame fits a substream's wide fields");
            }
            self.exss.size = self.exss.header_size + size;
            match self.exss.write() {
                Ok(header) => break header,
                Err(_) if !self.exss.wide => self.exss.wide = true,
                Err(e) => panic!("a fitted substream header does not write: {}", e.0),
            }
        };
        out.extend_from_slice(&self.core_frame);
        out.extend_from_slice(&header);
        out.extend_from_slice(self.lossless.frame());
        self.frames += 1;
        self.lossless_bytes += u64::from(size);
        self.against_core += u64::from(self.lossless.against_core());
        Ok(())
    }

    /// Bytes of the lossless frames sent so far, headers included.
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
