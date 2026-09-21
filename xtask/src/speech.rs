//! Calibrate and report on the speech gate.
//!
//! The gate's thresholds are ours — nothing specifies them — so they were set
//! by looking at what the features actually do on labelled material rather
//! than by choosing round numbers. This is the harness that produced them, and
//! it is kept so the next person can disagree with them from the same data:
//!
//! ```text
//! cargo xtask speech --speech en.wav --speech fr.wav \
//!                    --other pink.wav --other chord.wav --other tone.wav
//! ```
//!
//! It also builds the test the gate exists to pass. `--mix` lays speech at a
//! known loudness against other material at a different known loudness and
//! asks what the gate recovers: the programme figure should read the mixture,
//! the dialogue figure should read the speech. That is a ground truth the
//! detector cannot talk its way around.

use hz_analysis::speech::{Analysis, DialogueMeter, Features, SpeechGate, Thresholds};
use hz_core::{Error, Result};
use hz_io::container::pcm::PcmFormat;
use hz_io::container::wav::{RiffKind, WavFormat, WavReader, WavWriter};
use std::path::{Path, PathBuf};

const BLOCK_FRAMES: usize = 8192;

pub struct Options {
    pub speech: Vec<PathBuf>,
    pub other: Vec<PathBuf>,
    /// Which channel to listen to. Defaults to the only one, or to the centre
    /// of a 5.1 or 7.1 file.
    pub channel: Option<usize>,
    /// Print the feature distributions, not just the verdicts.
    pub features: bool,
    /// Build a mixture of the first speech and the first other file, at these
    /// two loudnesses, and report what the gate recovers.
    pub mix: bool,
    pub speech_lufs: f64,
    pub other_lufs: f64,
    pub out: Option<PathBuf>,
    /// Threshold overrides, so a default can be argued from a sweep rather
    /// than from a rebuild.
    pub cpp: Option<f64>,
    pub band_ratio: Option<f64>,
    pub flux: Option<f64>,
    pub modulation: Option<f64>,
    pub snr: Option<f64>,
    pub block_voice: Option<f64>,
    pub hangover: Option<usize>,
}

impl Options {
    fn thresholds(&self) -> Thresholds {
        let mut thresholds = Thresholds::default();
        if let Some(value) = self.cpp {
            thresholds.cpp_db = value;
        }
        if let Some(value) = self.band_ratio {
            thresholds.speech_band_ratio = value;
        }
        if let Some(value) = self.flux {
            thresholds.flux = value;
        }
        if let Some(value) = self.modulation {
            thresholds.modulation_db = value;
        }
        if let Some(value) = self.snr {
            thresholds.snr_db = value;
        }
        if let Some(value) = self.block_voice {
            thresholds.block_voice_fraction = value;
        }
        if let Some(value) = self.hangover {
            thresholds.hangover_frames = value;
        }
        thresholds
    }
}

pub fn run(options: Options) -> Result<()> {
    if options.mix {
        return mix(&options);
    }

    println!(
        "{:<20} {:>7} {:>7} {:>7} {:>7}",
        "file", "frames", "raw", "open", "blocks"
    );
    println!("{}", "-".repeat(52));

    let mut reports = Vec::new();
    for path in options.speech.iter().chain(&options.other) {
        let label = if options.speech.contains(path) {
            Label::Speech
        } else {
            Label::Other
        };
        let report = examine(path, options.channel, options.thresholds())?;
        println!(
            "{:<20} {:>7} {:>6.1}% {:>6.1}% {:>6.1}%",
            short(path),
            report.frames,
            100.0 * report.raw_rate,
            100.0 * report.open_rate,
            100.0 * report.block_rate,
        );
        reports.push((label, report));
    }

    if options.features {
        println!();
        print_features(&reports);
    }

    println!();
    let coverage = reports
        .iter()
        .filter(|(label, _)| *label == Label::Speech)
        .map(|(_, r)| r.open_rate)
        .fold(f64::INFINITY, f64::min);
    let false_alarm = reports
        .iter()
        .filter(|(label, _)| *label == Label::Other)
        .map(|(_, r)| r.open_rate)
        .fold(0.0, f64::max);
    if coverage.is_finite() {
        println!("worst speech coverage    {:>6.1}%", 100.0 * coverage);
    }
    println!("worst false alarm        {:>6.1}%", 100.0 * false_alarm);

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    Speech,
    Other,
}

struct Report {
    frames: usize,
    raw_rate: f64,
    open_rate: f64,
    block_rate: f64,
    traced: Vec<Features>,
}

