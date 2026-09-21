//! Write a small synthetic master file set.
//!
//! The one real master set on this machine declares sixteen elements over a
//! twenty-one channel audio component, so the converter refuses it — correctly,
//! since the mapping cannot be guessed. That leaves nothing to exercise the
//! conversion end to end, which is what this makes: a set whose config, event
//! stream and audio agree with each other, small enough to inspect by eye.
//!
//! # Three kinds of scene
//!
//! **Tones**: a different pure tone per channel, so a channel that ends up in
//! the wrong place is audible rather than merely wrong. It is also the worst
//! possible scene on which to settle anything perceptual — forty tones over
//! five octaves, so any frequency weighting moves their ranking by tens of
//! decibels where broadband content would move it by two or three.
//!
//! **Broadband**: what a mix is made of. One object is a voice — noise shaped
//! to the band a voice occupies, with a syllabic rise and fall — one is a
//! rumble, and the rest are effects: noise centred at places spread over the
//! audible range, a third of them given a width. Their levels are spread over
//! sixteen decibels so that no two rules rank them the same way for free.
//! The trajectories are the same as the tone scene's, so the two differ in
//! what the objects carry and in nothing else.
//!
//! **Bursts**: the broadband scene with its effects gated — on for a while,
//! off for a while, the whiles drawn between eighty milliseconds and four
//! tenths of a second, which is three to fifteen blocks of the fold. It is
//! the scene for anything about onsets and releases: the other two turn
//! smoothly and nothing in them ever starts or stops.

use hz_analysis::bands::{erb_rate, erb_rate_frequency};
use hz_core::{Error, Result};
use hz_io::container::caf::CafWriter;
use hz_io::container::pcm::PcmFormat;
use hz_io::master::{Config, EventStream};
use std::f64::consts::TAU;
use std::path::{Path, PathBuf};

pub struct Options {
    /// Path of the `.atmos` config; the other two components sit beside it.
    pub output: PathBuf,
    pub objects: u32,
    pub seconds: u32,
    /// Which bed to write: `left`, `lfe` or `7.1.2`. See [`bed_of`].
    pub bed: String,
    /// What the objects carry: `tones`, `broadband` or `bursts`.
    pub scene: String,
    /// Every Nth object is snapped to the nearest speaker, and every Mth is
    /// confined to the front of the room: each makes a class of its own, and
    /// an object that is both makes a third. Nought does neither.
    pub snap_every: u32,
    pub zone_every: u32,
    /// The first N objects carry one and the same tone at −4 dBFS, so that
    /// an element carrying two of them passes full scale.
    pub coherent: u32,
}

const SAMPLE_RATE: u32 = 48_000;
/// Bed channels, in the order their audio channels appear, with the ids the
/// master's bed configuration names them by.
const BED: [(&str, u32); 2] = [("L", 0), ("LFE", 3)];
const LFE_ONLY: [(&str, u32); 1] = [("LFE", 3)];
/// The bed an authored master has: seven around, the LFE, and two overhead.
const BED_7_1_2: [(&str, u32); 10] = [
    ("L", 0),
    ("R", 1),
    ("C", 2),
    ("LFE", 3),
    ("Lss", 4),
    ("Rss", 5),
    ("Lrs", 6),
    ("Rrs", 7),
    ("Lts", 10),
    ("Rts", 11),
];

/// The bed a name asks for.
///
/// `lfe` is the shape the encoder carried before beds were folded, and what
/// a scene meant to exercise the fold alone still wants; `7.1.2` is what an
/// authored master brings, and what the fold of a bed is measured on.
pub fn bed_of(name: &str) -> Option<&'static [(&'static str, u32)]> {
    Some(match name {
        "left" => &BED,
        "lfe" => &LFE_ONLY,
        "7.1.2" => &BED_7_1_2,
        _ => return None,
    })
}

