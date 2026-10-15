//! Encode PCM to DTS with `hz-dca`, then read the stream back.
//!
//! ```text
//! cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 6 --out out.dts
//! cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 8 --lossless 24 --out out.dts
//! cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 12 --lossless 24 --out out.dts
//! ```
//!
//! Phase C1's round trip, with `--lossless` phase C2's, and with twelve
//! channels — a 7.1.4, the heights last — phase C3's. The input is raw
//! interleaved 32-bit PCM in WAV order, what `ffmpeg -f s32le` writes; the
//! last frame is padded with silence, as FFmpeg pads it. The stream is then
//! put through everything `cargo xtask dts` does — each header read and
//! rewritten — and, with the `dca-oracle` feature, decoded frame by frame:
//! a core-only stream is compared with the input by lag and per-channel
//! signal-to-error ratio, a Master Audio stream sample for sample, every
//! channel of every frame.
//!
//! A Master Audio stream also goes through FFmpeg where it is installed —
//! decoded with its checksums checked, and compared sample for sample in
//! the same way. FFmpeg reports nothing on a healthy stream, so any line it
//! prints fails the run; so does a profile other than DTS-HD MA, which would
//! mean it decoded the core instead.
//!
//! A 7.1.4 is the standard DTS:X profile. Through `dca`, the 7.1 it presents
//! has to be the composite — each height folded into its corner as the
//! encoder folds it — the four heights the input's, and the integer unfold
//! of the two ([`dts::unfold`]) the input's 7.1, all bit for bit. Through
//! FFmpeg, which decodes the bed, the 7.1 has to be the composite and the
//! profile DTS-HD MA + DTS:X.
//!
//! With `--lossless`, the stream is held under `--ceiling` bits a second,
//! the core and every header included (24.5 Mb/s by default), frames too
//! large for their own substream sent partly ahead into a decoder smoothing
//! buffer of `--buffer` steps of 16 kB (phase C4); `dts` then feeds every
//! substream to that buffer as both decoders keep it and fails the run on a
//! frame over the ceiling, a buffer starved or overfull. A frame that cannot
//! be carried even so is refused, named. `--dialnorm` states a dialogue
//! normalisation in the core header.
//!
//! `--ffmpeg-lfe` reproduces FFmpeg's LFE history defect, which is what makes
//! a core byte-identical to `ffmpeg -c:a dca -b:a <rate>` on the same samples
//! (see `hz_dca::encoder::Config::lfe_history_quirk`).

use crate::dts::{self, Reference};
use hz_core::{Error, Result};
use hz_dca::encoder::{Config, Encoder, FRAME_SAMPLES, Layout};
use hz_dca::{lossless, master};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Options {
    pub input: PathBuf,
    pub channels: usize,
    /// Bits of a lossless layer: a Master Audio stream rather than a core.
    pub lossless: Option<u8>,
    pub sample_rate: u32,
    pub bit_rate: u32,
    pub adpcm: bool,
    pub ffmpeg_lfe: bool,
    /// `log2` of the lossless frame's segments, when not the reference
    /// streams' 1.
    pub segments_log2: Option<u8>,
    /// The MSB/LSB split flag, when not the reference streams' (on at 24
    /// bits, off at 16).
    pub lsb_split: Option<bool>,
    /// Every lossless channel coded as itself, not against the core.
    pub full_coded: bool,
    /// Bits a second the whole stream is held under, with `--lossless`.
    pub ceiling: u32,
    /// The smoothing buffer code, 0 to 15 steps of 16 kB, with `--lossless`.
    pub buffer: u8,
    /// The core header's dialogue normalisation, in dB, 0 to 31.
    pub dialnorm: u8,
    pub out: PathBuf,
}

pub fn run(options: Options) -> Result<()> {
    if let Some(bits) = options.lossless {
        return run_lossless(&options, bits);
    }
    if options.segments_log2.is_some() || options.lsb_split.is_some() || options.full_coded {
        return Err(Error::refused(
            &options.input,
            "segments, the LSB split and full coding are the lossless layer's: they need \
             --lossless",
        ));
    }
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
        dialnorm: options.dialnorm,
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
        input: options.out.clone(),
        frames: None,
        reference: Some(reference),
        unfold: None,
        rate_log: None,
        ceiling: None,
    })
}

