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
//!
//! The measurement runs on a thread of its own, and each unit's substreams
//! are coded on threads of theirs ([`hz_iamf::Writer::with_threads`]): the
//! meters see every unit in order and each substream's coder the same calls,
//! so the sequence is the same to the byte as on one thread, which is where
//! all of it runs when no thread can be started.

use crate::lane::{LANE_DEPTH, Lane, Stage};
use hz_analysis::loudness::{ChannelWeight, Meter};
use hz_analysis::truepeak::TruePeak;
use hz_core::{Error, Result};
use hz_iamf::{Codec, Element, Headphones, Loudness, PositionKind, Writer};
use hz_io::container::SampleFormat;
use hz_programme::follow::Following;
use hz_programme::mix::Mixing;
use hz_programme::overlay;
use hz_programme::progress::Progress;
use hz_programme::quantise::{Levels, Quantiser};
use hz_programme::source::Source;
use hz_programme::{Part, parts_of};
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
    /// [`hz_programme::overlay`].
    pub overlay: Option<usize>,
    /// With `objects`: render the master's last this-many objects into the
    /// bed element, and carry the rest as objects.
    pub voices_to_bed: Option<usize>,
    /// What shapes an overlay: the same options, with the same defaults, as
    /// `encode --overlay`.
    pub overlaying: overlay::Options,
    /// What a mix does to the elements it makes: the same options, with the
    /// same defaults, as `encode`'s.
    pub mixing: Mixing,
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
/// YouTube's IAMF streams carry about 46 a channel. This is well above, for
/// a programme that is an encode's output rather than a stream's: a coupled
/// pair at 192, past the 128 Xiph calls transparent for stereo, and a mono
/// substream — an object, the centre, with no joint stereo to lean on — at
/// 96. At 64 a channel CELT folds 16 to 20 kHz rather than coding it; at 96
/// it codes it. A 7.1.4 comes to about 1.08 Mbit/s.
pub const OPUS_BITRATE: u32 = 96;

/// The output, open, with its descriptors written.
pub(crate) type Output = Writer<BufWriter<File>>;

/// The most threads a unit's substreams are coded on.
pub(crate) const THREADS: usize = 8;

/// Threads for the substreams: as many as there are cores, up to
/// [`THREADS`], or the caller's alone.
pub(crate) fn coding_threads(threads: bool) -> usize {
    if threads {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(THREADS))
    } else {
        1
    }
}

/// Open `config.out` and start a sequence of these elements there.
pub(crate) fn open(config: &Config, sample_rate: u32, elements: Vec<Element>) -> Result<Output> {
    open_presenting(
        config,
        sample_rate,
        elements,
        hz_iamf::Presentation::default(),
    )
}