pub fn run(options: Options) -> Result<()> {
    let Some(bed) = bed_of(&options.bed) else {
        return Err(Error::unsupported(
            &options.output,
            format!("`{}`; the beds are left, lfe and 7.1.2", options.bed),
        ));
    };
    let stem = options
        .output
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".atmos"))
        .ok_or_else(|| Error::malformed(&options.output, "the config ends in `.atmos`"))?
        .to_string();
    let directory = options.output.parent().unwrap_or(Path::new("."));
    let metadata_name = format!("{stem}.atmos.metadata");
    let audio_name = format!("{stem}.atmos.audio");

    let channels = bed.len() as u32 + options.objects;
    let frames = u64::from(options.seconds) * u64::from(SAMPLE_RATE);
    let synth = match options.scene.as_str() {
        "tones" => Synth::Tones,
        "broadband" => {
            Synth::broadband(bed.len(), options.objects as usize, frames as usize, false)
        }
        "bursts" => Synth::broadband(bed.len(), options.objects as usize, frames as usize, true),
        other => {
            return Err(Error::unsupported(
                &options.output,
                format!("`{other}`; the scenes are tones, broadband and bursts"),
            ));
        }
    };
    let synth = Synth::Coherent {
        first: options.coherent as usize,
        bed: bed.len(),
        rest: Box::new(synth),
    };

    // --- config ------------------------------------------------------------
    let mut config = String::new();
    config.push_str("language: eng\nversion: 0.5.1\npresentations:\n");
    config.push_str("  - type: home\n    simplified: false\n");
    config.push_str(&format!("    metadata: {metadata_name}\n"));
    config.push_str(&format!("    audio: {audio_name}\n"));
    config.push_str("    offset: 0.0\n    fps: 24\n");
    config.push_str(&format!(
        "    scBedConfiguration: [{}]\n",
        bed.iter()
            .map(|(_, id)| id.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    config.push_str("    creationTool: harlettizer-xtask\n    creationToolVersion: 0.0.0\n");
    config.push_str("    bedInstances:\n      - channels:\n");
    for (name, id) in bed {
        config.push_str(&format!(
            "          - channel: {name}\n            ID: {id}\n"
        ));
    }
    config.push_str("    objects:\n");
    for index in 0..options.objects {
        config.push_str(&format!("      - ID: {}\n", 10 + index));
    }

    // Parsing it back before writing it proves the fixture is a master set and
    // not merely text that looks like one.
    let parsed = Config::parse(&config).map_err(|e| Error::malformed(&options.output, e))?;
    parsed.write(&options.output)?;

    // --- events ------------------------------------------------------------
    // One update per object per second, on a circle, so positions are easy to
    // check by eye and every axis gets exercised.
    let mut events = String::from("sampleRate: 48000\nevents:\n");
    for (_, id) in bed {
        events.push_str(&format!(
            "  - ID: {id}\n    samplePos: 0\n    active: true\n    importance: 1.0\n    \
             gain: 0\n    rampLength: 0\n"
        ));
    }
    for second in 0..options.seconds {
        let sample = u64::from(second) * u64::from(SAMPLE_RATE);
        for index in 0..options.objects {
            let angle = TAU * f64::from(index) / f64::from(options.objects)
                + TAU * f64::from(second) / f64::from(options.seconds.max(1));
            let x = round(angle.cos());
            let y = round(angle.sin());
            let z = round(f64::from(second) / f64::from(options.seconds.max(1)));
            let size = synth.size(index as usize);
            let snapped = options.snap_every > 0 && index % options.snap_every == 0;
            let zoned = options.zone_every > 0 && index % options.zone_every == 0;
            events.push_str(&format!(
                "  - ID: {}\n    samplePos: {sample}\n    active: true\n    \
                 pos: [{x}, {y}, {z}]\n    size: {size:.1}\n    importance: 1.0\n    \
                 gain: 0\n    rampLength: 1152\n{}{}",
                10 + index,
                if snapped { "    snap: true\n" } else { "" },
                if zoned { "    zones: no back\n" } else { "" },
            ));
        }
    }
    let parsed_events =
        EventStream::parse(&events).map_err(|e| Error::malformed(&options.output, e))?;
    parsed_events.write(&directory.join(&metadata_name))?;

    // --- audio -------------------------------------------------------------
    let audio_path = directory.join(&audio_name);
    let mut writer = CafWriter::create(
        &audio_path,
        PcmFormat::master(f64::from(SAMPLE_RATE), channels),
    )?;

    const BLOCK: usize = 4096;
    let mut block = vec![0i32; BLOCK * channels as usize];
    let mut written = 0u64;
    while written < frames {
        let count = BLOCK.min((frames - written) as usize);
        for frame in 0..count {
            let at = written as usize + frame;
            for channel in 0..channels as usize {
                let value = synth.sample(channel, at);
                block[frame * channels as usize + channel] = (value * 8_388_607.0) as i32;
            }
        }
        writer.write_frames(&block[..count * channels as usize])?;
        written += count as u64;
    }
    writer.finish()?;

    println!(
        "wrote {} ({} bed channels, {} objects, {} channels, {} frames, {})",
        options.output.display(),
        bed.len(),
        options.objects,
        channels,
        frames,
        options.scene
    );
    Ok(())
}

/// Keep the fixture readable: coordinates to three decimals.
fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// What each channel carries.
enum Synth {
    /// A different tone per channel.
    Tones,
    /// The first objects carry one and the same loud tone, and the rest is
    /// whatever `rest` says — the scene a fold has to keep inside the
    /// domain, since two of them in one element pass full scale.
    Coherent {
        first: usize,
        bed: usize,
        rest: Box<Synth>,
    },
    /// Shaped noise per object, made in advance; the bed keeps its tones.
    Broadband {
        bed: usize,
        signals: Vec<Vec<f64>>,
        sizes: Vec<f64>,
    },
}

impl Synth {
    /// The channel's sample at frame `at`.
    fn sample(&self, channel: usize, at: usize) -> f64 {
        match self {
            Self::Coherent { first, bed, rest } => match channel.checked_sub(*bed) {
                Some(object) if object < *first => {
                    let t = at as f64 / f64::from(SAMPLE_RATE);
                    (TAU * 500.0 * t).sin() * 10f64.powf(-4.0 / 20.0)
                }
                _ => rest.sample(channel, at),
            },
            Self::Tones => tone(channel, at),
            Self::Broadband { bed, signals, .. } => match channel.checked_sub(*bed) {
                Some(object) => signals[object][at],
                None => tone(channel, at),
            },
        }
    }

    /// The object's width, which the event stream states.
    fn size(&self, object: usize) -> f64 {
        match self {
            Self::Coherent { rest, .. } => rest.size(object),
            Self::Tones => 0.0,
            Self::Broadband { sizes, .. } => sizes[object],
        }
    }

    /// The broadband scene: a voice, a rumble, and effects — gated on and
    /// off when `bursts` is asked for.
    fn broadband(bed: usize, objects: usize, frames: usize, bursts: bool) -> Self {
        let rate = f64::from(SAMPLE_RATE);
        let effects = objects.saturating_sub(2);
        let mut signals = Vec::with_capacity(objects);
        let mut sizes = Vec::with_capacity(objects);
        for object in 0..objects {
            let mut noise = Noise::new(object as u64 + 1);
            let mut signal: Vec<f64> = (0..frames).map(|_| noise.next()).collect();
            let (level_db, size) = match object {
                // A voice: the band a voice occupies, rising and falling four
                // times a second, at the level dialogue is mixed to.
                0 => {
                    let mut high = Biquad::high_pass(300.0, 0.707, rate);
                    let mut low = Biquad::low_pass(3400.0, 0.707, rate);
                    for (n, sample) in signal.iter_mut().enumerate() {
                        let shaped = low.step(high.step(*sample));
                        let syllable = 0.5 - 0.5 * (TAU * 4.0 * n as f64 / rate).cos();
                        *sample = shaped * syllable;
                    }
                    (-20.0, 0.0)
                }
                // A rumble: nothing above sixty hertz, ten decibels louder
                // than the voice — which a meter takes for the loudest thing
                // in the scene.
                1 => {
                    let mut first = Biquad::low_pass(60.0, 0.707, rate);
                    let mut second = Biquad::low_pass(60.0, 0.707, rate);
                    for sample in &mut signal {
                        *sample = second.step(first.step(*sample));
                    }
                    (-10.0, 0.0)
                }
                // An effect: noise around a centre spread evenly on the
                // ERB-rate scale between 150 Hz and 12 kHz, broad, at a level
                // spread over sixteen decibels, and every third one wide.
                _ => {
                    let effect = object - 2;
                    let low = erb_rate(150.0);
                    let high = erb_rate(12_000.0);
                    let centre = erb_rate_frequency(
                        low + (effect as f64 + 0.5) / effects.max(1) as f64 * (high - low),
                    );
                    let mut band = Biquad::band_pass(centre, 0.7, rate);
                    for sample in &mut signal {
                        *sample = band.step(*sample);
                    }
                    let spread = (effect as f64 * 0.618_033_988_75).fract();
                    let size = if effect % 3 == 2 { 0.3 } else { 0.0 };
                    (-14.0 - 16.0 * spread, size)
                }
            };
            // To the level asked for, as a mean square over the whole signal
            // — before any gate, so that a burst is at that level while it is
            // on.
            let power = signal.iter().map(|s| s * s).sum::<f64>() / frames.max(1) as f64;
            if power > 0.0 {
                let scale = 10f64.powf(level_db / 20.0) / power.sqrt();
                for sample in &mut signal {
                    *sample *= scale;
                }
            }
            if bursts && object >= 2 {
                gate(&mut signal, &mut noise, rate);
            }
            signals.push(signal);
            sizes.push(size);
        }
        Self::Broadband {
            bed,
            signals,
            sizes,
        }
    }
}

/// Switch a signal on and off for stretches drawn between eighty milliseconds
/// and four tenths of a second, with a two-millisecond ramp at each edge so
/// that the edge itself is not a click's worth of energy.
fn gate(signal: &mut [f64], noise: &mut Noise, rate: f64) {
    let ramp = (0.002 * rate) as usize;
    let stretch = |noise: &mut Noise| ((0.08 + 0.32 * (noise.next() + 1.0) / 2.0) * rate) as usize;
    let mut at = 0usize;
    // Start on or off as the noise says, so the scene does not open on
    // forty attacks at once.
    let mut on = noise.next() > 0.0;
    while at < signal.len() {
        let length = stretch(noise).max(ramp * 2);
        let end = (at + length).min(signal.len());
        if !on {
            for (n, sample) in signal[at..end].iter_mut().enumerate() {
                // The ramps: down over the first samples of an off stretch,
                // and up over its last, so that every edge is eased.
                let into = n.min(end - at - 1 - n);
                let eased = (ramp - into.min(ramp)) as f64 / ramp as f64;
                *sample *= eased;
            }
        }
        on = !on;
        at = end;
    }
}

/// A different tone per channel.
fn tone(channel: usize, at: usize) -> f64 {
    let t = at as f64 / f64::from(SAMPLE_RATE);
    let hz = 220.0 * (channel as f64 + 1.0);
    (TAU * hz * t).sin() * 0.25
}

/// A deterministic noise source, so that a scene is the same scene every
/// time it is made.
struct Noise(u64);

impl Noise {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// Uniform on `[-1, 1)`.
    fn next(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        bits as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// A transposed direct-form-II biquad, designed from the audio EQ cookbook.
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    s1: f64,
    s2: f64,
}

impl Biquad {
    fn design(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    fn low_pass(hz: f64, q: f64, rate: f64) -> Self {
        let w0 = TAU * hz / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        Self::design(
            (1.0 - cos) / 2.0,
            1.0 - cos,
            (1.0 - cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        )
    }

    fn high_pass(hz: f64, q: f64, rate: f64) -> Self {
        let w0 = TAU * hz / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        Self::design(
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        )
    }

    /// Constant peak gain.
    fn band_pass(hz: f64, q: f64, rate: f64) -> Self {
        let w0 = TAU * hz / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        Self::design(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }
}