fn run_lossless(options: &Options, bits: u8) -> Result<()> {
    let layout = match options.channels {
        6 => lossless::Layout::FivePointOne,
        8 => lossless::Layout::SevenPointOne,
        12 => lossless::Layout::SevenPointOneFour,
        n => {
            return Err(Error::refused(
                &options.input,
                format!("{n} channels: the lossless encoder takes 6, 8 or 12"),
            ));
        }
    };
    if options.sample_rate != 48000 {
        return Err(Error::refused(
            &options.input,
            "the lossless encoder takes 48 kHz",
        ));
    }
    let reference = Reference::read(&options.input, options.channels)?;
    let mut config = master::Config::reference(layout, bits);
    config.core.bit_rate = options.bit_rate;
    config.core.adpcm = options.adpcm;
    config.core.lfe_history_quirk = options.ffmpeg_lfe;
    if let Some(segments_log2) = options.segments_log2 {
        config.lossless.segments_log2 = segments_log2;
    }
    if let Some(split) = options.lsb_split {
        config.lossless.scalable_lsbs = split;
    }
    config.lossless.residual = !options.full_coded;
    config.core.dialnorm = options.dialnorm;
    config.peak.ceiling = options.ceiling;
    config.peak.buffer_code = options.buffer;
    let mut encoder =
        master::Encoder::new(config).map_err(|e| Error::refused(&options.input, e.0))?;
    let per_frame = FRAME_SAMPLES * options.channels;
    let mut frame = vec![0i32; per_frame];
    let mut stream = Vec::new();
    let mut frames = 0;
    let started = std::time::Instant::now();
    for chunk in reference.pcm.chunks(per_frame) {
        frame[..chunk.len()].copy_from_slice(chunk);
        frame[chunk.len()..].fill(0);
        encoder
            .encode(&frame, &mut stream)
            .map_err(|e| refused(&options.input, frames, e))?;
        frames += 1;
    }
    encoder
        .finish(&mut stream)
        .map_err(|e| refused(&options.input, frames, e))?;
    let took = started.elapsed().as_secs_f64();
    std::fs::write(&options.out, &stream).map_err(|e| Error::io(&options.out, e))?;
    let pcm_bytes = reference.pcm.len() * usize::from(bits) / 8;
    println!(
        "encoded {frames} frames, {} bytes: {:.1} % of the {bits}-bit PCM, the {}-byte cores \
         included; in {took:.2} s, {:.0}× real time",
        stream.len(),
        100.0 * stream.len() as f64 / pcm_bytes as f64,
        encoder.core_frame_size(),
        frames as f64 * FRAME_SAMPLES as f64 / 48000.0 / took
    );
    let core_channels = 6;
    println!(
        "  lossless layer         {} bytes, {:.1} % of the PCM, {:.0} kb/s; {:.2} of {core_channels} \
         channels a frame coded against the core",
        encoder.lossless_bytes(),
        100.0 * encoder.lossless_bytes() as f64 / pcm_bytes as f64,
        encoder.lossless_bytes() as f64 * 8.0 * 48000.0 / (frames as f64 * 512.0) / 1000.0,
        encoder.against_core() as f64 / frames as f64
    );
    let rate = encoder.rate();
    let mbps = |bytes: usize| bytes as f64 * 8.0 * 48000.0 / 512.0 / 1e6;
    println!(
        "  peak rate              {:.3} Mb/s at the largest frame ({} bytes) under {:.3} ({} \
         bytes); {} frames sent partly ahead; buffer {}, {} bytes held at most",
        mbps(rate.largest),
        rate.largest,
        mbps(rate.budget),
        rate.budget,
        rate.ahead,
        if rate.buffer == 0 {
            "none stated".to_string()
        } else {
            format!("{} bytes stated", rate.buffer)
        },
        rate.held
    );
    let decoded = dts::run(dts::Options {
        input: options.out.clone(),
        frames: None,
        reference: Some(Reference {
            pcm: reference.pcm.clone(),
            channels: reference.channels,
        }),
        unfold: None,
        rate_log: None,
        ceiling: Some(options.ceiling),
    });
    // FFmpeg decodes the bed, and of a 7.1.4 that is the composite.
    let ffmpeg = if options.channels == 12 {
        let [composite, _, _] = dts::split_7_1_4(&reference);
        through_ffmpeg(&options.out, &composite, "DTS-HD MA + DTS:X,")
    } else {
        through_ffmpeg(&options.out, &reference, "DTS-HD MA,")
    };
    decoded?;
    ffmpeg
}

