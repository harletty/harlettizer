//! Encode PCM to DTS with `hz-dca`, then read the stream back.
//!
//! ```text
//! cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 6 --out out.dts
//! cargo run -p xtask --features dca-oracle -- dts-encode in.s32 --channels 8 --lossless 24 --out out.dts
//! ```
//!
//! Phase C1's round trip, and with `--lossless` phase C2's. The input is raw
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
    pub out: PathBuf,
}

pub fn run(options: Options) -> Result<()> {
    if let Some(bits) = options.lossless {
        return run_lossless(&options, bits);
    }
    if options.segments_log2.is_some() || options.lsb_split.is_some() {
        return Err(Error::refused(
            &options.input,
            "segments and the LSB split are the lossless layer's: they need --lossless",
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
    })
}

fn run_lossless(options: &Options, bits: u8) -> Result<()> {
    let layout = match options.channels {
        6 => lossless::Layout::FivePointOne,
        8 => lossless::Layout::SevenPointOne,
        n => {
            return Err(Error::refused(
                &options.input,
                format!("{n} channels: the lossless encoder takes 6 or 8"),
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
    let mut encoder =
        master::Encoder::new(config).map_err(|e| Error::refused(&options.input, e.0))?;
    let per_frame = FRAME_SAMPLES * options.channels;
    let mut frame = vec![0i32; per_frame];
    let mut stream = Vec::new();
    let mut frames = 0;
    for chunk in reference.pcm.chunks(per_frame) {
        frame[..chunk.len()].copy_from_slice(chunk);
        frame[chunk.len()..].fill(0);
        encoder
            .encode(&frame, &mut stream)
            .map_err(|e| Error::refused(&options.input, format!("frame {frames}: {e}")))?;
        frames += 1;
    }
    std::fs::write(&options.out, &stream).map_err(|e| Error::io(&options.out, e))?;
    let pcm_bytes = reference.pcm.len() * usize::from(bits) / 8;
    println!(
        "encoded {frames} frames, {} bytes: {:.1} % of the {bits}-bit PCM, the {}-byte cores \
         included",
        stream.len(),
        100.0 * stream.len() as f64 / pcm_bytes as f64,
        encoder.core_frame_size()
    );
    let decoded = dts::run(dts::Options {
        input: options.out.clone(),
        frames: None,
        reference: Some(Reference {
            pcm: reference.pcm.clone(),
            channels: reference.channels,
        }),
    });
    let ffmpeg = through_ffmpeg(&options.out, &reference);
    decoded?;
    ffmpeg
}

/// Decode `stream` through FFmpeg, its checksums checked, and compare it with
/// `reference` sample for sample. Skipped, and said so, without FFmpeg.
fn through_ffmpeg(stream: &Path, reference: &Reference) -> Result<()> {
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
    if !profile.starts_with("DTS-HD MA,") {
        failures.push(format!("FFmpeg sees {profile:?}, not DTS-HD MA"));
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

    /// Phase C2a's acceptance: 5.1 and 7.1 at 16 and 24 bits come back bit
    /// for bit, every frame and every channel, through `dca` (with the
    /// `dca-oracle` feature) and through FFmpeg (where it is installed);
    /// every header of the stream, the lossless frames' included, reads and
    /// writes back byte for byte with its checksum valid. The reference
    /// streams' shape for each, and two others: eight segments without the
    /// split flag at 24 bits, and 256 segments of two samples with it at 16,
    /// the core with ADPCM.
    #[test]
    fn master_audio_comes_back_bit_for_bit() {
        let shapes = [
            (6, 16, None, None, false),
            (6, 24, None, None, false),
            (8, 16, None, None, false),
            (8, 24, None, None, false),
            (8, 24, Some(3), Some(false), false),
            (6, 16, Some(8), Some(true), true),
        ];
        for (channels, bits, segments_log2, lsb_split, adpcm) in shapes {
            let dir = std::env::temp_dir().join(format!(
                "hz-dts-ma-{}-{channels}-{bits}-{segments_log2:?}",
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
                out: dir.join("out.dts"),
            });
            std::fs::remove_dir_all(&dir).unwrap();
            result.unwrap_or_else(|e| {
                panic!("{channels} channels at {bits} bits, segments {segments_log2:?}: {e}")
            });
        }
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
