//! Encode PCM to a DTS core with `hz-dca`, then read the stream back.
//!
//! ```text
//! cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 6 --out out.dts
//! ```
//!
//! Phase C1's round trip. The input is raw interleaved 32-bit PCM in WAV
//! order, what `ffmpeg -f s32le` writes; the last frame is padded with
//! silence, as FFmpeg pads it. The stream is then put through everything
//! `cargo xtask dts` does — each header read and rewritten — and, with the
//! `dca-oracle` feature, decoded frame by frame and compared with the input:
//! the lag the decoder's output has and each channel's signal-to-error ratio.
//!
//! `--ffmpeg-lfe` reproduces FFmpeg's LFE history defect, which is what makes
//! the stream byte-identical to `ffmpeg -c:a dca -b:a <rate>` on the same
//! samples (see `hz_dca::encoder::Config::lfe_history_quirk`).

use crate::dts::{self, Reference};
use hz_core::{Error, Result};
use hz_dca::encoder::{Config, Encoder, FRAME_SAMPLES, Layout};
use std::path::PathBuf;

pub struct Options {
    pub input: PathBuf,
    pub channels: usize,
    pub sample_rate: u32,
    pub bit_rate: u32,
    pub adpcm: bool,
    pub ffmpeg_lfe: bool,
    pub out: PathBuf,
}

pub fn run(options: Options) -> Result<()> {
    let layout = match options.channels {
        1 => Layout::Mono,
        2 => Layout::Stereo,
        4 => Layout::TwoTwo,
        5 => Layout::FivePointZero,
        6 => Layout::FivePointOne,
        n => {
            return Err(Error::refused(
                &options.input,
                format!("{n} channels: the core encoder takes 1, 2, 4, 5 or 6"),
            ));
        }
    };
    let reference = Reference::read(&options.input, options.channels)?;
    let mut encoder = Encoder::new(Config {
        layout,
        sample_rate: options.sample_rate,
        bit_rate: options.bit_rate,
        adpcm: options.adpcm,
        pcm_resolution_code: 0,
        lfe_history_quirk: options.ffmpeg_lfe,
    })
    .map_err(|e| Error::refused(&options.input, e.0))?;

    let per_frame = FRAME_SAMPLES * options.channels;
    let mut frame = vec![0i32; per_frame];
    let mut stream = Vec::new();
    for chunk in reference.pcm.chunks(per_frame) {
        frame[..chunk.len()].copy_from_slice(chunk);
        frame[chunk.len()..].fill(0);
        encoder.encode(&frame, &mut stream);
    }
    std::fs::write(&options.out, &stream).map_err(|e| Error::io(&options.out, e))?;
    println!(
        "encoded {} frames of {} bytes, rate code {}",
        stream.len() / encoder.frame_size(),
        encoder.frame_size(),
        encoder.header().bit_rate_code
    );
    dts::run(dts::Options {
        input: options.out,
        frames: None,
        reference: Some(reference),
    })
}