fn examine(path: &Path, channel: Option<usize>, thresholds: Thresholds) -> Result<Report> {
    let (rate, samples, channels) = read_mono(path, channel)?;

    let mut gate = SpeechGate::with_thresholds(rate, thresholds);
    gate.trace();
    for block in samples.chunks(BLOCK_FRAMES) {
        gate.push(block);
    }
    gate.flush();
    let _ = channels;

    let raw = gate.raw_frames();
    let open = gate.open_frames();
    let blocks = gate.blocks();

    Ok(Report {
        frames: raw.len(),
        raw_rate: rate_of(raw),
        open_rate: rate_of(&open),
        block_rate: rate_of(&blocks),
        traced: gate.traced().to_vec(),
    })
}

fn rate_of(flags: &[bool]) -> f64 {
    if flags.is_empty() {
        return 0.0;
    }
    flags.iter().filter(|f| **f).count() as f64 / flags.len() as f64
}

/// The distributions the thresholds were read off.
///
/// Only frames with something in them: a feature measured on room tone is a
/// property of the dither, and averaging it in would hide the thing being
/// looked for.
fn print_features(reports: &[(Label, Report)]) {
    println!(
        "{:<10} {:>18} {:>18} {:>18} {:>18}",
        "class", "band ratio", "cpp dB", "modulation dB", "flux"
    );
    println!("{}", "-".repeat(86));

    for label in [Label::Speech, Label::Other] {
        let mut band = Vec::new();
        let mut cpp = Vec::new();
        let mut modulation = Vec::new();
        let mut flux = Vec::new();
        for (_, report) in reports.iter().filter(|(l, _)| *l == label) {
            for frame in report
                .traced
                .iter()
                .filter(|f| f.level_db > f.floor_db + 8.0)
            {
                band.push(frame.speech_band_ratio);
                cpp.push(frame.cpp);
                modulation.push(frame.modulation);
                if frame.flux.is_finite() {
                    flux.push(frame.flux);
                }
            }
        }
        let name = match label {
            Label::Speech => "speech",
            Label::Other => "other",
        };
        println!(
            "{name:<10} {:>18} {:>18} {:>18} {:>18}",
            spread(&mut band),
            spread(&mut cpp),
            spread(&mut modulation),
            spread(&mut flux),
        );
    }
    println!();
    println!("(50th / 75th / 90th / 99th percentile, over frames above the noise floor)");
}

