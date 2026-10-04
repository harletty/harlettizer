//! `harlettizer iamf`: a programme written as an IAMF sequence.
//!
//! Two ways. By default the objects do not survive as objects: IAMF before
//! v2.0 has none, and v2.0 is not yet what a browser or a television decodes.
//! So the master is rendered — its bed channels routed to their speakers, its
//! objects panned on the room's cube ([`hz_render::room`]) — and the 7.1.4 bed
//! is what is coded. With `--objects` the objects are carried as IAMF v2.0
//! objects instead; see [`crate::iamf_objects`].
//!
//! Either way the loudness a decoder normalises by is measured on the two
//! layouts the mix presentation states: 7.1.4, and the stereo pair a decoder
//! folds it to.

use crate::encode::Progress;
use crate::source::{Source, keyframes_of, tracks};
use hz_analysis::loudness::{ChannelWeight, Meter};
use hz_analysis::truepeak::TruePeak;
use hz_core::{Error, Result, speakers};
use hz_iamf::{Codec, Element, Headphones, Loudness, PositionKind, Writer};
use hz_io::adm::model::TypeDefinition;
use hz_io::container::SampleFormat;
use hz_render::{Keyframe, Layout, Mixdown};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

/// What `iamf` was asked to do.
pub struct Config {
    pub input: PathBuf,
    pub out: PathBuf,
    pub frames: Option<u64>,
    pub mono_prefix: Option<PathBuf>,
    pub codec: Codec,
    pub bits: u32,
    pub frame: usize,
    pub headphones: Headphones,
    pub progress: bool,
    /// Carry the objects as IAMF v2.0 objects, positions coded this way,
    /// rather than rendering them to a bed.
    pub objects: Option<PositionKind>,
    /// With `objects`: the most object elements to use, folding the master's
    /// objects into them when it has more.
    pub elements: Option<usize>,
    /// With `objects`: keep the master's elements and pan its last this-many
    /// objects onto them — `encode --overlay`, written as IAMF. See
    /// [`crate::overlay`].
    pub overlay: Option<usize>,
    /// With `objects`: render the master's last this-many objects into the
    /// bed element, and carry the rest as objects.
    pub voices_to_bed: Option<usize>,
    /// What shapes an overlay: the same options, with the same defaults, as
    /// `encode --overlay`.
    pub overlaying: Overlaying,
}

/// What shapes `--overlay` beyond its sources, as `encode` takes it.
pub struct Overlaying {
    /// How far a bed-only fit may land from a source before the moving
    /// elements are let in; `None` declines the preference.
    pub beds_first: Option<f64>,
    /// What was typed for it, so that a negative reach is refused rather than
    /// read as the preference declined.
    pub beds_asked: f64,
    pub bounds: crate::overlay::Bounds,
    /// Whether a source may take a spare element of its own.
    pub spare: bool,
    /// Where the block-by-block account goes.
    pub report: Option<PathBuf>,
    /// What the mixed elements are rounded to, in bits.
    pub fold_depth: u32,
    /// What sets the audibility floor: see `hz_cluster::floor`.
    pub dialnorm: f64,
    /// Whether a limiter keeps the mixed elements inside the codec's domain.
    pub limit: bool,
    /// How a block's power is weighed.
    pub weighing: hz_cluster::scene::Loudness,
}

/// Samples a channel per temporal unit, unless asked otherwise: a FLAC block
/// size that compresses well and stays inside the streamable subset, about
/// 85 ms at 48 kHz.
pub const FRAME: usize = 4096;

/// An Opus packet's samples unless asked otherwise: 20 ms, what libopus is
/// tuned at and what YouTube's IAMF streams use.
pub const OPUS_FRAME: usize = 960;

/// Opus kilobits a second for each channel unless asked otherwise.
///
/// YouTube's IAMF streams carry about 46 a channel. This is a step above,
/// for a programme that is an encode's output rather than a stream's: a
/// 7.1.4 at about 720 kbit/s, between a streaming service's immersive tier
/// and a disc's.
pub const OPUS_BITRATE: u32 = 64;

/// An object of the master: its input channel, its updates, and the next one
/// to act on.
struct Moving {
    source: usize,
    keyframes: Vec<Keyframe>,
    next: usize,
}

/// The output, open, with its descriptors written.
pub(crate) type Output = Writer<BufWriter<File>>;

