//! A listening bench: the scene and each rule's fold, rendered on 7.1.4 and
//! blinded, and the ranking once somebody has listened.
//!
//! ```text
//! cargo xtask bench programme.atmos --out bench/ --condition flat/fitted --condition kweighted/fitted
//! cargo xtask bench programme.atmos --out bench/ --score bench/sheet.csv
//! ```
//!
//! Every figure in `docs/clustering.md` is measured, not judged by ear, and
//! the notebook records ties the metric cannot settle: the plain power
//! against the K-weighted one, the fitted weighting against the nearest on
//! widths, the perceptual importance against both. Listening settles a tie,
//! and listening has to be reproducible or it is an opinion. This is the
//! reproducible form.
//!
//! # What it makes
//!
//! Rendered on 7.1.4 with the repository's own panner — the vector-base
//! amplitude panning of Pulkki that the metric's fourth presentation is —
//! from the same master, over the same seconds:
//!
//! - the **reference**: every object panned at its own position, ramped
//!   across each block the way a decoder ramps a payload, the bed at its
//!   speakers and the LFE passed through;
//! - the **anchor**: the reference through a low-pass at 3.5 kHz, which is
//!   what BS.1534 puts at the bottom of the scale so that the scale has a
//!   bottom;
//! - and one stimulus per **condition**, `loudness/weighting`: the scene
//!   folded into the elements asked for, steered by that loudness, weighed
//!   by that weighting, everything else as it ships, the elements mixed the
//!   way the encoder mixes them and panned where the fold put them.
//!
//! The files are named by a shuffle seeded from the run, the reference
//! among them as a hidden reference, and `key.txt` says which is which — for
//! the scorer, not the listener. `sheet.csv` is the sheet to fill in, one
//! score from nought to a hundred per file, the way BS.1534 has it.
//!
//! # What it does with a sheet
//!
//! `--score` joins the sheet to the key and prints each condition's mean,
//! the hidden reference's score — a listener who scores it under ninety did
//! not hear the reference, and BS.1534 leaves that listener out — and the
//! ranking. The ranking is the result, and `docs/clustering.md` is where it
//! goes, once, with the scenes it was made on.

use crate::cluster::{Audio, Track, programme_audio};
use hz_cluster::scene::{Loudness, PERCEPTION, Scene, Source};
use hz_cluster::smooth::{SMOOTHING, Smoother};
use hz_cluster::{Clusterer, Clustering, Object, Weighting};
use hz_core::{Error, Result};
use hz_io::container::pcm::PcmFormat;
use hz_io::container::wav::{RiffKind, WavFormat, WavWriter};
use hz_io::master::MasterSet;
use hz_render::{Layout, Panner};
use std::path::{Path, PathBuf};

pub struct Options {
    pub input: PathBuf,
    /// Where the stimuli, the key and the sheet go.
    pub out: PathBuf,
    /// Elements to fold into.
    pub elements: usize,
    /// Samples a block: the encoder's, by default.
    pub block: u64,
    /// How many seconds to render, from the start. The whole master when
    /// absent.
    pub seconds: Option<f64>,
    /// The conditions, each `loudness/weighting`.
    pub conditions: Vec<String>,
    /// What the blinding is shuffled by.
    pub seed: u64,
    /// A filled-in sheet to score instead of making stimuli.
    pub score: Option<PathBuf>,
}

/// The low-pass of BS.1534's anchor, in hertz.
const ANCHOR_HZ: f64 = 3500.0;