/// A refusal of the encoder's as an error naming the input. A refusal of the
/// resolution names the sample within the frame, so the frame — `frames`
/// sent before it — is added; the others name it themselves.
fn refused(input: &Path, frames: usize, e: master::Refused) -> Error {
    match e {
        master::Refused::Resolution { .. } => Error::refused(input, format!("frame {frames}: {e}")),
        _ => Error::refused(input, e.to_string()),
    }
}

/// Decode `stream` through FFmpeg, its checksums checked, and compare it with
/// `reference` sample for sample; the profile FFmpeg reports has to start
/// with `profile`. Skipped, and said so, without FFmpeg.
fn through_ffmpeg(stream: &Path, reference: &Reference, expected: &str) -> Result<()> {
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=profile,channels",
            "-of",
            "csv=p=0",
        ])
        .arg(stream)
        .output();
    let Ok(probe) = probe else {
        println!("  ffmpeg                 not installed: not decoded");
        return Ok(());
    };
    let profile = String::from_utf8_lossy(&probe.stdout).trim().to_string();
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-err_detect", "crccheck", "-f", "dts", "-i"])
        .arg(stream)
        .args(["-f", "s32le", "-"])
        .output()
        .map_err(|e| Error::io(stream, e))?;
    let complaints = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let channels = reference.channels;
    let samples: Vec<i32> = output
        .stdout
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let planes: Vec<Vec<i32>> = (0..channels)
        .map(|ch| {
            samples
                .iter()
                .skip(ch)
                .step_by(channels)
                .map(|&s| s >> 8)
                .collect()
        })
        .collect();
    let exact = dts::compare_exact(reference, &planes);
    println!("  ffmpeg                 {profile}; {exact}");
    let mut failures = Vec::new();
    if !complaints.is_empty() {
        println!(
            "  ffmpeg says            {}",
            complaints.replace('\n', "; ")
        );
        failures.push("FFmpeg reports errors".to_string());
    }
    if !profile.starts_with(expected) {
        failures.push(format!(
            "FFmpeg sees {profile:?}, not {}",
            expected.trim_end_matches(',')
        ));
    }
    if !exact.is_exact() {
        failures.push(format!(
            "a decode through FFmpeg that is not its input: {exact}"
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::refused(stream, failures.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of 5.1 or 7.1 that gives every stage something to do:
    /// tones, a correlated pair, noise, silence, a chirp, and a side and
    /// its rear both at full scale, which the fold takes past 24 bits.
    fn signal(channels: usize, bits: u8) -> Vec<i32> {
        let mut state = 0x9e37_79b9u32;
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            f64::from(state as i32) / f64::from(i32::MAX)
        };
        let full = f64::from((1u32 << (bits - 1)) - 1);
        let mut pcm = Vec::with_capacity(48000 * channels);
        for n in 0..48000 {
            let t = f64::from(n) / 48000.0;
            let tone = |f: f64, phase: f64| (std::f64::consts::TAU * f * t + phase).sin();
            for ch in 0..channels {
                let v = match ch {
                    0 => 0.7 * tone(440.0, 0.0),
                    1 => 0.6 * tone(440.0, 0.4) + 0.01 * noise(),
                    2 => tone(100.0 + 2000.0 * t, 0.0),
                    3 => 0.0,
                    // The rears of a 7.1, the surrounds of a 5.1.
                    4 => noise(),
                    5 => 0.3 * tone(60.0, 0.0),
                    // The sides: square at full scale, with rear 4's noise.
                    _ => {
                        if (n / 64) % 2 == 0 {
                            1.0
                        } else {
                            -1.0
                        }
                    }
                };
                let v = (v * full).round().clamp(-full - 1.0, full) as i32;
                pcm.push(v << (32 - bits));
            }
        }
        pcm
    }

    /// Phase C2's acceptance: 5.1 and 7.1 at 16 and 24 bits come back bit
    /// for bit, every frame and every channel, through `dca` (with the
    /// `dca-oracle` feature) and through FFmpeg (where it is installed);
    /// every header of the stream, the lossless frames' included, reads and
    /// writes back byte for byte with its checksum valid. Each with the
    /// channels the core carries coded against it (C2b), the core with ADPCM
    /// and without, and each coded alone (C2a); and two shapes off the
    /// reference streams': eight segments without the split flag at 24 bits,
    /// and 256 segments of two samples with it at 16, the core with ADPCM.
    #[test]
    fn master_audio_comes_back_bit_for_bit() {
        let mut shapes = Vec::new();
        for (channels, bits) in [(6, 16), (6, 24), (8, 16), (8, 24)] {
            for (adpcm, full_coded) in [(false, false), (true, false), (false, true)] {
                shapes.push((channels, bits, None, None, adpcm, full_coded));
            }
        }
        shapes.push((8, 24, Some(3), Some(false), false, false));
        shapes.push((6, 16, Some(8), Some(true), true, false));
        for (channels, bits, segments_log2, lsb_split, adpcm, full_coded) in shapes {
            let what = format!(
                "{channels} channels at {bits} bits, segments {segments_log2:?}, adpcm {adpcm}, \
                 full-coded {full_coded}"
            );
            let dir = std::env::temp_dir().join(format!(
                "hz-dts-ma-{}-{channels}-{bits}-{segments_log2:?}-{adpcm}-{full_coded}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let input = dir.join("in.s32");
            let bytes: Vec<u8> = signal(channels, bits)
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            std::fs::write(&input, bytes).unwrap();
            let result = run(Options {
                input,
                channels,
                lossless: Some(bits),
                sample_rate: 48000,
                bit_rate: 1_509_000,
                adpcm,
                ffmpeg_lfe: false,
                segments_log2,
                lsb_split,
                full_coded,
                ceiling: hz_dca::peak::CEILING,
                buffer: 8,
                dialnorm: 0,
                out: dir.join("out.dts"),
            });
            std::fs::remove_dir_all(&dir).unwrap();
            result.unwrap_or_else(|e| panic!("{what}: {e}"));
        }
    }

    /// Encode `pcm` as `run` would from a file, in a directory of its own.
    fn encode_file(name: &str, pcm: &[i32], channels: usize, adpcm: bool) -> Result<()> {
        encode_under(name, pcm, channels, adpcm, hz_dca::peak::CEILING)
    }

    /// The same under `ceiling` bits a second.
    fn encode_under(
        name: &str,
        pcm: &[i32],
        channels: usize,
        adpcm: bool,
        ceiling: u32,
    ) -> Result<()> {
        let dir = std::env::temp_dir().join(format!("hz-dts-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.s32");
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(&input, bytes).unwrap();
        let result = run(Options {
            input,
            channels,
            lossless: Some(24),
            sample_rate: 48000,
            bit_rate: 1_509_000,
            adpcm,
            ffmpeg_lfe: false,
            segments_log2: None,
            lsb_split: None,
            full_coded: false,
            ceiling,
            buffer: 8,
            dialnorm: 0,
            out: dir.join("out.dts"),
        });
        std::fs::remove_dir_all(&dir).unwrap();
        result
    }

    /// Phase C3's acceptance (a) and (b) on synthetic 7.1.4: through `dca`,
    /// the composite bed, the heights and their unfold bit for bit (with the
    /// `dca-oracle` feature); through FFmpeg, the composite and the DTS:X
    /// profile (where it is installed); every header, the extension block's
    /// included, rewriting byte for byte. The 7.1 is `signal`'s at half
    /// scale, so that its full-scale square in the sides cannot pass 24
    /// bits with a height in it; the heights are its first four channels
    /// again, at half scale.
    #[test]
    fn the_standard_dtsx_profile_comes_back_bit_for_bit() {
        let bed = signal(8, 24);
        let mut pcm = Vec::with_capacity(bed.len() / 8 * 12);
        for frame in bed.chunks_exact(8) {
            pcm.extend(frame.iter().map(|&s| s >> 9 << 8));
            pcm.extend(frame[..4].iter().map(|&s| s >> 9 << 8));
        }
        for adpcm in [false, true] {
            encode_file(&format!("dtsx-{adpcm}"), &pcm, 12, adpcm)
                .unwrap_or_else(|e| panic!("adpcm {adpcm}: {e}"));
        }
    }

    /// Phase C3's acceptance (e): white noise in all twelve channels, 24-bit
    /// at half scale so that no fold passes 24 bits, is refused by the
    /// navigation check at its first frame, as the domain says: the bed
    /// alone passes the furthest offset the word can name. At an eighth of
    /// full scale it is still refused; at a 4096th the stream is lossless.
    #[test]
    fn white_noise_is_refused_by_the_navigation_check_or_comes_back() {
        let noise = |shift: u32| {
            let mut state = 0x2545_f491u32;
            (0..48000 * 12)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    (state as i32) >> shift << 8
                })
                .collect::<Vec<i32>>()
        };
        for shift in [9, 11] {
            let e = encode_file(&format!("noise-{shift}"), &noise(shift), 12, false)
                .expect_err("white noise past the navigation word's reach");
            let message = e.to_string();
            assert!(
                message.contains("frame 0 (from sample 0)")
                    && message.contains("cannot be navigated"),
                "{message}"
            );
        }
        encode_file("noise-20", &noise(20), 12, false).unwrap();
    }

    /// Phase C4's worst case: 24-bit white noise at full scale in every
    /// channel of a 7.1, alone and in a burst a quarter of a second long
    /// between tones. At the default ceiling both go out whole, every frame
    /// in its own substream. At 9 Mb/s the burst is smoothed — sent partly
    /// ahead into the decoder's buffer, under the ceiling — and comes back
    /// bit for bit through `dca` and FFmpeg, every substream feeding the
    /// buffer as both keep it; the noise alone is refused at its first
    /// frame, which no earlier substream can help carry. The same burst in a
    /// 7.1.4, at 2^-10 of full scale where the navigation word still
    /// reaches the heights, is smoothed under 8 Mb/s.
    #[test]
    fn white_noise_bursts_are_smoothed_under_the_ceiling_or_refused() {
        let pcm = |channels: usize, shift: u32, burst: std::ops::Range<usize>| {
            let mut state = 0x2545_f491u32;
            let tones = signal(channels.min(8), 24);
            (0..48000)
                .flat_map(|n| (0..channels).map(move |ch| (n, ch)))
                .map(|(n, ch)| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    let v = if burst.contains(&(n / FRAME_SAMPLES)) {
                        state as i32
                    } else {
                        // The 7.1's channels again for the heights; its
                        // noise rear and full-scale sides only in a 7.1.
                        match ch % 8 {
                            4 | 6 | 7 if channels == 12 => 0,
                            c => tones[n * 8 + c],
                        }
                    };
                    v >> shift >> 8 << 8
                })
                .collect::<Vec<i32>>()
        };
        let noise = pcm(8, 0, 0..94);
        let burst = pcm(8, 0, 40..64);
        encode_file("worst-noise", &noise, 8, false).unwrap();
        encode_file("worst-burst", &burst, 8, false).unwrap();
        encode_under("worst-burst-9", &burst, 8, false, 9_000_000).unwrap();
        let e = encode_under("worst-noise-9", &noise, 8, false, 9_000_000)
            .expect_err("white noise over the ceiling from its first frame")
            .to_string();
        assert!(
            e.contains("frame 0 (from sample 0)")
                && e.contains("more than its substream can carry"),
            "{e}"
        );
        encode_under(
            "worst-burst-714",
            &pcm(12, 10, 40..64),
            12,
            false,
            8_000_000,
        )
        .unwrap();
    }

    #[test]
    fn a_sample_off_by_one_is_not_exact() {
        let reference = Reference {
            pcm: (0..64).map(|n| n << 8).collect(),
            channels: 2,
        };
        let mut planes: Vec<Vec<i32>> = (0..2)
            .map(|ch| (0..32).map(|n| 2 * n + ch).collect())
            .collect();
        assert!(dts::compare_exact(&reference, &planes).is_exact());
        planes[1][7] += 1;
        let exact = dts::compare_exact(&reference, &planes);
        assert_eq!(exact.differing, vec![(1, 1, 7)]);
        planes[0].pop();
        assert_eq!(dts::compare_exact(&reference, &planes).short, vec![0]);
    }
}
