//! What gain a shipped stream states, and when it changes it.
//!
//! ```text
//! cargo xtask drc programme.thd
//! cargo xtask drc programme.thd --each          # one line per word
//! ```
//!
//! # Why this reads streams and does not run an encoder
//!
//! [`LEGAL.md`](../../LEGAL.md) §3.1 admits a measurement against the
//! proprietary encoder only from a licensed run of it, and none has been
//! available to this project. That closes the obvious way to recover its
//! compression curves — feed a level ramp in, read the gains out.
//!
//! It does not close this one. A shipped stream is a published work that the
//! project already reads from end to end, and the gain words are in it.
//! Nothing here runs the encoder, traces it, or looks inside it; this reads
//! the same bytes a decoder reads.
//!
//! # What it can and cannot recover
//!
//! It recovers **what a real stream states**: the value, the cadence, how it
//! moves, and how the four presentations differ. That is enough to answer
//! whether the word carries a compression curve at all.
//!
//! A curve is a gain as a *function of level*, so recovering one needs the
//! audio beside the words. Decode the presentation the substream carries and
//! pass it with `--audio`:
//!
//! ```text
//! truehdd decode --presentation 0 --format pcm --output-path programme_p0 programme.thd
//! cargo xtask drc programme.thd --substream 0 --audio programme_p0.pcm --channels 2
//! ```
//!
//! The level is a leaky integrator over the access units' power, and
//! `--tau` sweeps its time constant: the reference's gains follow one of about
//! 600 to 900 ms, which is what the correlation peaks at on every reference
//! stream tried.

use hz_core::{Error, Result};
use std::path::PathBuf;

pub struct Options {
    pub input: PathBuf,
    /// One line per word rather than the summary.
    pub each: bool,
    /// Stop after this many access units.
    pub units: Option<usize>,
    /// The decoded presentation, as raw 24-bit little-endian PCM, to join the
    /// gains to the level they answer.
    pub audio: Option<PathBuf>,
    /// How many channels that presentation carries.
    pub channels: usize,
    /// Which substream's words to join.
    pub substream: usize,
    /// The level detector's time constant, in milliseconds.
    pub tau: f64,
}

/// One stated gain, and where it was stated.
struct Word {
    unit: usize,
    substream: usize,
    gain: i32,
    deadline: u32,
}