pub fn run(options: Options) -> Result<()> {
    if let Some(sheet) = &options.score {
        return score(&options.out, sheet);
    }

    let master = MasterSet::open(&options.input)?;
    let events = master.read_events(0)?;
    let programme = &master.config.presentations[0];
    let ids: Vec<u32> = programme.objects.iter().map(|object| object.id).collect();
    let audio = programme_audio(&master, 0)?;
    let sample_rate = events.sample_rate.unwrap_or(audio.rate);
    let frames = match options.seconds {
        Some(seconds) => ((seconds * f64::from(sample_rate)) as u64).min(audio.frames),
        None => audio.frames,
    };
    let step = options.block.max(1);
    let blocks = frames.div_ceil(step);

    // The bed, as the harness places it: pinned to its speaker's place, the
    // LFE passed through to the layout's own.
    let mut bed: Vec<(usize, [f64; 3])> = Vec::new();
    let mut lfe: Option<usize> = None;
    let mut channel_index = 0usize;
    for instance in &programme.bed_instances {
        for channel in &instance.channels {
            let here = channel_index;
            channel_index += 1;
            if hz_core::speakers::is_lfe_label(&channel.channel) {
                lfe = Some(here);
            } else if let Some(position) = hz_render::fold::bed_position(&channel.channel) {
                bed.push((here, position));
            }
        }
    }
    let first_object = audio.channels - ids.len();

    let layout = Layout::surround_7_1_4();
    let panner = Panner::new(&layout)?;
    let channels = layout.channels();
    let lfe_channel = layout
        .speakers
        .iter()
        .position(|label| hz_core::speakers::is_lfe_label(label))
        .unwrap_or(3);

    std::fs::create_dir_all(&options.out).map_err(|e| Error::io(&options.out, e))?;
    println!("{}", options.input.display());
    println!(
        "  rendering    {:.1} s on {}, {} blocks of {} samples, into {} elements",
        frames as f64 / f64::from(sample_rate),
        layout.name,
        blocks,
        step,
        options.elements
    );

    // What every stimulus shares: the walk through the master.
    let walk = Walk {
        events: &events,
        ids: &ids,
        audio: &audio,
        bed: &bed,
        lfe,
        first_object,
        frames,
        step,
        panner: &panner,
        channels,
        lfe_channel,
    };

    let mut stimuli: Vec<(String, Vec<Vec<f32>>)> = Vec::new();
    let reference = walk.reference();
    stimuli.push((
        "anchor".to_string(),
        anchored(&reference, f64::from(sample_rate)),
    ));
    stimuli.push(("reference".to_string(), reference));
    for condition in &options.conditions {
        let (loudness, weighting) = parse(condition, &options.input)?;
        let rendered = walk.folded(
            loudness,
            weighting,
            options.elements,
            f64::from(sample_rate),
        )?;
        stimuli.push((condition.clone(), rendered));
    }

    // Every stimulus scaled by one gain, so that nothing passes full scale
    // and every level stays what it was against the others: forty
    // broadband objects on twelve speakers add up past it, and a session
    // where the reference clips is a session about clipping.
    let loudest = stimuli
        .iter()
        .flat_map(|(_, rendered)| rendered.iter())
        .flat_map(|channel| channel.iter())
        .fold(0.0f32, |m, s| m.max(s.abs()));
    let headroom = 10f32.powf(-1.0 / 20.0);
    if loudest > headroom {
        let gain = headroom / loudest;
        for (_, rendered) in &mut stimuli {
            for channel in rendered.iter_mut() {
                for sample in channel.iter_mut() {
                    *sample *= gain;
                }
            }
        }
        println!(
            "  level        every stimulus by {:.1} dB, since the loudest reached {loudest:.2} of \
             full scale",
            20.0 * gain.log10()
        );
    }

    // Blinded: the files are named by a shuffle, and the key says which is
    // which.
    let mut order: Vec<usize> = (0..stimuli.len()).collect();
    let mut state = options.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..order.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    let mut key = String::new();
    let mut sheet = String::from("file,score\n");
    for (slot, index) in order.iter().enumerate() {
        let name = format!("stimulus_{}.wav", (b'a' + slot as u8) as char);
        let (condition, rendered) = &stimuli[*index];
        write_wav(&options.out.join(&name), rendered, sample_rate)?;
        let peak = rendered
            .iter()
            .flat_map(|channel| channel.iter())
            .fold(0.0f32, |m, s| m.max(s.abs()));
        key.push_str(&format!("{name},{condition}\n"));
        sheet.push_str(&format!("{name},\n"));
        println!("  wrote        {name}  ({condition}, peaking at {peak:.3})");
    }
    let key_path = options.out.join("key.txt");
    std::fs::write(&key_path, key).map_err(|e| Error::io(&key_path, e))?;
    let sheet_path = options.out.join("sheet.csv");
    std::fs::write(&sheet_path, sheet).map_err(|e| Error::io(&sheet_path, e))?;
    println!(
        "  key          {} — for the scorer, not the listener",
        options.out.join("key.txt").display()
    );
    println!(
        "  sheet        {} — one score from 0 to 100 per file, then \
         `cargo xtask bench {} --out {} --score {}`",
        options.out.join("sheet.csv").display(),
        options.input.display(),
        options.out.display(),
        options.out.join("sheet.csv").display()
    );
    Ok(())
}

