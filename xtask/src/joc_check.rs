//! Hold the objects an `harlettizer ac3 --eac3 --joc` stream carries to the
//! master they were made from.
//!
//! ```text
//! cargo run -p xtask --features eac3-oracle -- joc-check stream.ec3 --master in.atmos
//! HZ_JOC_TRACE=dir harlettizer ac3 --eac3 --joc … && \
//!     cargo run -p xtask --features eac3-oracle -- joc-check stream.ec3 --master in.atmos --elements dir
//! ```
//!
//! The stream's objects are rebuilt by the workspace decoder, as a receiver
//! rebuilds them — from its decoded core, by the gains its payloads carry —
//! and each is compared with its element of the master: an object with its
//! gains applied, or a bed channel, the LFE left out, in the order the
//! encoder takes them. The decoder hands objects back the encoder's delay
//! plus the joint object coding latency after the master's samples went in.
//!
//! A master with more than fifteen elements is folded into fifteen, and its
//! objects are mixes of the master's: `--elements` names the encoder's trace
//! of them, which they are compared with instead. The trace says where each
//! element is too, so the scene is also rendered on 7.1.4 three ways — the
//! master's, the fold's elements where they are, and the decoded objects
//! where they are — and the last two held to the first: what the fold cost,
//! and what the stream costs.

use crate::graft::decode_objects;
use hz_core::{Error, Result};
use hz_programme::follow::Following;
use hz_programme::path::Path as Trajectory;
use hz_programme::source::Source;
use hz_programme::{Part, parts_of};
use hz_render::{Keyframe, Layout, Mixdown};
use std::path::{Path, PathBuf};

pub struct Options {
    pub stream: PathBuf,
    pub master: PathBuf,
    pub mono_prefix: Option<PathBuf>,
    /// The encoder's trace of the elements it carried — see
    /// `harlettizer ac3`'s `HZ_JOC_TRACE`.
    pub elements: Option<PathBuf>,
}

/// Samples a decoder's objects lag the master by: `hz-ac3`'s delay and joint
/// object coding's.
const LAG: usize = hz_ac3::DELAY + eac3::JOC_LATENCY_SAMPLES;
const FRAME: usize = hz_ac3::FRAME_SAMPLES;
/// Syncframes left out at each end: at the start while the decoder's
/// history fills, at the end past the master's.
const TRIM: usize = 2;

