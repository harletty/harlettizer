//! Hold the objects an `harlettizer ac3 --eac3 --joc` stream carries to the
//! master they were made from.
//!
//! ```text
//! cargo run -p xtask --features eac3-oracle -- joc-check stream.ec3 --master in.atmos
//! ```
//!
//! The stream's objects are rebuilt by the workspace decoder, as a receiver
//! rebuilds them — from its decoded core, by the gains its payloads carry —
//! and each is compared with its element of the master: an object with its
//! gains applied, or a bed channel, the LFE left out, in the order the
//! encoder takes them. The decoder hands objects back the encoder's delay
//! plus the joint object coding latency after the master's samples went in.

use crate::graft::decode_objects;
use hz_core::{Error, Result};
use hz_programme::path::Path as Trajectory;
use hz_programme::source::Source;
use hz_programme::{Part, parts_of};
use std::path::PathBuf;

pub struct Options {
    pub stream: PathBuf,
    pub master: PathBuf,
    pub mono_prefix: Option<PathBuf>,
}

/// Samples a decoder's objects lag the master by: `hz-ac3`'s delay and joint
/// object coding's.
const LAG: usize = hz_ac3::DELAY + eac3::JOC_LATENCY_SAMPLES;
const FRAME: usize = hz_ac3::FRAME_SAMPLES;

pub fn run(options: Options) -> Result<()> {
    let decoded = decode_objects(&options.stream)?;
    let oamd = decoded.oamd;
    let decoded = decoded.objects;
    let elements = elements(&options)?;
    if decoded.len() != elements.len() {
        return Err(Error::malformed(
            &options.stream,
            format!(
                "{} objects, where the master has {} elements",
                decoded.len(),
                elements.len()
            ),
        ));
    }
    let frames = decoded.first().map_or(0, Vec::len) / FRAME;
    println!(
        "{}: {} objects over {frames} syncframes, against {}",
        options.stream.display(),
        decoded.len(),
        options.master.display()
    );
    // The first two frames, while the decoder's history fills, and the last
    // two, past the master's end, are left out.
    let (start, end) = (2 * FRAME, frames.saturating_sub(2) * FRAME);
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
        "  objects      {:.2} dB against the master's elements, by element {}",
        snr(signal, error),
        by_element.join(" ")
    );
    Ok(())
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