/// The scene, walked block by block.
struct Walk<'a> {
    events: &'a hz_io::master::EventStream,
    ids: &'a [u32],
    audio: &'a Audio,
    bed: &'a [(usize, [f64; 3])],
    lfe: Option<usize>,
    first_object: usize,
    frames: u64,
    step: u64,
    panner: &'a Panner,
    channels: usize,
    lfe_channel: usize,
}

impl Walk<'_> {
    /// The tracks as they stand at the start of the walk, and a way to move
    /// them to a block.
    fn tracks(&self) -> (Vec<Track>, usize) {
        (self.ids.iter().map(|id| Track::new(*id)).collect(), 0)
    }

    /// Everything the mix said up to `at`.
    fn advance(&self, state: &mut [Track], next_event: &mut usize, at: u64) {
        while *next_event < self.events.events.len() {
            let event = &self.events.events[*next_event];
            if event.sample_pos.unwrap_or(0) > at {
                break;
            }
            if let Some(track) = event
                .id
                .and_then(|id| state.iter_mut().find(|track| track.id == id))
            {
                track.apply(event);
            }
            *next_event += 1;
        }
    }

    /// The bed and the objects for one block, as the fold's scene has them.
    fn sources(&self, state: &[Track]) -> Vec<Source> {
        let mut sources = Vec::with_capacity(self.bed.len() + state.len());
        for (_, position) in self.bed {
            sources.push(Source {
                position: *position,
                pinned: Some(*position),
                mode: hz_cluster::class::BED,
                ..Source::default()
            });
        }
        for track in state {
            sources.push(Source {
                position: track.position,
                gain: track.gain,
                size: track.size,
                importance: track.importance,
                pinned: None,
                mode: track.mode,
            });
        }
        sources
    }

    /// The source channel of each entry of [`Walk::sources`].
    fn channels_of(&self) -> Vec<usize> {
        let mut channels: Vec<usize> = self.bed.iter().map(|(channel, _)| *channel).collect();
        channels.extend((0..self.ids.len()).map(|index| self.first_object + index));
        channels
    }

    /// Pan `signal` into `out`, its gains ramping from `from` to `to` across
    /// the block, the way a decoder ramps a payload.
    fn pan(
        &self,
        signal: &[f32],
        gain: f64,
        from: &[f32],
        to: &[f32],
        out: &mut [Vec<f32>],
        at: usize,
    ) {
        let frames = signal.len();
        if frames == 0 || gain == 0.0 {
            return;
        }
        let step = 1.0 / frames as f64;
        for (channel, target) in out.iter_mut().enumerate() {
            let (start, end) = (f64::from(from[channel]), f64::from(to[channel]));
            if start == 0.0 && end == 0.0 {
                continue;
            }
            let slope = (end - start) * step;
            for (frame, sample) in signal.iter().enumerate() {
                let weight = start + slope * frame as f64;
                target[at + frame] += (f64::from(*sample) * gain * weight) as f32;
            }
        }
    }

    /// The scene as authored: every object at its own place.
    fn reference(&self) -> Vec<Vec<f32>> {
        let mut out = vec![vec![0.0f32; self.frames as usize]; self.channels];
        let (mut state, mut next_event) = self.tracks();
        let channels = self.channels_of();
        let mut previous: Vec<Vec<f32>> = Vec::new();
        let mut now = vec![0.0f32; self.channels];
        let mut signal: Vec<f32> = Vec::new();
        let mut at = 0u64;
        while at < self.frames {
            self.advance(&mut state, &mut next_event, at);
            let sources = self.sources(&state);
            let frames = self.step.min(self.frames - at) as usize;
            previous.resize_with(sources.len(), || vec![0.0f32; self.channels]);
            for (index, source) in sources.iter().enumerate() {
                self.panner.gains(source.position, source.size, &mut now);
                signal.clear();
                signal.extend(
                    self.audio
                        .samples_of(channels[index], at, frames as u64)
                        .map(|s| s as f32),
                );
                signal.resize(frames, 0.0);
                // The first block has nothing to ramp from and starts where it
                // ends up.
                if at == 0 {
                    previous[index].copy_from_slice(&now);
                }
                self.pan(
                    &signal,
                    source.gain,
                    &previous[index],
                    &now,
                    &mut out,
                    at as usize,
                );
                previous[index].copy_from_slice(&now);
            }
            self.lfe_through(&mut out, at, frames);
            at += self.step;
        }
        out
    }

    /// The LFE, passed through to the layout's own channel.
    fn lfe_through(&self, out: &mut [Vec<f32>], at: u64, frames: usize) {
        if let Some(channel) = self.lfe {
            for (frame, sample) in self
                .audio
                .samples_of(channel, at, frames as u64)
                .enumerate()
            {
                out[self.lfe_channel][at as usize + frame] += sample as f32;
            }
        }
    }

    /// The scene folded under a rule, its elements mixed as the encoder mixes
    /// them and panned where the fold put them.
    fn folded(
        &self,
        loudness: Loudness,
        weighting: Weighting,
        elements: usize,
        sample_rate: f64,
    ) -> Result<Vec<Vec<f32>>> {
        let mut out = vec![vec![0.0f32; self.frames as usize]; self.channels];
        let (mut state, mut next_event) = self.tracks();
        let channels = self.channels_of();
        let mut scene = Scene::weighed(sample_rate, loudness)?;
        let mut ahead = Scene::weighed(sample_rate, loudness)?;
        let mut smoother = Smoother::new(SMOOTHING);
        let mut clusterer = Clusterer::new(elements, weighting)?;
        let blocks = self.frames.div_ceil(self.step);
        let mut previous: Option<Clustering> = None;
        let mut placing: Vec<Object> = Vec::new();
        let mut signals: Vec<Vec<f32>> = Vec::new();
        let mut gains: Vec<f64> = Vec::new();
        let mut mixed: Vec<Vec<f32>> = vec![Vec::new(); elements];
        let mut from: Vec<Vec<f64>> = Vec::new();
        let mut gains_was: Vec<Vec<f32>> = vec![vec![0.0; self.channels]; elements];
        let mut gains_now = vec![0.0f32; self.channels];
        let mut block = 0u64;
        let mut at = 0u64;
        while at < self.frames {
            self.advance(&mut state, &mut next_event, at);
            let sources = self.sources(&state);
            let frames = self.step.min(self.frames - at) as usize;

            // The scene, and each object's signal, as the encoder has them.
            scene.start();
            signals.resize_with(sources.len(), Vec::new);
            gains.clear();
            for (index, source) in sources.iter().enumerate() {
                let signal = &mut signals[index];
                signal.clear();
                signal.extend(
                    self.audio
                        .samples_of(channels[index], at, frames as u64)
                        .map(|s| s as f32),
                );
                signal.resize(frames, 0.0);
                scene.push(source, signal.iter().map(|s| f64::from(*s)));
                gains.push(source.gain);
            }
            let objects = scene.finish();
            // The blocks ahead, recorded for the smoothing as the encoder
            // records them.
            while smoother.recorded() <= block + SMOOTHING.ahead as u64
                && smoother.recorded() < blocks
            {
                let later = smoother.recorded() * self.step;
                ahead.start();
                for (index, source) in sources.iter().enumerate() {
                    ahead.push(
                        source,
                        self.audio.samples_of(channels[index], later, self.step),
                    );
                }
                let read: Vec<f64> = ahead.finish().iter().map(|o| o.energy).collect();
                smoother.record(&read);
            }
            smoother.place(objects, &mut placing);
            let clustering = clusterer.cluster(&placing, previous.as_ref())?;

            from.clear();
            match &previous {
                Some(previous) if previous.weights.len() == clustering.weights.len() => {
                    from.extend(previous.weights.iter().cloned());
                }
                _ => from.extend(clustering.weights.iter().cloned()),
            }
            hz_cluster::mix::mix(
                &signals,
                &gains,
                &from,
                &clustering.weights,
                frames,
                &mut mixed,
            );

            // Each element panned where the fold put it, ramping from where
            // the last block left it.
            for (element, signal) in mixed.iter().enumerate() {
                self.panner
                    .gains(clustering.positions[element], 0.0, &mut gains_now);
                if at == 0 {
                    gains_was[element].copy_from_slice(&gains_now);
                }
                self.pan(
                    signal,
                    1.0,
                    &gains_was[element],
                    &gains_now,
                    &mut out,
                    at as usize,
                );
                gains_was[element].copy_from_slice(&gains_now);
            }
            self.lfe_through(&mut out, at, frames);
            previous = Some(clustering);
            block += 1;
            at += self.step;
        }
        Ok(out)
    }
}