pub fn run(options: Options) -> Result<()> {
    let decoded = decode_objects(&options.stream)?;
    let oamd = decoded.oamd;
    let decoded = decoded.objects;
    let elements = match &options.elements {
        Some(dir) => traced(dir, decoded.len())?,
        None => elements(&options)?,
    };
    if decoded.len() != elements.len() {
        return Err(Error::malformed(
            &options.stream,
            format!(
                "{} objects, where the master has {} elements; a folded master's are \
                 compared with the encoder's trace of them, `--elements`",
                decoded.len(),
                elements.len()
            ),
        ));
    }
    let frames = decoded.first().map_or(0, Vec::len) / FRAME;
    if frames <= 2 * TRIM {
        return Err(Error::unsupported(
            &options.stream,
            format!(
                "{frames} syncframes, and the first and last {TRIM} are left out: \
                 {} at least to hold anything to the master",
                2 * TRIM + 1
            ),
        ));
    }
    println!(
        "{}: {} objects over {frames} syncframes, against {}",
        options.stream.display(),
        decoded.len(),
        options.master.display()
    );
    // The first two frames, while the decoder's history fills, and the last
    // two, past the master's end, are left out.
    let (start, end) = (TRIM * FRAME, (frames - TRIM) * FRAME);
    let (mut signal, mut error) = (0f64, 0f64);
    let mut by_element = Vec::new();
    for (object, element) in decoded.iter().zip(&elements) {
        let (mut s, mut e) = (0f64, 0f64);
        for (n, y) in object.iter().enumerate().take(end).skip(start) {
            let x = n
                .checked_sub(LAG)
                .and_then(|m| element.get(m))
                .copied()
                .unwrap_or(0.0);
            let y = f64::from(*y);
            s += x * x;
            e += (y - x) * (y - x);
        }
        signal += s;
        error += e;
        by_element.push(if s > 0.0 {
            format!("{:.1}", snr(s, e))
        } else {
            "silent".to_string()
        });
    }
    let mut counts = std::collections::BTreeMap::<usize, usize>::new();
    for count in &oamd {
        *counts.entry(*count).or_default() += 1;
    }
    println!(
        "  oamd         {} as the workspace decoder reads it",
        counts
            .iter()
            .map(|(objects, frames)| format!("{objects} elements ×{frames}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  objects      {:.2} dB against {}, by element {}",
        snr(signal, error),
        if options.elements.is_some() {
            "the elements the encoder carried"
        } else {
            "the master's elements"
        },
        by_element.join(" ")
    );
    if let Some(dir) = &options.elements {
        let positions = positions(&dir.join("positions.csv"), elements.len())?;
        let master = master_render(&options)?;
        // The master can end inside the frames scored: the scene is held
        // to it only as far as it goes.
        let end = end.min(master.len() / WIDTH);
        if end <= start {
            println!("  scene        the master ends before the frames left to score");
            return Ok(());
        }
        let carried = render(&elements, &positions, 0, master.len() / WIDTH);
        let rebuilt: Vec<Vec<f64>> = decoded
            .iter()
            .map(|object| object.iter().map(|&y| f64::from(y)).collect())
            .collect();
        let rebuilt = render(&rebuilt, &positions, LAG, master.len() / WIDTH);
        let range = start * WIDTH..end * WIDTH;
        let against = |scene: &[f64]| {
            let (mut s, mut e) = (0f64, 0f64);
            for (x, y) in master[range.clone()].iter().zip(&scene[range.clone()]) {
                s += x * x;
                e += (y - x) * (y - x);
            }
            snr(s, e)
        };
        println!(
            "  scene        on 7.1.4 against the master's render: {:.2} dB the elements \
             carried where they are, {:.2} dB the decoded objects",
            against(&carried),
            against(&rebuilt)
        );
    }
    Ok(())
}

/// Channels of the layout the scenes are rendered on.
const WIDTH: usize = 12;

/// The encoder's trace of `count` elements, each its whole signal on the
/// master's clock.
fn traced(dir: &Path, count: usize) -> Result<Vec<Vec<f64>>> {
    (0..count)
        .map(|n| {
            let path = dir.join(format!("element-{n}.f32"));
            let bytes = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
            Ok(bytes
                .chunks_exact(4)
                .map(|b| f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])))
                .collect())
        })
        .collect()
}

/// Where each of `count` elements is at the end of each syncframe's block,
/// a line a syncframe.
fn positions(path: &Path, count: usize) -> Result<Vec<Vec<[f64; 3]>>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    text.lines()
        .enumerate()
        .map(|(line, row)| {
            let values: Vec<f64> = row
                .split(',')
                .map(|x| x.trim().parse::<f64>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| Error::malformed(path, format!("line {}: {e}", line + 1)))?;
            if values.len() != 3 * count {
                return Err(Error::malformed(
                    path,
                    format!(
                        "line {}: {} values for {count} elements",
                        line + 1,
                        values.len()
                    ),
                ));
            }
            Ok(values.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect())
        })
        .collect()
}

/// The master, its LFE left out, rendered on 7.1.4 as `harlettizer ac3`
/// renders it: bed channels routed to their speakers, or placed where the
/// layout has none, objects on the room's cube.
fn master_render(options: &Options) -> Result<Vec<f64>> {
    let path = options.master.as_path();
    let mut source = Source::open(path, options.mono_prefix.as_deref())?;
    let scale = 1.0 / f64::from(1u32 << (source.pcm_format().bits_per_channel - 1));
    let stride = source.channels();
    let layout = Layout::surround_7_1_4();
    let mut following = Following::new(Mixdown::new(&layout)?);
    for part in parts_of(path, &source)? {
        match part {
            Part::Lfe { .. } => {}
            Part::Bed {
                channel,
                name,
                position,
            } => {
                let label = hz_core::speakers::by_master_name(name).map_or(name, |s| s.label);
                let mixdown = following.mixdown();
                match layout.index_of(label) {
                    Some(speaker) => {
                        mixdown.speaker(channel, speaker);
                    }
                    None => {
                        let index = mixdown.object(channel);
                        mixdown.update(
                            index,
                            &Keyframe {
                                position,
                                ..Keyframe::default()
                            },
                        );
                    }
                }
            }
            Part::Object { channel, keyframes } => following.object(channel, &keyframes),
        }
    }
    let total = source.frames() as usize;
    let mut raw = vec![0i32; FRAME * stride];
    let mut input = vec![0f32; FRAME * stride];
    let mut out = vec![0f64; total * WIDTH];
    let mut at = 0usize;
    while at < total {
        let got = source.fill(&mut raw, FRAME.min(total - at))?;
        if got == 0 {
            break;
        }
        for (x, &r) in input.iter_mut().zip(&raw[..got * stride]) {
            *x = (f64::from(r) * scale) as f32;
        }
        following.render(
            &input[..got * stride],
            stride,
            got,
            at as u64,
            &mut out[at * WIDTH..(at + got) * WIDTH],
        );
        at += got;
    }
    out.truncate(at * WIDTH);
    Ok(out)
}