fn spread(values: &mut [f64]) -> String {
    if values.is_empty() {
        return "        -".to_string();
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |f: f64| values[((values.len() - 1) as f64 * f).round() as usize];
    format!(
        "{:.2}/{:.2}/{:.2}/{:.2}",
        at(0.50),
        at(0.75),
        at(0.90),
        at(0.99)
    )
}

/// Lay speech at one loudness against other material at another, and ask the
/// gate to recover the speech level from the mixture.
fn mix(options: &Options) -> Result<()> {
    let speech_path = options
        .speech
        .first()
        .ok_or_else(|| Error::unsupported(Path::new("--mix"), "needs a --speech file"))?;
    let other_path = options
        .other
        .first()
        .ok_or_else(|| Error::unsupported(Path::new("--mix"), "needs an --other file"))?;

    let (rate, speech, _) = read_mono(speech_path, options.channel)?;
    let (other_rate, other, _) = read_mono(other_path, options.channel)?;
    if other_rate != rate {
        return Err(Error::unsupported(
            other_path,
            format!("{other_rate} Hz against {rate} Hz: resample first"),
        ));
    }

    // Scale each part so that, measured alone, it reads its target loudness.
    let speech = scaled(rate, &speech, options.speech_lufs);
    let other = scaled(rate, &other, options.other_lufs);

    // Alternate them, whole seconds at a time, so every block is one or the
    // other rather than a fade between them.
    let segment = rate as usize * 6;
    let mut timeline: Vec<f32> = Vec::new();
    let mut speech_at = 0;
    let mut other_at = 0;
    for turn in 0..6usize {
        let (source, at) = if turn.is_multiple_of(2) {
            (&speech, &mut speech_at)
        } else {
            (&other, &mut other_at)
        };
        for _ in 0..segment {
            timeline.push(source[*at % source.len()]);
            *at += 1;
        }
    }

    if let Some(path) = &options.out {
        let mut writer = WavWriter::create(
            path,
            RiffKind::Rf64,
            WavFormat::plain(PcmFormat::wav(f64::from(rate), 1, 24)),
        )?;
        let scale = 8_388_607.0f32;
        let words: Vec<i32> = timeline
            .iter()
            .map(|s| (s * scale).clamp(-scale, scale) as i32)
            .collect();
        writer.write_frames(&words)?;
        writer.finish()?;
        println!("wrote {}", path.display());
    }

    let mut meter = DialogueMeter::with_thresholds(
        rate,
        &[hz_analysis::ChannelWeight::Unity],
        Analysis::Channel(0),
        options.thresholds(),
    );
    for block in timeline.chunks(BLOCK_FRAMES) {
        meter.push(block);
    }
    meter.flush();

    // The timeline was built here, so which blocks are speech is known
    // exactly. That turns the gate's verdict into a confusion matrix rather
    // than an impression.
    let kept = meter.gate().blocks();
    let subblock = hz_analysis::loudness::subblock_frames(rate);
    let mut hit = 0usize;
    let mut miss = 0usize;
    let mut leaked = 0usize;
    let mut rejected = 0usize;
    for (index, &dialogue) in kept.iter().enumerate() {
        // Block `index` covers sub-blocks `index..index+4`.
        let from = index * subblock;
        let to = from + 4 * subblock;
        let truth = (from / segment).is_multiple_of(2)
            && (to.saturating_sub(1) / segment).is_multiple_of(2);
        match (truth, dialogue) {
            (true, true) => hit += 1,
            (true, false) => miss += 1,
            (false, true) => leaked += 1,
            (false, false) => rejected += 1,
        }
    }

    println!(
        "half speech at {:.1} LUFS, half other at {:.1} LUFS",
        options.speech_lufs, options.other_lufs
    );
    println!("  programme    {:>8.2} LUFS", meter.integrated());
    println!("  dialogue     {:>8.2} LUFS", meter.dialogue());
    println!(
        "  kept         {:>7.1}% of blocks",
        100.0 * meter.dialogue_fraction()
    );
    println!(
        "  error        {:>8.2} LU against the speech target",
        meter.dialogue() - options.speech_lufs
    );
    println!(
        "  blocks       {hit} kept in speech, {miss} missed, {leaked} leaked from the other material, {rejected} rejected"
    );

    // Where the leakage sits: at the seams, or spread through the segment.
    let blocks_per_segment = segment / subblock;
    let mut per_segment = [(0usize, 0usize); 6];
    for (index, &dialogue) in kept.iter().enumerate() {
        let which = (index / blocks_per_segment.max(1)).min(5);
        per_segment[which].1 += 1;
        if dialogue {
            per_segment[which].0 += 1;
        }
    }
    print!("  per segment ");
    for (which, (dialogue, total)) in per_segment.iter().enumerate() {
        let what = if which.is_multiple_of(2) {
            "speech"
        } else {
            "other"
        };
        print!(" {what} {dialogue}/{total}");
    }
    println!();
    Ok(())
}

/// The same samples, scaled so that they measure `target` on their own.
fn scaled(rate: u32, samples: &[f32], target: f64) -> Vec<f32> {
    let mut meter = hz_analysis::Meter::new(rate, &[hz_analysis::ChannelWeight::Unity]);
    for block in samples.chunks(BLOCK_FRAMES) {
        meter.push(block);
    }
    meter.flush();
    let measured = meter.integrated();
    if !measured.is_finite() {
        return samples.to_vec();
    }
    let gain = 10f32.powf(((target - measured) / 20.0) as f32);
    samples.iter().map(|s| s * gain).collect()
}

fn read_mono(path: &Path, channel: Option<usize>) -> Result<(u32, Vec<f32>, usize)> {
    let mut reader = WavReader::open(path)?;
    let format = reader.format().pcm;
    let channels = format.channels as usize;
    if channels == 0 {
        return Err(Error::malformed(path, "no channels"));
    }
    // A 5.1 or 7.1 file without an explicit choice: the centre, which is where
    // dialogue is and what the gate is meant to be pointed at.
    let pick = channel.unwrap_or(if channels >= 6 { 2 } else { 0 });

    let scale = format.full_scale();
    let mut words = vec![0i32; BLOCK_FRAMES * channels];
    let mut out = Vec::new();
    loop {
        let frames = reader.read_frames(&mut words)?;
        if frames == 0 {
            break;
        }
        for frame in words[..frames * channels].chunks_exact(channels) {
            out.push(frame[pick.min(channels - 1)] as f32 / scale);
        }
    }
    Ok((format.sample_rate_hz(), out, channels))
}

fn short(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Sweep one threshold and print what it costs and buys.
///
/// Unused by the command line: it exists so that a change to the defaults can
/// be argued from a table rather than from a hunch.
#[allow(dead_code)]
pub fn sweep(path: &Path, values: &[f64]) -> Result<()> {
    let (rate, samples, _) = read_mono(path, None)?;
    for &cpp_db in values {
        let mut gate = SpeechGate::with_thresholds(
            rate,
            Thresholds {
                cpp_db,
                ..Thresholds::default()
            },
        );
        for block in samples.chunks(BLOCK_FRAMES) {
            gate.push(block);
        }
        gate.flush();
        println!(
            "cpp {cpp_db:>5.2}  open {:>6.1}%",
            100.0 * rate_of(&gate.open_frames())
        );
    }
    Ok(())
}