/// The condition `loudness/weighting`.
fn parse(condition: &str, input: &Path) -> Result<(Loudness, Weighting)> {
    let (loudness, weighting) = condition.split_once('/').ok_or_else(|| {
        Error::unsupported(
            input,
            format!("`{condition}`; a condition is `loudness/weighting`, say `flat/fitted`"),
        )
    })?;
    let loudness = match loudness {
        "flat" => Loudness::Flat,
        "kweighted" => Loudness::KWeighted,
        "perceptual" => Loudness::Perceptual(PERCEPTION),
        other => {
            return Err(Error::unsupported(
                input,
                format!("`{other}`; the loudnesses are flat, kweighted and perceptual"),
            ));
        }
    };
    let weighting = match weighting {
        "fitted" => Weighting::Fitted,
        "nearest" => Weighting::Nearest,
        "spread" => Weighting::Spread,
        other => {
            return Err(Error::unsupported(
                input,
                format!("`{other}`; the weightings are fitted, nearest and spread"),
            ));
        }
    };
    Ok((loudness, weighting))
}

/// The reference through the anchor's low-pass, twice over for the slope.
fn anchored(reference: &[Vec<f32>], sample_rate: f64) -> Vec<Vec<f32>> {
    reference
        .iter()
        .map(|channel| {
            let mut first = LowPass::new(ANCHOR_HZ, sample_rate);
            let mut second = LowPass::new(ANCHOR_HZ, sample_rate);
            channel
                .iter()
                .map(|s| second.step(first.step(f64::from(*s))) as f32)
                .collect()
        })
        .collect()
}