/// Whether `out` names a Matroska file, which the sequence is then the one
/// track of.
fn is_matroska(out: &std::path::Path) -> bool {
    out.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mka") || e.eq_ignore_ascii_case("mkv"))
}

/// Open `config.out` and start a sequence of these elements there.
pub(crate) fn open(config: &Config, sample_rate: u32, elements: Vec<Element>) -> Result<Output> {
    let out_file = File::create(&config.out).map_err(|e| Error::io(&config.out, e))?;
    let iamf_config = hz_iamf::Config {
        elements,
        codec: config.codec,
        sample_rate,
        bits: config.bits,
        frame: config.frame,
        headphones: config.headphones,
    };
    let out = BufWriter::new(out_file);
    let writer = if is_matroska(&config.out) {
        Writer::matroska(out, iamf_config, "harlettizer")
    } else {
        Writer::new(out, iamf_config)
    };
    writer.map_err(|e| match e {
        hz_iamf::Error::Io(e) => Error::io(&config.out, e),
        hz_iamf::Error::Unsupported(what) => Error::unsupported(&config.out, what),
    })
}

/// Read up to `want` frames into `raw`, as many reads as it takes: how many
/// landed, fewer only at the end of the input.
pub(crate) fn fill(
    source: &mut Source,
    raw: &mut [i32],
    want: usize,
    stride: usize,
) -> Result<usize> {
    let mut got = 0;
    while got < want {
        let landed = source.read(&mut raw[got * stride..want * stride])?;
        if landed == 0 {
            break;
        }
        got += landed;
    }
    Ok(got)
}

/// The loudness of what a decoder hands back, measured as it goes: a 7.1.4
/// bed, and the stereo pair it folds to the decoder's way.
pub(crate) struct Measure {
    meter: Meter,
    stereo_meter: Meter,
    true_peak: TruePeak,
    stereo_true_peak: TruePeak,
    peak: f64,
    stereo_peak: f64,
    stereo: Vec<f32>,
}

impl Measure {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let weights: Vec<ChannelWeight> = Layout::surround_7_1_4()
            .speakers
            .iter()
            .map(|label| ChannelWeight::for_speaker_label(label))
            .collect();
        Self {
            meter: Meter::new(sample_rate, &weights),
            stereo_meter: Meter::new(sample_rate, &[ChannelWeight::Unity; 2]),
            true_peak: TruePeak::new(weights.len()),
            stereo_true_peak: TruePeak::new(2),
            peak: 0.0,
            stereo_peak: 0.0,
            stereo: Vec::new(),
        }
    }

    /// Interleaved 7.1.4 frames, full scale ±1.
    pub(crate) fn push(&mut self, bed: &[f32]) {
        let coded = hz_iamf::layout::SEVEN_ONE_FOUR;
        let width = coded.channels();
        self.stereo.clear();
        for frame in bed.chunks_exact(width) {
            let (mut left, mut right) = (0.0f64, 0.0f64);
            for (&x, gains) in frame.iter().zip(coded.stereo) {
                self.peak = self.peak.max(f64::from(x).abs());
                left += f64::from(x) * gains[0];
                right += f64::from(x) * gains[1];
            }
            self.stereo_peak = self.stereo_peak.max(left.abs()).max(right.abs());
            self.stereo.push(left as f32);
            self.stereo.push(right as f32);
        }
        self.meter.push(bed);
        self.true_peak.push(bed);
        self.stereo_meter.push(&self.stereo);
        self.stereo_true_peak.push(&self.stereo);
    }

    /// Stereo, then 7.1.4: the order the mix presentation states them in.
    pub(crate) fn finish(mut self) -> [Loudness; 2] {
        self.meter.flush();
        self.stereo_meter.flush();
        [
            Loudness {
                integrated: self.stereo_meter.integrated(),
                digital_peak: decibels(self.stereo_peak),
                true_peak: self.stereo_true_peak.peak_db(),
            },
            Loudness {
                integrated: self.meter.integrated(),
                digital_peak: decibels(self.peak),
                true_peak: self.true_peak.peak_db(),
            },
        ]
    }
}

/// How the summary names the coding.
pub(crate) fn coding(config: &Config) -> String {
    match config.codec {
        Codec::Flac => format!("FLAC {}-bit", config.bits),
        Codec::Lpcm => format!("LPCM {}-bit", config.bits),
        Codec::Opus { bitrate } => format!("Opus at {} kbit/s a channel", bitrate / 1000),
    }
}