pub fn run(options: Options) -> Result<()> {
    let bytes = std::fs::read(&options.input).map_err(|e| Error::io(&options.input, e))?;

    // The same walk `xtask thd` makes: the substream count and the Evolution
    // flag come from the major sync and are carried forward, so a unit cannot
    // be read without the ones before it.
    let limit = options.units.unwrap_or(usize::MAX);
    let mut at = 0usize;
    let mut substreams = 0usize;
    let mut evolution = false;
    let mut count = 0usize;
    let mut words: Vec<Word> = Vec::new();
    let mut widest_seen = 0usize;

    while at < bytes.len() && count < limit {
        let unit = match hz_mlp::reader::access_unit(&bytes[at..], substreams, evolution) {
            Ok(unit) => unit,
            Err(why) => {
                println!("access unit {count} at byte {at}: {why}");
                break;
            }
        };
        substreams = unit.directory.len();
        widest_seen = widest_seen.max(substreams);
        if let Some(sync) = &unit.major_sync {
            evolution = sync.flags & (1 << 12) != 0;
        }
        for (substream, entry) in unit.directory.iter().enumerate() {
            if entry.extra_word {
                words.push(Word {
                    unit: count,
                    substream,
                    gain: entry.drc_gain_update,
                    deadline: entry.drc_time_update,
                });
            }
        }
        at += unit.bytes;
        count += 1;
    }
    if count == 0 {
        return Err(Error::unsupported(&options.input, "no access units"));
    }

    println!("{}", options.input.display());
    println!(
        "  in           {} access units, {} substreams, {:.1} s at 48 kHz",
        count,
        widest_seen,
        count as f64 * 40.0 / 48_000.0
    );
    if words.is_empty() {
        println!("  drc          no word at all: the stream states no schedule");
        return Ok(());
    }

    if options.each {
        for word in &words {
            println!(
                "  {:>8.3}s  substream {}  gain {:>4} ({:+.2} dB)  stands {} units",
                word.unit as f64 * 40.0 / 48_000.0,
                word.substream,
                word.gain,
                gain_db(word.gain),
                1u32 << word.deadline
            );
        }
        println!();
    }

    // Per substream: what it says, how often, and how much it moves.
    let widest = widest_seen.saturating_sub(1);
    println!("  substream    words   gain            distinct  changes  cadence");
    for substream in 0..=widest {
        let mine: Vec<&Word> = words.iter().filter(|w| w.substream == substream).collect();
        if mine.is_empty() {
            println!("  {substream:<12} none");
            continue;
        }
        let low = mine.iter().map(|w| w.gain).min().unwrap_or(0);
        let high = mine.iter().map(|w| w.gain).max().unwrap_or(0);
        let mut seen: Vec<i32> = mine.iter().map(|w| w.gain).collect();
        seen.sort_unstable();
        seen.dedup();
        let changes = mine.windows(2).filter(|w| w[0].gain != w[1].gain).count();
        let cadence = mine
            .windows(2)
            .map(|w| w[1].unit - w[0].unit)
            .min()
            .unwrap_or(0);
        println!(
            "  {substream:<12} {:<7} {:>4}..{:<4} {:+.2}..{:+.2} dB  {:<9} {:<8} every {} units",
            mine.len(),
            low,
            high,
            gain_db(low),
            gain_db(high),
            seen.len(),
            changes,
            cadence,
        );
    }

    if let Some(audio) = &options.audio {
        curve(
            &words,
            audio,
            options.channels,
            options.substream,
            options.tau,
        )?;
    }

    // The whole point of the summary: a gain that never moves is not a
    // compression curve, whatever the field is called.
    let moving = (0..=widest).filter(|substream| {
        let mine: Vec<i32> = words
            .iter()
            .filter(|w| w.substream == *substream)
            .map(|w| w.gain)
            .collect();
        mine.windows(2).any(|w| w[0] != w[1])
    });
    let moving: Vec<usize> = moving.collect();
    println!();
    if moving.is_empty() {
        println!(
            "  finding      every substream states one gain and never changes it, so this word \
             carries no curve: it is a level, set once"
        );
    } else {
        println!(
            "  finding      substream{} {} change{} their gain, so something in the programme \
             moves {}",
            if moving.len() > 1 { "s" } else { "" },
            moving
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if moving.len() > 1 { "" } else { "s" },
            if moving.len() > 1 { "them" } else { "it" }
        );
    }
    Ok(())
}