/// [`open`], the mix presentation stating `presentation` as well.
pub(crate) fn open_presenting(
    config: &Config,
    sample_rate: u32,
    elements: Vec<Element>,
    presentation: hz_iamf::Presentation,
) -> Result<Output> {
    let out_file = File::create(&config.out).map_err(|e| Error::io(&config.out, e))?;
    let iamf_config = hz_iamf::Config {
        elements,
        codec: config.codec,
        sample_rate,
        bits: config.bits,
        frame: config.frame,
        headphones: config.headphones,
        presentation,
    };
    let out = BufWriter::new(out_file);
    let writer = if crate::is_matroska(&config.out) {
        Writer::matroska(out, iamf_config, "harlettizer")
    } else {
        Writer::new(out, iamf_config)
    };
    writer.map_err(|e| match e {
        hz_iamf::Error::Io(e) => Error::io(&config.out, e),
        hz_iamf::Error::Unsupported(what) => Error::unsupported(&config.out, what),
    })
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

/// One unit as the measurement takes it: the whole mix as a decoder renders
/// it on 7.1.4, and the dialogue alone when it has an element of its own.
pub(crate) struct Measured {
    pub(crate) whole: Vec<f32>,
    pub(crate) dialogue: Vec<f32>,
    /// The interleaved samples of each that are this unit's.
    pub(crate) len: usize,
}

/// The measurement: the whole mix, and the dialogue alone when there is one.
pub(crate) struct Measuring {
    pub(crate) whole: Measure,
    pub(crate) dialogue: Option<Measure>,
}

impl Stage for Measuring {
    type In = Measured;
    type Out = Measured;

    fn run(&mut self, unit: Measured) -> Measured {
        self.whole.push(&unit.whole[..unit.len]);
        if let Some(dialogue) = &mut self.dialogue {
            dialogue.push(&unit.dialogue[..unit.len]);
        }
        unit
    }
}

/// The measurement on a thread of its own, and the buffers it hands back to
/// be filled again. Each meter sees the units in the order they are pushed,
/// so it measures what it would on the caller's thread.
pub(crate) struct Measurer {
    lane: Lane<Measuring>,
    spare: Vec<Measured>,
}

impl Measurer {
    /// `measuring` on a thread of its own when `threads`, its units up to
    /// `width` interleaved samples.
    pub(crate) fn new(measuring: Measuring, width: usize, threads: bool) -> Self {
        let dialogue = if measuring.dialogue.is_some() {
            width
        } else {
            0
        };
        let spare = (0..=LANE_DEPTH)
            .map(|_| Measured {
                whole: vec![0.0; width],
                dialogue: vec![0.0; dialogue],
                len: 0,
            })
            .collect();
        Self {
            lane: Lane::new(measuring, "iamf-measure", threads),
            spare,
        }
    }

    /// A unit to fill: one not yet handed over, or the oldest measured.
    pub(crate) fn unit(&mut self) -> Measured {
        self.spare.pop().unwrap_or_else(|| self.lane.recv())
    }

    pub(crate) fn push(&mut self, unit: Measured) {
        self.lane.send(unit);
    }

    /// The measurement, every unit pushed measured.
    pub(crate) fn finish(self) -> Measuring {
        self.lane.into_stage()
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
    run_on(config, true)
}

/// [`run`], the measurement and the substreams on threads of their own when
/// `threads`, or all on the caller's.
pub(crate) fn run_on(config: Config, threads: bool) -> Result<()> {
    if config.objects.is_some() {
        return crate::iamf_objects::run(config, threads);
    }
    let path = config.input.as_path();
    let mut source = Source::open(path, config.mono_prefix.as_deref())?;
    // Creating the output truncates it, and every file of the input is still
    // to be read: refuse one that is the input, before any work is done.
    hz_io::paths::refuse_inputs(source.files(), &config.out)?;
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

    // Every track to a source of the mixdown, in the master's order: a bed
    // channel to its speaker, or — one 7.1.4 does not have, a wide, a top
    // side — an object that never moves from its speaker; an object on its
    // path.
    let mut following = Following::new(Mixdown::new(&layout)?);
    let (mut routed, mut placed, mut objects) = (0usize, 0usize, 0usize);
    for part in parts_of(path, &source)? {
        match part {
            Part::Lfe { channel } => {
                following.mixdown().speaker(channel, lfe);
                routed += 1;
            }
            Part::Bed {
                channel,
                name,
                position,
            } => {
                let label = hz_core::speakers::by_master_name(name).map_or(name, |s| s.label);
                let mixdown = following.mixdown();
                if let Some(speaker) = layout.index_of(label) {
                    mixdown.speaker(channel, speaker);
                    routed += 1;
                } else {
                    let index = mixdown.object(channel);
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
            Part::Object { channel, keyframes } => {
                following.object(channel, &keyframes);
                objects += 1;
            }
        }
    }
    if routed + placed + objects == 0 {
        return Err(Error::unsupported(path, "nothing in it to render"));
    }

    let mut writer = open(&config, sample_rate, vec![Element::Channels(coded)])?
        .with_threads(coding_threads(threads));

    let frame = config.frame;
    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    let mut progress = Progress::new(config.progress, total);

    let mut raw = vec![0i32; frame * stride];
    let mut input = vec![0f32; frame * stride];
    let mut mixed = vec![0f64; frame * width];
    let mut quantised = vec![0i32; frame * width];
    let measuring = Measuring {
        whole: Measure::new(sample_rate),
        dialogue: None,
    };
    let mut measurer = Measurer::new(measuring, frame * width, threads);

    let quantiser = Quantiser::bits(config.bits);
    let full_scale = quantiser.full_scale();
    let mut levels = Levels::default();

    let mut at = 0u64;
    while at < total {
        let want = (total - at).min(frame as u64) as usize;
        let got = source.fill(&mut raw, want)?;
        if got == 0 {
            break;
        }
        for (x, &r) in input[..got * stride].iter_mut().zip(&raw[..got * stride]) {
            *x = (f64::from(r) * input_scale) as f32;
        }

        // The block, split at every update the master makes inside it so
        // that each lands on its own sample.
        mixed[..got * width].fill(0.0);
        following.render(
            &input[..got * stride],
            stride,
            got,
            at,
            &mut mixed[..got * width],
        );

        // Into the codec's integers, and measured as a decoder will hand them
        // back: the bed as it is, and folded to stereo the decoder's way.
        for (q, &m) in quantised[..got * width]
            .iter_mut()
            .zip(&mixed[..got * width])
        {
            *q = quantiser.code(m, &mut levels);
        }
        let mut measured = measurer.unit();
        for (f, &q) in measured.whole[..got * width]
            .iter_mut()
            .zip(&quantised[..got * width])
        {
            *f = (f64::from(q) / full_scale) as f32;
        }
        measured.len = got * width;
        measurer.push(measured);

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

    let loudness = measurer.finish().whole.finish();
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
        "  headroom     {} samples clipped, the bed peaking at {:.2} of full scale before the \
         codec's integers",
        levels.clipped, levels.peak
    );
    if levels.clipped > 0 {
        eprintln!(
            "warning: the render clipped {} samples; the master's objects sum past full scale on \
             7.1.4",
            levels.clipped
        );
    }
    report_written(&config, at, sample_rate)
}

fn decibels(linear: f64) -> f64 {
    20.0 * linear.log10()
}

pub(crate) fn lkfs(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.1} LKFS")
    } else {
        "silence".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use std::path::Path;

    /// A third of a second and some: sixteen units of 1024 and a short one
    /// to end on, seventeen of Opus's 960.
    const SAMPLES: usize = 16 * 1024 + 300;

    /// How a test codes the master: a name, the codec, the frame, how the
    /// objects travel — rendered to the bed, or as objects with this many
    /// voices rendered into a dialogue element — and the output's extension.
    type Case = (&'static str, Codec, usize, Option<usize>, &'static str);

    fn cases() -> Vec<Case> {
        let mut cases: Vec<Case> = vec![
            ("bed-flac", Codec::Flac, 1024, None, "iamf"),
            ("bed-lpcm", Codec::Lpcm, 1024, None, "iamf"),
            ("objects-flac", Codec::Flac, 1024, Some(0), "mka"),
            ("voices-flac", Codec::Flac, 4096, Some(1), "iamf"),
        ];
        if cfg!(feature = "opus") {
            let opus = Codec::Opus { bitrate: 64_000 };
            cases.push(("bed-opus", opus, OPUS_FRAME, None, "mka"));
            cases.push(("objects-opus", opus, OPUS_FRAME, Some(0), "iamf"));
        }
        cases
    }

    /// `input` coded into `dir` as `case` says, the measurement and the
    /// substreams — and with objects the writer — on threads of their own
    /// when `threads`: how the encode ended, and what it wrote.
    fn coded(
        input: &Path,
        dir: &Path,
        case: &Case,
        threads: bool,
    ) -> (std::result::Result<(), String>, Vec<u8>) {
        let (name, codec, frame, voices, extension) = *case;
        let out = dir.join(format!("{name}-{threads}.{extension}"));
        let config = Config {
            input: input.to_path_buf(),
            out: out.clone(),
            frames: None,
            mono_prefix: None,
            codec,
            bits: 24,
            frame,
            headphones: Headphones::Stereo,
            progress: false,
            objects: voices.map(|_| PositionKind::Cart16),
            elements: None,
            overlay: None,
            voices_to_bed: voices.filter(|&n| n > 0),
            overlaying: overlay::Options::default(),
            mixing: Mixing::default(),
        };
        let ended = run_on(config, threads).map_err(|e| e.to_string());
        (ended, std::fs::read(&out).unwrap())
    }

    /// The temporal units in a standalone sequence: its temporal delimiters.
    fn units(bytes: &[u8]) -> usize {
        let mut at = 0;
        let mut delimiters = 0;
        while at < bytes.len() {
            let header = bytes[at];
            at += 1;
            let (mut size, mut shift) = (0usize, 0);
            loop {
                let b = bytes[at];
                at += 1;
                size |= usize::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            if header >> 3 == 4 {
                delimiters += 1;
            }
            at += size;
        }
        delimiters
    }

    /// The measurement on a thread of its own, the substreams on theirs and,
    /// with objects, the writer on its own write what one thread writes: the
    /// same bytes, a bed or objects, every codec, standalone or in Matroska,
    /// the loudness patched in at the end the same.
    #[test]
    fn the_threads_write_what_one_thread_writes() {
        let dir = fixtures::scratch("iamf", "threads");
        let input = fixtures::master(&dir, SAMPLES);
        for case in cases() {
            let [(inline, alone), (threaded, beside)] =
                [false, true].map(|threads| coded(&input, &dir, &case, threads));
            let (name, ..) = case;
            assert_eq!(inline, Ok(()), "{name}");
            assert_eq!(threaded, Ok(()), "{name}");
            assert!(!alone.is_empty(), "{name}");
            assert!(alone == beside, "{name}: the same sequence on threads");
        }
    }

    /// Audio that ends before its header says, in the sixth unit of 1024:
    /// the encode stops there with the same error, on threads or not, and
    /// leaves the same units — the five read whole.
    #[test]
    fn a_read_that_fails_stops_the_encode_where_one_thread_stops() {
        const CUT: usize = 5 * 1024 + 300;
        let dir = fixtures::scratch("iamf", "cut");
        let input = fixtures::master(&dir, SAMPLES);
        let audio = dir.join("m.atmos.audio");
        let length = std::fs::metadata(&audio).unwrap().len();
        // Ten channels of 24 bits.
        let missing = ((SAMPLES - CUT) * 10 * 3) as u64;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&audio)
            .unwrap()
            .set_len(length - missing)
            .unwrap();
        for case in cases() {
            let (name, _, frame, _, extension) = case;
            if frame != 1024 {
                continue;
            }
            let (inline, alone) = coded(&input, &dir, &case, false);
            assert!(inline.is_err(), "{name}: the read fails");
            if extension == "iamf" {
                assert_eq!(units(&alone), 5, "{name}: the units read whole");
            }
            for run in 0..8 {
                let (threaded, beside) = coded(&input, &dir, &case, true);
                assert_eq!(threaded, inline, "{name}, run {run}: said the same way");
                assert!(alone == beside, "{name}, run {run}: the same units left");
            }
        }
    }
}
