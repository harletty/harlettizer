//! DTS-HD Master Audio: a core and a lossless layer in one stream.
//!
//! Each frame is a core frame ([`crate::encoder`]) followed by an extension
//! substream ([`crate::exss`]) carrying one audio asset whose only component
//! is the lossless layer ([`crate::lossless`]). The core is the 5.1 a
//! decoder of the lossless layer's first channel set alone presents — the
//! input itself for a 5.1, the fold for a 7.1 — so a core-only decoder and a
//! 5.1 lossless decoder play the same mix, and every lossless channel is
//! coded as itself, independent of the core.
//!
//! The substream header is the reference streams' (one presentation, one
//! asset, the core backward-compatible in the core substream), with the
//! smoothing buffer stated as none: every lossless frame sits whole in its
//! own substream, so the substreams vary in size frame to frame. Smoothing
//! that peak is phase C4.

use crate::bits::Bits;
use crate::encoder;
#[cfg(doc)]
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
                    channels: layout.channels() as u16,
                    mapping: ChannelMapping::OneToOne {
                        embedded_stereo: false,
                        embedded_6ch: layout == Layout::SevenPointOne,
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
            core,
            lossless,
            exss,
        })
    }

    /// Encode one frame — [`FRAME_SAMPLES`] samples of every channel,
    /// interleaved in WAV order as 32-bit samples whose bits below the
    /// configured resolution are zero — and append it to `out`.
    ///
    /// # Panics
    /// If `pcm` is not exactly one frame.
    pub fn encode(&mut self, pcm: &[i32], out: &mut Vec<u8>) -> Result<(), Refused> {
        let size = self.lossless.encode(pcm)?.len() as u32;
        self.exss.asset_sizes[0] = size;
        if let Coding::Components { xll: Some(x), .. } = &mut self.exss.assets[0].coding {
            x.size = size;
        }
        // Narrow fields where the whole substream fits them, wide otherwise.
        self.exss.wide = false;
        let header = loop {
            self.exss
                .fit()
                .expect("a lossless frame fits a substream's wide fields");
            self.exss.size = self.exss.header_size + size;
            match self.exss.write() {
                Ok(header) => break header,
                Err(_) if !self.exss.wide => self.exss.wide = true,
                Err(e) => panic!("a fitted substream header does not write: {}", e.0),
            }
        };
        self.core.encode(self.lossless.core_pcm(), out);
        out.extend_from_slice(&header);
        out.extend_from_slice(self.lossless.frame());
        Ok(())
    }

    /// Bytes of the core in every frame.
    pub fn core_frame_size(&self) -> usize {
        self.core.frame_size()
    }
}