/// What the gains answer to: the level of the presentation they apply to.
///
/// The detector is a one-pole leaky integrator over the power of each access
/// unit, which is the simplest thing that could be right and is enough to find
/// the time constant — the correlation peaks sharply, so the reference's is not
/// far from one.
///
/// What comes out is the curve binned by level, with the local slope beside it.
/// A slope of zero is a null band; a slope of −s is a ratio of `1/(1−s)`.
fn curve(
    words: &[Word],
    audio: &std::path::Path,
    channels: usize,
    substream: usize,
    tau_ms: f64,
) -> Result<()> {
    const UNIT: usize = 40;
    let bytes = std::fs::read(audio).map_err(|e| Error::io(audio, e))?;
    let stride = 3 * channels;
    let frames = bytes.len() / stride;
    if frames == 0 || channels == 0 {
        return Err(Error::unsupported(audio, "no frames"));
    }

    // The power of each access unit, summed over the channels: what a
    // detector integrates.
    let units = frames / UNIT;
    let mut power = vec![0.0f64; units];
    for (unit, into) in power.iter_mut().enumerate() {
        let mut sum = 0.0;
        for frame in 0..UNIT {
            for channel in 0..channels {
                let at = ((unit * UNIT + frame) * channels + channel) * 3;
                let raw = i32::from(bytes[at])
                    | (i32::from(bytes[at + 1]) << 8)
                    | (i32::from(bytes[at + 2]) << 16);
                let signed = if raw & 0x80_0000 != 0 {
                    raw - (1 << 24)
                } else {
                    raw
                };
                let value = f64::from(signed) / f64::from(1 << 23);
                sum += value * value;
            }
        }
        *into = sum / UNIT as f64;
    }

    // One pole, causal, over the units.
    let per_unit = 48_000.0 / UNIT as f64;
    let alpha = 1.0 / (tau_ms / 1000.0 * per_unit).max(1.0);
    let mut level = vec![0.0f64; units];
    let mut held = 0.0;
    for (unit, into) in level.iter_mut().enumerate() {
        held += alpha * (power[unit] - held);
        *into = held;
    }

    let mut pairs: Vec<(f64, f64)> = words
        .iter()
        .filter(|word| word.substream == substream)
        .filter_map(|word| {
            let at = word.unit.min(units.saturating_sub(1));
            (units > 0).then(|| (10.0 * level[at].max(1e-12).log10(), gain_db(word.gain)))
        })
        .collect();
    if pairs.len() < 16 {
        return Err(Error::unsupported(audio, "too few words to fit a curve"));
    }
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let levels: Vec<f64> = pairs.iter().map(|p| p.0).collect();
    let gains: Vec<f64> = pairs.iter().map(|p| p.1).collect();
    let (ml, mg) = (mean(&levels), mean(&gains));
    let cov: f64 = pairs.iter().map(|p| (p.0 - ml) * (p.1 - mg)).sum();
    let vl: f64 = levels.iter().map(|l| (l - ml) * (l - ml)).sum();
    let vg: f64 = gains.iter().map(|g| (g - mg) * (g - mg)).sum();

    println!();
    println!(
        "  curve        substream {substream}, {} words, detector {tau_ms:.0} ms,          correlation {:+.3}, slope {:+.3} dB/dB",
        pairs.len(),
        cov / (vl * vg).sqrt(),
        cov / vl
    );
    println!("  level        words   gain      slope     ratio");
    let mut previous: Option<(f64, f64)> = None;
    let (low, high) = (levels[0], levels[levels.len() - 1]);
    let mut edge = (low / 3.0).floor() * 3.0;
    while edge < high {
        let inside: Vec<f64> = pairs
            .iter()
            .filter(|p| p.0 >= edge && p.0 < edge + 3.0)
            .map(|p| p.1)
            .collect();
        if inside.len() >= 8 {
            let here = mean(&inside);
            let centre = edge + 1.5;
            let slope = previous.map(|(l, g)| (here - g) / (centre - l));
            println!(
                "  {edge:>6.0}..{:<6.0} {:<7} {here:+6.2} dB  {}  {}",
                edge + 3.0,
                inside.len(),
                slope.map_or("        ".to_string(), |s| format!("{s:+.2} dB/dB")),
                slope.map_or(String::new(), |s| {
                    // A slope of −1 is where the output stops rising at all;
                    // past it a louder input comes out *quieter*, which is not
                    // a ratio and should not be printed as one.
                    if s > -0.05 {
                        "flat".to_string()
                    } else if s <= -1.0 {
                        "over unity".to_string()
                    } else {
                        format!("{:.1}:1", 1.0 / (1.0 + s))
                    }
                })
            );
            previous = Some((centre, here));
        }
        edge += 3.0;
    }
    Ok(())
}

/// The field is sixty-fourths of a power of two.
fn gain_db(gain: i32) -> f64 {
    f64::from(gain) / 64.0 * 6.020_599_913_279_624
}