/// `signals`, sample `lag + n` of each the master's sample `n`, rendered on
/// 7.1.4 for `frames` samples, each element ramping across its syncframe's
/// block — the span of the master a frame's objects come out as, [`LAG`]
/// behind the frame — to where `positions` puts it at the block's end, as
/// the encoder's weights ramp and a decoder's positions do.
fn render(
    signals: &[Vec<f64>],
    positions: &[Vec<[f64; 3]>],
    lag: usize,
    frames: usize,
) -> Vec<f64> {
    let count = signals.len();
    let layout = Layout::surround_7_1_4();
    let mut following = Following::new(Mixdown::new(&layout).expect("7.1.4"));
    let delay = LAG as u64;
    for element in 0..count {
        let keyframes: Vec<Keyframe> = positions
            .iter()
            .enumerate()
            .map(|(k, at)| {
                let start = (k * FRAME) as u64;
                Keyframe {
                    sample_pos: start.saturating_sub(delay),
                    position: at[element],
                    ramp_samples: (FRAME as u64 - delay.saturating_sub(start)) as u32,
                    ..Keyframe::default()
                }
            })
            .collect();
        following.object(element, &keyframes);
    }
    let mut out = vec![0f64; frames * WIDTH];
    let mut input = vec![0f32; FRAME * count];
    let mut at = 0;
    while at < frames {
        let got = FRAME.min(frames - at);
        for n in 0..got {
            for (element, signal) in signals.iter().enumerate() {
                input[n * count + element] =
                    signal.get(at + n + lag).copied().unwrap_or(0.0) as f32;
            }
        }
        following.render(
            &input[..got * count],
            count,
            got,
            at as u64,
            &mut out[at * WIDTH..(at + got) * WIDTH],
        );
        at += got;
    }
    out
}

fn snr(signal: f64, error: f64) -> f64 {
    10.0 * (signal / error.max(1e-30)).log10()
}

/// The master's elements as the encoder takes them, each its whole signal.
fn elements(options: &Options) -> Result<Vec<Vec<f64>>> {
    let path = options.master.as_path();
    let mut source = Source::open(path, options.mono_prefix.as_deref())?;
    let scale = 1.0 / f64::from(1u32 << (source.pcm_format().bits_per_channel - 1));
    let stride = source.channels();
    let mut channels: Vec<(usize, Option<Trajectory>)> = Vec::new();
    for part in parts_of(path, &source)? {
        match part {
            Part::Lfe { .. } => {}
            Part::Bed { channel, .. } => channels.push((channel, None)),
            Part::Object { channel, keyframes } => {
                channels.push((channel, Some(Trajectory::new(&keyframes))));
            }
        }
    }
    let total = source.frames() as usize;
    let mut raw = vec![0i32; FRAME * stride];
    let mut out = vec![Vec::with_capacity(total); channels.len()];
    let mut hints = vec![0usize; channels.len()];
    let mut at = 0usize;
    while at < total {
        let got = source.fill(&mut raw, FRAME.min(total - at))?;
        if got == 0 {
            break;
        }
        for (((channel, trajectory), hint), out) in channels.iter().zip(&mut hints).zip(&mut out) {
            for n in 0..got {
                let gain = trajectory
                    .as_ref()
                    .map_or(1.0, |t| t.gain((at + n) as u64, hint));
                out.push(f64::from(raw[n * stride + channel]) * scale * gain);
            }
        }
        at += got;
    }
    Ok(out)
}
