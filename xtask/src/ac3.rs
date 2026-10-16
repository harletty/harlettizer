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

#[cfg(test)]
mod tests {
    use super::*;
    use hz_ac3::{ChannelMode, Config, Encoder, FRAME_SAMPLES, Format, Metadata};

    /// Every syncframe of `stream` decoded by the `eac3` crate, channel by
    /// channel in WAV order.
    fn decode(stream: &[u8]) -> Vec<Vec<f32>> {
        let mut extractor = Extractor::default();
        extractor.push_bytes(stream);
        let mut decoder = PcmDecoder::new();
        let mut channels: Vec<Vec<f32>> = Vec::new();
        while let Some(frame) = extractor.next_frame().unwrap() {
            let pushed = if frame.info().bitstream_id <= 10 {
                decoder.push_legacy_ac3_access_unit(frame.as_bytes())
            } else {
                decoder.push_access_unit(frame.as_bytes())
            }
            .unwrap();
            let pcm = pushed.pcm;
            let mut planes: Vec<(usize, &Vec<f32>)> = pcm
                .fullband_channel_order
                .iter()
                .zip(&pcm.fullband_channels)
                .map(|(channel, samples)| (wav_rank(*channel), samples))
                .collect();
            if let Some(lfe) = &pcm.lfe_channel {
                planes.push((wav_rank(BedChannel::LowFrequencyEffects), lfe));
            }
            planes.sort_by_key(|(rank, _)| *rank);
            channels.resize(planes.len(), Vec::new());
            for (out, (_, plane)) in channels.iter_mut().zip(planes) {
                out.extend_from_slice(plane);
            }
        }
        channels
    }

    /// What the encoder says a decoder will play is what the workspace's
    /// decoder plays, frame by frame and channel by channel — with coupling,
    /// with the LFE, with the stereo pair rematrixed, in both formats.
    #[test]
    fn the_reconstruction_is_what_the_decoder_plays() {
        let cases = [
            (Format::Eac3, ChannelMode::ThreeTwo, true, 384),
            (Format::Ac3, ChannelMode::ThreeTwo, true, 448),
            (Format::Ac3, ChannelMode::Stereo, false, 192),
        ];
        for (format, mode, lfe, kbps) in cases {
            let channels = mode.full_bandwidth_channels() + usize::from(lfe);
            let mut encoder = Encoder::new(Config {
                format,
                sample_rate: 48_000,
                mode,
                lfe,
                bit_rate: Some(kbps),
                coupling: true,
                metadata: Metadata::default(),
            })
            .unwrap();
            encoder.reconstruct();
            const FRAMES: usize = 16;
            let mut seed = 5u32;
            let mut noise = || {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            };
            // Each channel its own tone over noise, the first two sharing
            // most of theirs so that the pair rematrixes.
            let input: Vec<Vec<f32>> = {
                let shared: Vec<f32> = (0..FRAMES * FRAME_SAMPLES).map(|_| noise()).collect();
                (0..channels)
                    .map(|ch| {
                        (0..FRAMES * FRAME_SAMPLES)
                            .map(|n| {
                                let tone = (n as f32 * 0.013 * (ch + 1) as f32).sin();
                                let own = noise();
                                0.3 * tone + 0.2 * own + if ch < 2 { 0.3 * shared[n] } else { 0.0 }
                            })
                            .collect()
                    })
                    .collect()
            };
            let (mut stream, mut reconstructed) = (Vec::new(), vec![Vec::new(); channels]);
            for k in 0..FRAMES {
                let frame: Vec<&[f32]> = input
                    .iter()
                    .map(|ch| &ch[k * FRAME_SAMPLES..(k + 1) * FRAME_SAMPLES])
                    .collect();
                encoder.encode(&frame, &mut stream).unwrap();
                for (ch, out) in reconstructed.iter_mut().enumerate() {
                    out.extend_from_slice(encoder.decoded(ch).unwrap());
                }
            }
            let coupled = encoder.coupled();
            let decoded = decode(&stream);
            assert_eq!(decoded.len(), channels);
            let (mut signal, mut error, mut worst) = (0f64, 0f64, 0f32);
            for (ours, theirs) in reconstructed.iter().zip(&decoded) {
                assert_eq!(ours.len(), theirs.len());
                for (a, b) in ours.iter().zip(theirs) {
                    signal += f64::from(*b).powi(2);
                    error += f64::from(a - b).powi(2);
                    worst = worst.max((a - b).abs());
                }
            }
            let agreement = 10.0 * (signal / error.max(1e-30)).log10();
            eprintln!("{format:?} {mode:?} coupled {coupled}: {agreement:.1} dB, worst {worst:e}");
            assert!(
                agreement > 100.0,
                "{format:?} {mode:?}: the reconstruction agrees with the decoder to {agreement:.1} dB"
            );
        }
    }
}