/// The summary's loudness and peak lines.
pub(crate) fn report_loudness([folded, native]: [Loudness; 2]) {
    println!(
        "  loudness     {} on 7.1.4, {} on stereo",
        lkfs(native.integrated),
        lkfs(folded.integrated)
    );
    println!(
        "  peaks        {:.1} dBFS sampled, {:.1} dBTP on 7.1.4; {:.1} dBFS, {:.1} dBTP on stereo",
        native.digital_peak, native.true_peak, folded.digital_peak, folded.true_peak
    );
}

/// The summary's last line, and the number of bytes it names.
pub(crate) fn report_written(config: &Config, samples: u64, sample_rate: u32) -> Result<()> {
    let bytes = std::fs::metadata(&config.out)
        .map_err(|e| Error::io(&config.out, e))?
        .len();
    let seconds = samples as f64 / f64::from(sample_rate);
    println!(
        "  wrote        {}, {bytes} bytes, {:.0} kbit/s",
        config.out.display(),
        bytes as f64 * 8.0 / seconds / 1000.0
    );
    Ok(())
}

pub fn run(config: Config) -> Result<()> {
    if config.objects.is_some() {
        return crate::iamf_objects::run(config);
    }
    let path = config.input.as_path();
    let mut source = Source::open(path, config.mono_prefix.as_deref())?;
    let format = *source.pcm_format();
    if format.sample_format == SampleFormat::Float {
        return Err(Error::unsupported(
            path,
            "floating-point audio; the master formats this reads are integer PCM",
        ));
    }
    let input_scale = 1.0 / f64::from(1u32 << (format.bits_per_channel - 1));
    let sample_rate = source.sample_rate();
    let stride = source.channels();

    let layout = Layout::surround_7_1_4();
    let coded = hz_iamf::layout::SEVEN_ONE_FOUR;
    debug_assert_eq!(layout.speakers, coded.labels);
    let width = layout.channels();
    let lfe = layout.index_of("LFE1").expect("7.1.4 has an LFE");

    // Every track to a source of the mixdown.
    let mut mixdown = Mixdown::new(&layout)?;
    let mut moving = Vec::new();
    let (mut routed, mut placed, mut objects) = (0usize, 0usize, 0usize);
    let described = source.adm();
    for track in tracks(path, described, stride)? {
        match track.format.type_definition {
            TypeDefinition::DirectSpeakers => {
                let label = track.speaker_label();
                // Either spelling: an ADM label, or a master's channel name.
                let label = speakers::by_master_name(label).map_or(label, |s| s.label);
                if speakers::is_lfe_label(label) {
                    mixdown.speaker(track.source_channel, lfe);
                    routed += 1;
                } else if let Some(channel) = layout.index_of(label) {
                    mixdown.speaker(track.source_channel, channel);
                    routed += 1;
                } else {
                    // A bed channel 7.1.4 does not have — a wide, a top
                    // side — is an object that never moves from its speaker.
                    let position = hz_render::fold::bed_position(label).ok_or_else(|| {
                        Error::unsupported(
                            path,
                            format!(
                                "track {} is the bed channel `{label}`, which has no place in \
                                 the room to render it at",
                                track.number
                            ),
                        )
                    })?;
                    let index = mixdown.object(track.source_channel);
                    mixdown.update(
                        index,
                        &Keyframe {
                            position,
                            ..Keyframe::default()
                        },
                    );
                    placed += 1;
                }
            }
            TypeDefinition::Objects => {
                let index = mixdown.object(track.source_channel);
                let keyframes = keyframes_of(track.format, described.sample_rate);
                // Where and as loud as its first update says from the first
                // sample, as `encode` and `--objects` carry it, rather than
                // silent until that update arrives.
                if let Some(first) = keyframes.iter().min_by_key(|k| k.sample_pos) {
                    mixdown.update(
                        index,
                        &Keyframe {
                            ramp_samples: 0,
                            ..*first
                        },
                    );
                }
                moving.push(Moving {
                    source: index,
                    keyframes,
                    next: 0,
                });
                objects += 1;
            }
            other => eprintln!(
                "note: track {} is `{other}` audio, which a bed render does not take; left out",
                track.number
            ),
        }
    }
    if routed + placed + objects == 0 {
        return Err(Error::unsupported(path, "nothing in it to render"));
    }

    let mut writer = open(&config, sample_rate, vec![Element::Channels(coded)])?;

    let frame = config.frame;
    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    let mut progress = Progress::new(config.progress, total);

    let mut raw = vec![0i32; frame * stride];
    let mut input = vec![0f32; frame * stride];
    let mut mixed = vec![0f64; frame * width];
    let mut quantised = vec![0i32; frame * width];
    let mut measured = vec![0f32; frame * width];
    let mut measure = Measure::new(sample_rate);

    let full_scale = f64::from(1u32 << (config.bits - 1));
    let (low, high) = (-full_scale, full_scale - 1.0);
    let mut clipped = 0u64;
    let mut mix_peak = 0.0f64;

    let mut at = 0u64;
    while at < total {
        let want = (total - at).min(frame as u64) as usize;
        let got = fill(&mut source, &mut raw, want, stride)?;
        if got == 0 {
            break;
        }
        for (x, &r) in input[..got * stride].iter_mut().zip(&raw[..got * stride]) {
            *x = (f64::from(r) * input_scale) as f32;
        }

        // The block, split at every update the master makes inside it so
        // that each lands on its own sample.
        mixed[..got * width].fill(0.0);
        let mut done = 0;
        while done < got {
            let now = at + done as u64;
            let mut next_update = u64::MAX;
            for object in &mut moving {
                while let Some(state) = object.keyframes.get(object.next) {
                    if state.sample_pos > now {
                        next_update = next_update.min(state.sample_pos);
                        break;
                    }
                    mixdown.update(object.source, state);
                    object.next += 1;
                }
            }
            let end = next_update.saturating_sub(at).min(got as u64) as usize;
            mixdown.render(
                &input[done * stride..got * stride],
                stride,
                end - done,
                &mut mixed[done * width..got * width],
            );
            done = end;
        }

        // Into the codec's integers, and measured as a decoder will hand them
        // back: the bed as it is, and folded to stereo the decoder's way.
        for (q, &m) in quantised[..got * width]
            .iter_mut()
            .zip(&mixed[..got * width])
        {
            let scaled = (m * full_scale).round();
            mix_peak = mix_peak.max(m.abs());
            if scaled < low || scaled > high {
                clipped += 1;
            }
            *q = scaled.clamp(low, high) as i32;
        }
        for (f, &q) in measured[..got * width]
            .iter_mut()
            .zip(&quantised[..got * width])
        {
            *f = (f64::from(q) / full_scale) as f32;
        }
        measure.push(&measured[..got * width]);

        writer
            .push(&quantised[..got * width], &[])
            .map_err(|e| Error::io(&config.out, e))?;
        at += got as u64;
        if let Some(progress) = progress.as_mut() {
            progress.at(at);
        }
        if got < frame {
            break;
        }
    }
    if at == 0 {
        return Err(Error::unsupported(path, "no audio to encode"));
    }

    let loudness = measure.finish();
    let units = writer.units();
    writer
        .finish(loudness)
        .map_err(|e| Error::io(&config.out, e))?;

    let mut parts = Vec::new();
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if objects > 0 {
        parts.push(plural(objects, "object", "objects"));
    }
    if routed > 0 {
        parts.push(format!(
            "{} routed to {}",
            plural(routed, "bed channel", "bed channels"),
            if routed == 1 {
                "its speaker"
            } else {
                "their speakers"
            }
        ));
    }
    if placed > 0 {
        parts.push(format!(
            "{} placed where 7.1.4 has no speaker",
            plural(placed, "bed channel", "bed channels")
        ));
    }
    println!(
        "  programme    {}, rendered to 7.1.4 on the room's cube",
        parts.join(", ")
    );
    println!(
        "  encoded      {units} temporal units of {frame}, {at} samples, {}, {} substreams",
        coding(&config),
        coded.substreams()
    );
    report_loudness(loudness);
    println!(
        "  headroom     {clipped} samples clipped, the bed peaking at {mix_peak:.2} of full scale \
         before the codec's integers"
    );
    if clipped > 0 {
        eprintln!(
            "warning: the render clipped {clipped} samples; the master's objects sum past full \
             scale on 7.1.4"
        );
    }
    report_written(&config, at, sample_rate)
}

fn decibels(linear: f64) -> f64 {
    20.0 * linear.log10()
}

fn lkfs(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.1} LKFS")
    } else {
        "silence".to_string()
    }
}