/// A second-order low-pass, from the audio EQ cookbook.
struct LowPass {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    s1: f64,
    s2: f64,
}

impl LowPass {
    fn new(hz: f64, rate: f64) -> Self {
        let w0 = std::f64::consts::TAU * hz / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - cos) / 2.0 / a0,
            b1: (1.0 - cos) / a0,
            b2: (1.0 - cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Twenty-four bits, interleaved.
fn write_wav(path: &Path, channels: &[Vec<f32>], sample_rate: u32) -> Result<()> {
    let count = channels.len() as u32;
    let frames = channels.first().map_or(0, Vec::len);
    let mut writer = WavWriter::create(
        path,
        RiffKind::Riff,
        WavFormat::plain(PcmFormat::wav(f64::from(sample_rate), count, 24)),
    )?;
    const BLOCK: usize = 4096;
    let mut block = vec![0i32; BLOCK * count as usize];
    let mut at = 0usize;
    while at < frames {
        let take = BLOCK.min(frames - at);
        for frame in 0..take {
            for (channel, samples) in channels.iter().enumerate() {
                let value = (f64::from(samples[at + frame]) * 8_388_607.0)
                    .round()
                    .clamp(-8_388_608.0, 8_388_607.0);
                block[frame * count as usize + channel] = value as i32;
            }
        }
        writer.write_frames(&block[..take * count as usize])?;
        at += take;
    }
    writer.finish()
}

/// Join a filled-in sheet to the key and rank the conditions.
fn score(out: &Path, sheet: &Path) -> Result<()> {
    let key_path = out.join("key.txt");
    let key = std::fs::read_to_string(&key_path).map_err(|e| Error::io(&key_path, e))?;
    let filled = std::fs::read_to_string(sheet).map_err(|e| Error::io(sheet, e))?;
    let conditions: Vec<(String, String)> = key
        .lines()
        .filter_map(|line| line.split_once(','))
        .map(|(file, condition)| (file.to_string(), condition.to_string()))
        .collect();
    let mut scores: Vec<(String, f64)> = Vec::new();
    for line in filled.lines().skip(1) {
        let Some((file, score)) = line.split_once(',') else {
            continue;
        };
        let Ok(score) = score.trim().parse::<f64>() else {
            return Err(Error::malformed(
                sheet,
                format!("`{line}` carries no score"),
            ));
        };
        let Some((_, condition)) = conditions.iter().find(|(known, _)| known == file) else {
            return Err(Error::malformed(
                sheet,
                format!("`{file}` is not in the key"),
            ));
        };
        scores.push((condition.clone(), score));
    }
    if scores.is_empty() {
        return Err(Error::malformed(sheet, "no scores filled in"));
    }
    // BS.1534: a listener who does not hear the hidden reference is left out.
    if let Some((_, reference)) = scores
        .iter()
        .find(|(condition, _)| condition == "reference")
        && *reference < 90.0
    {
        println!(
            "  note         the hidden reference scored {reference:.0}; BS.1534 leaves a listener \
             who scores it under 90 out"
        );
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    println!("  ranking");
    for (rank, (condition, score)) in scores.iter().enumerate() {
        println!("    {:>2}. {:<24} {score:>5.1}", rank + 1, condition);
    }
    Ok(())
}
