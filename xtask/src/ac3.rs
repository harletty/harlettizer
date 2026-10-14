//! Decode an AC-3 or E-AC-3 stream through the workspace's own decoder and
//! hold it to the PCM it was made from.
//!
//! ```text
//! cargo run -p xtask --features eac3-oracle -- ac3 stream.ac3 [--reference in.f32] [--pcm out.f32]
//! ```
//!
//! FFmpeg decoding a stream is one decoder agreeing with it; the `eac3` crate
//! is a second, written independently, and its AC-3 path is the one that
//! decodes the legacy core of a dual-core E-AC-3 stream — what a 7.1 phase of
//! the lossy track will lean on. Every syncframe goes through it; a frame it
//! refuses is a failure. With `--reference` — raw interleaved 32-bit float in
//! WAV order, what `hz-ac3`'s `raw_to_ac3` example reads — the decoded PCM is
//! compared with the source after the encoder's delay, channel by channel.
//! With `--pcm` it is written out, interleaved, in WAV order.

use eac3::{BedChannel, Extractor, PcmDecoder};
use hz_core::{Error, Result};
use std::path::PathBuf;

pub struct Options {
    pub input: PathBuf,
    pub reference: Option<PathBuf>,
    pub pcm: Option<PathBuf>,
}

/// Samples a decoder's output lags an `hz-ac3` input by.
const DELAY: usize = 256;

/// A channel's place in WAV order among the channels AC-3 can carry.
fn wav_rank(channel: BedChannel) -> usize {
    match channel {
        BedChannel::FrontLeft => 0,
        BedChannel::FrontRight => 1,
        BedChannel::Center => 2,
        BedChannel::LowFrequencyEffects => 3,
        BedChannel::RearCenter => 4,
        BedChannel::SurroundLeft => 5,
        BedChannel::SurroundRight => 6,
        _ => 7,
    }
}

pub fn run(options: Options) -> Result<()> {
    let bytes = std::fs::read(&options.input).map_err(|e| Error::io(&options.input, e))?;
    let mut extractor = Extractor::default();
    extractor.push_bytes(&bytes);
    let mut decoder = PcmDecoder::new();

    let mut channels = 0usize;
    let mut sample_rate = 0u32;
    let mut decoded: Vec<f32> = Vec::new();
    let mut frames = 0usize;
    loop {
        let frame = match extractor.next_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(e) => {
                return Err(Error::malformed(
                    &options.input,
                    format!("syncframe {frames}: {e:?}"),
                ));
            }
        };
        // AC-3 through the legacy path, E-AC-3 through the main one.
        let pushed = if frame.info().bitstream_id <= 10 {
            decoder.push_legacy_ac3_access_unit(frame.as_bytes())
        } else {
            decoder.push_access_unit(frame.as_bytes())
        };
        let pushed = pushed
            .map_err(|e| Error::malformed(&options.input, format!("syncframe {frames}: {e}")))?;
        let pcm = pushed.pcm;
        let mut planes: Vec<(usize, &[f32])> = pcm
            .fullband_channel_order
            .iter()
            .zip(&pcm.fullband_channels)
            .map(|(channel, samples)| (wav_rank(*channel), samples.as_slice()))
            .collect();
        if let Some(lfe) = &pcm.lfe_channel {
            planes.push((wav_rank(BedChannel::LowFrequencyEffects), lfe.as_slice()));
        }
        planes.sort_by_key(|(rank, _)| *rank);
        if frames == 0 {
            channels = planes.len();
            sample_rate = pcm.sample_rate;
        } else if planes.len() != channels {
            return Err(Error::malformed(
                &options.input,
                format!(
                    "syncframe {frames} has {} channels, not {channels}",
                    planes.len()
                ),
            ));
        }
        let samples = pcm.samples_per_channel();
        for n in 0..samples {
            for (_, plane) in &planes {
                decoded.push(plane[n]);
            }
        }
        frames += 1;
    }
    // The extractor says "nothing more" both at the end and when it is
    // waiting for the rest of a frame; the whole file is in, so anything it
    // still holds is a frame cut short.
    if extractor.buffered_len() > 0 {
        return Err(Error::malformed(
            &options.input,
            format!(
                "{} bytes after syncframe {frames} are not a whole syncframe",
                extractor.buffered_len()
            ),
        ));
    }
    if frames == 0 {
        return Err(Error::malformed(&options.input, "no AC-3 syncframe in it"));
    }
    println!(
        "{}: {frames} syncframes decoded by the eac3 crate, {channels} channels at {sample_rate} Hz",
        options.input.display()
    );

    if let Some(path) = &options.pcm {
        let raw: Vec<u8> = decoded.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(path, raw).map_err(|e| Error::io(path, e))?;
        println!("  wrote        {}", path.display());
    }

    if let Some(path) = &options.reference {
        let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        let reference: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let frames_ref = reference.len() / channels;
        let frames_dec = decoded.len() / channels;
        // The stream has to carry the whole reference out past the delay; a
        // shorter one lost its end, which a comparison over the common part
        // would not see.
        if frames_dec < frames_ref + DELAY {
            return Err(Error::malformed(
                &options.input,
                format!(
                    "decodes to {frames_dec} samples, short of the {} the reference and the \
                     {DELAY}-sample delay need",
                    frames_ref + DELAY
                ),
            ));
        }
        let usable = frames_ref;
        let (mut signal, mut noise) = (vec![0f64; channels], vec![0f64; channels]);
        for n in 0..usable {
            for ch in 0..channels {
                let r = f64::from(reference[n * channels + ch]);
                let d = f64::from(decoded[(n + DELAY) * channels + ch]);
                signal[ch] += r * r;
                noise[ch] += (d - r) * (d - r);
            }
        }
        let snr = |s: f64, n: f64| 10.0 * (s / n.max(1e-30)).log10();
        let total = snr(signal.iter().sum(), noise.iter().sum());
        println!(
            "  against      {}: {total:.2} dB over {usable} samples, by channel {}",
            path.display(),
            signal
                .iter()
                .zip(&noise)
                .map(|(s, n)| format!("{:.1}", snr(*s, *n)))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    Ok(())
}
