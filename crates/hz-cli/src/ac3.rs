//! `harlettizer ac3`: a programme rendered to 5.1 or stereo and coded as AC-3.
//!
//! The objects do not survive as objects — AC-3 has none — so the master is
//! rendered as `iamf` renders it, its bed channels routed to their speakers
//! and its objects panned on the room's cube, onto 7.1; and 7.1 is folded to
//! the layout asked for by the fold measured off shipped TrueHD streams
//! ([`hz_render::fold::presentation_matrix`]), the one this project's own
//! TrueHD presentations carry. The cube has no place for a 5.1 surround at
//! ±110°, and the fold is what a 5.1 listener of the same master hears from
//! the lossless stream. The loudness is measured on what is coded, before the
//! codec, and reported beside the dialnorm the stream states.
//!
//! With `--joc` the objects do survive: E-AC-3 carries them beside the 5.1
//! core by joint object coding ([`crate::joc`]).

use crate::lane::{LANE_DEPTH, Lane, Stage};
use hz_ac3::{ChannelMode, DELAY, Encoder, FRAME_SAMPLES};
use hz_analysis::loudness::{ChannelWeight, Meter};
use hz_core::{Error, Result};
use hz_io::container::SampleFormat;
use hz_programme::follow::Following;
use hz_programme::progress::Progress;
use hz_programme::source::Source;
use hz_programme::{Part, parts_of};
use hz_render::fold::{FoldMode, presentation_matrix};
use hz_render::{Keyframe, Layout, Mixdown};
use std::io::Write;
use std::path::PathBuf;

/// What `ac3` was asked to do.
pub struct Config {
    pub input: PathBuf,
    pub out: PathBuf,
    pub frames: Option<u64>,
    pub mono_prefix: Option<PathBuf>,
    /// `5.1` or `2.0`.
    pub layout: Layout,
    /// kb/s; the encoder's default for the channel count when not given.
    pub bit_rate: Option<u32>,
    pub dialnorm: i8,
    pub coupling: bool,
    /// Write one independent E-AC-3 substream rather than AC-3.
    pub eac3: bool,
    /// With `eac3` and 5.1, carry the objects by joint object coding.
    pub joc: bool,
    pub progress: bool,
}

/// The threads a joint object coding encode runs on, at most. A frame's
/// work comes in pieces of an object or a band each, a dozen or so at a
/// time, and the encoder's own steps between them are one thread's: past
/// six, more threads wait more than they work, and a thread with nothing to
/// do looks for work before it sleeps — the encode no sooner, the processor
/// busier (`docs/joc.md`).
const JOC_THREADS: usize = 6;

pub fn run(config: Config) -> Result<()> {
    if !config.joc {
        return encode(config);
    }
    // Joint object coding fits a frame's objects, and the downmix's side of
    // their equations, at once (`hz-joc`'s `parallel` feature): on a pool of
    // its own that the whole encode runs in, so that the thread driving it
    // works in the pool between its own steps. The same stream, to the byte.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(JOC_THREADS));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|n| format!("joc-{n}"))
        // The stack the encode has on the main thread.
        .stack_size(8 << 20)
        .build()
        // A process that may start no more threads (a task limit) still
        // encodes, on the thread it has: a pool of that thread alone starts
        // none, and is what Rayon itself falls back to where there are no
        // threads. Not `encode` outside any pool: the estimator's work would
        // start Rayon's global pool, which fails the same way, and panics.
        // Rayon keeps that thread its own for the rest of the process, and
        // the stream is the same to the byte.
        .or_else(|_| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .use_current_thread()
                .build()
        })
        // Refused before anything is opened, the output left as it was.
        .map_err(|e| {
            Error::io(
                &config.out,
                std::io::Error::other(format!("no thread to encode on: {e}")),
            )
        })?;
    pool.install(|| encode(config))
}

fn encode(config: Config) -> Result<()> {
    encode_on(config, true)
}

/// [`encode`], the programme read and rendered on a thread of its own when
/// `threads` — and with objects, the joint object coding's encoders and fold
/// on theirs — on the caller's otherwise.
fn encode_on(config: Config, threads: bool) -> Result<()> {
    let path = config.input.as_path();
    let source = Source::open(path, config.mono_prefix.as_deref())?;
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

    let layout = &config.layout;
    let width = layout.channels();
    let rendered = Layout::surround_7_1();
    let rendered_width = rendered.channels();
    let fold = presentation_matrix(layout, FoldMode::default())
        .map_err(|e| Error::unsupported(path, e.to_string()))?
        .expect("5.1 and 2.0 are not 7.1");
    let (mode, lfe) = match layout.name {
        "5.1" => (ChannelMode::ThreeTwo, true),
        "2.0" => (ChannelMode::Stereo, false),
        other => {
            return Err(Error::unsupported(
                path,
                format!("AC-3 to {other}; 5.1 or 2.0"),
            ));
        }
    };
    let lfe_speaker = rendered.index_of("LFE1");
    if config.joc && (!config.eac3 || layout.name != "5.1") {
        return Err(Error::unsupported(
            path,
            "joint object coding without E-AC-3 on 5.1; it rebuilds objects from a 5.1 core",
        ));
    }
    let mut elements = Vec::new();
    let mut master_lfe = false;

    // Every track to a source of the mixdown, as `iamf` routes them.
    let mut following = Following::new(Mixdown::new(&rendered)?);
    let (mut routed, mut placed, mut objects, mut dropped) = (0usize, 0, 0, 0);
    for part in parts_of(path, &source)? {
        match part {
            Part::Lfe { channel } => {
                following
                    .mixdown()
                    .speaker(channel, lfe_speaker.expect("7.1 has an LFE"));
                routed += 1;
                master_lfe = true;
                // Stereo has no LFE, and the fold leaves it out, as a decoder
                // folding 5.1 to stereo does.
                dropped += usize::from(!layout.has("LFE1"));
            }
            Part::Bed {
                channel,
                name,
                position,
            } => {
                let label = hz_core::speakers::by_master_name(name).map_or(name, |s| s.label);
                elements.push(crate::joc::Element::Bed { channel, position });
                let mixdown = following.mixdown();
                if let Some(speaker) = rendered.index_of(label) {
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
                elements.push(crate::joc::Element::object(channel, keyframes));
                objects += 1;
            }
        }
    }
    if routed + placed + objects == 0 {
        return Err(Error::unsupported(path, "nothing in it to render"));
    }

    let codec = hz_ac3::Config {
        format: if config.eac3 {
            hz_ac3::Format::Eac3
        } else {
            hz_ac3::Format::Ac3
        },
        sample_rate,
        mode,
        lfe,
        bit_rate: config.bit_rate,
        coupling: config.coupling,
        metadata: hz_ac3::Metadata {
            dialnorm: config.dialnorm,
            ..hz_ac3::Metadata::default()
        },
    };
    let element_count = elements.len();
    let mut sink = if config.joc {
        Sink::Joc(Box::new(crate::joc::Writer::new(
            codec,
            elements,
            master_lfe,
            crate::joc::downmix_type(FoldMode::default()),
            &config.out,
            threads,
        )?))
    } else {
        Sink::Plain(Box::new(
            Encoder::new(codec).map_err(|e| Error::unsupported(&config.out, e.0))?,
        ))
    };
    let out_file = std::fs::File::create(&config.out).map_err(|e| Error::io(&config.out, e))?;
    let mut out = std::io::BufWriter::new(out_file);

    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    let mut progress = Progress::new(config.progress, total);

    let weights: Vec<ChannelWeight> = layout
        .speakers
        .iter()
        .map(|label| ChannelWeight::for_speaker_label(label))
        .collect();
    // The programme is read and rendered a few frames ahead of the encoders,
    // while they code the frames before: a frame rendered is one read, in
    // order, whichever thread renders it.
    let mut render = Lane::new(
        Render {
            source,
            following,
            fold,
            meter: Meter::new(sample_rate, &weights),
            input_scale,
            stride,
            width,
            rendered_width,
            total,
            at: 0,
            raw: vec![0i32; FRAME_SAMPLES * stride],
            mixed: vec![0f64; FRAME_SAMPLES * rendered_width],
            interleaved: vec![0f32; FRAME_SAMPLES * width],
            clipped: 0,
            peak: 0.0,
        },
        "render",
        threads,
    );
    for _ in 0..LANE_DEPTH {
        render.send(Rendered::new(stride, width));
    }
    let mut frame = Vec::new();

    let lag = if config.joc {
        crate::joc::LATENCY
    } else {
        DELAY
    };
    let mut at = 0u64;
    let mut syncframes = 0u64;
    loop {
        let rendered = match render.recv() {
            Ok(rendered) => rendered,
            // The frames before stay where a single thread would have left
            // them: with objects, the syncframes still being written for
            // them are waited for and written first.
            Err(e) => {
                return Err(match &mut sink {
                    Sink::Plain(_) => e,
                    Sink::Joc(writer) => {
                        frame.clear();
                        let e = writer.stop(e, &mut frame);
                        out.write_all(&frame)
                            .map_err(|e| Error::io(&config.out, e))?;
                        e
                    }
                });
            }
        };
        let got = rendered.got;

        frame.clear();
        let mut refs: [&[f32]; 6] = [&[]; 6];
        for (r, plane) in refs.iter_mut().zip(&rendered.planar) {
            *r = plane;
        }
        match &mut sink {
            Sink::Plain(encoder) => encoder
                .encode(&refs[..width], &mut frame)
                .map_err(|e| Error::refused(&config.out, e.0))?,
            Sink::Joc(writer) => {
                let coded =
                    writer.frame(&refs[..width], &rendered.input, stride, got, at, &mut frame);
                // The writer hands back whole syncframes, and its encoders
                // run behind it: those written before a refusal it finds now
                // are the stream up to it, and stay where they were written.
                if coded.is_err() {
                    out.write_all(&frame)
                        .map_err(|e| Error::io(&config.out, e))?;
                }
                coded?
            }
        }
        out.write_all(&frame)
            .map_err(|e| Error::io(&config.out, e))?;
        syncframes += 1;

        at += got as u64;
        if let Some(progress) = progress.as_mut() {
            progress.at(at);
        }
        // Past the input's end, the frames keep coming until the decoder's
        // output has caught up with it: its last samples are DELAY behind —
        // and with objects, joint object coding's latency further — and a
        // final frame that is nearly full has not that much silence in it to
        // carry them out.
        let ended = got < FRAME_SAMPLES;
        if ended && syncframes >= syncframes_to_cover(at, lag) {
            break;
        }
        // Its buffers, for a frame more.
        render.send(rendered);
    }
    let Render {
        mut meter,
        clipped,
        peak,
        ..
    } = render.into_stage();
    if let Sink::Joc(writer) = &mut sink {
        frame.clear();
        let finished = writer.finish(&mut frame);
        out.write_all(&frame)
            .map_err(|e| Error::io(&config.out, e))?;
        finished?;
    }
    out.flush().map_err(|e| Error::io(&config.out, e))?;
    if at == 0 {
        return Err(Error::unsupported(path, "no audio to encode"));
    }
    meter.flush();

    let mut parts = Vec::new();
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if objects > 0 {
        parts.push(plural(objects, "object", "objects"));
    }
    if routed > 0 {
        parts.push(format!(
            "{} routed to their speakers",
            plural(routed, "bed channel", "bed channels")
        ));
    }
    if placed > 0 {
        parts.push(format!(
            "{} placed where 7.1 has no speaker",
            plural(placed, "bed channel", "bed channels")
        ));
    }
    if dropped > 0 {
        parts.push("the LFE left out".to_string());
    }
    println!(
        "  programme    {}, rendered to 7.1 on the room's cube and folded to {}",
        parts.join(", "),
        layout.name
    );
    let (encoder, syncframes) = match &sink {
        Sink::Plain(encoder) => (encoder.as_ref(), syncframes),
        Sink::Joc(writer) => (writer.encoder(), writer.written()),
    };
    println!(
        "  encoded      {syncframes} syncframes, {at} samples, {} at {} kb/s{}",
        if config.eac3 { "E-AC-3" } else { "AC-3" },
        encoder.bit_rate(),
        if encoder.coupled() {
            ", high band coupled"
        } else {
            ""
        }
    );
    if let Sink::Joc(writer) = &sink {
        let carried = writer.elements();
        println!(
            "  objects      {carried} carried by joint object coding{}, fitted to the core a \
             decoder plays; every Evolution frame {} bytes{}",
            if carried < element_count {
                format!(", the master's {element_count} elements folded into them")
            } else {
                String::new()
            },
            writer.evolution_bytes(),
            match writer.squeezed() {
                0 => String::new(),
                n => format!(", {n} payloads quantised coarser to fit"),
            }
        );
        if let Some(folder) = writer.folder() {
            let cost = &folder.mixer.cost;
            let smoothing = folder.placer.smoothing();
            println!(
                "  fold         {:.4} mean over the presentations, {:.3} at its worst, as a \
                 fraction of the object's own gains; a block a syncframe, steadied by a hold \
                 over {} behind and {} ahead",
                cost.mean(),
                cost.worst,
                smoothing.behind,
                smoothing.ahead
            );
            println!(
                "  floor        {:.1} dBFS at the playback level a dialnorm of {} implies, \
                 {:.2} objects a block under it",
                folder.mixer.floor.threshold_dbfs(),
                config.dialnorm,
                cost.inaudible as f64 / cost.blocks.max(1) as f64
            );
        }
    }
    println!(
        "  loudness     {} on {}, the stream stating a dialnorm of {}",
        crate::iamf::lkfs(meter.integrated()),
        layout.name,
        config.dialnorm
    );
    println!(
        "  headroom     {clipped} samples clipped, the render peaking at {peak:.2} of full scale"
    );
    if clipped > 0 {
        eprintln!(
            "warning: the render clipped {clipped} samples; the master's objects sum past full \
             scale on {}",
            layout.name
        );
    }
    let bytes = std::fs::metadata(&config.out)
        .map_err(|e| Error::io(&config.out, e))?
        .len();
    println!("  wrote        {}, {bytes} bytes", config.out.display());
    Ok(())
}

/// Syncframes that carry `samples` of input all the way out of a decoder
/// whose output lags its input by `lag`.
fn syncframes_to_cover(samples: u64, lag: usize) -> u64 {
    (samples + lag as u64).div_ceil(FRAME_SAMPLES as u64)
}

/// Where each frame of core goes: an encoder, or the joint object coding
/// writer that drives two.
enum Sink {
    Plain(Box<Encoder>),
    Joc(Box<crate::joc::Writer>),
}

/// The programme read a frame at a time, rendered to 7.1 and folded to the
/// layout coded, its loudness measured and its peak kept: on a thread of its
/// own where one can be started ([`Lane`]), rendering frames while the
/// encoders code the ones before.
struct Render {
    source: Source,
    following: Following,
    /// A row for each channel of 7.1, a column for each one coded.
    fold: [[f64; 8]; 8],
    meter: Meter,
    input_scale: f64,
    /// The master's channels, the layout's, and 7.1's.
    stride: usize,
    width: usize,
    rendered_width: usize,
    /// The samples to encode, and those read.
    total: u64,
    at: u64,
    raw: Vec<i32>,
    mixed: Vec<f64>,
    interleaved: Vec<f32>,
    /// Samples the fold took past full scale, and its highest.
    clipped: u64,
    peak: f64,
}

/// A frame of the programme: the master's samples, `got` of them,
/// interleaved, and its render, planar in the layout's order. A frame past
/// the input's end is silence.
struct Rendered {
    input: Vec<f32>,
    planar: Vec<Vec<f32>>,
    got: usize,
}

impl Rendered {
    fn new(stride: usize, width: usize) -> Self {
        Self {
            input: vec![0f32; FRAME_SAMPLES * stride],
            planar: vec![vec![0f32; FRAME_SAMPLES]; width],
            got: 0,
        }
    }
}

impl Stage for Render {
    type In = Rendered;
    type Out = Result<Rendered>;

    /// The next frame, rendered into `frame`'s buffers.
    fn run(&mut self, mut frame: Rendered) -> Result<Rendered> {
        let (stride, width, rendered_width) = (self.stride, self.width, self.rendered_width);
        let want = (self.total - self.at).min(FRAME_SAMPLES as u64) as usize;
        let got = if want == 0 {
            0
        } else {
            match self.source.fill(&mut self.raw, want) {
                Ok(got) => got,
                Err(e) => {
                    // The encode stops at the failure: nothing past it is read.
                    self.at = self.total;
                    return Err(e);
                }
            }
        };
        for (x, &r) in frame.input[..got * stride]
            .iter_mut()
            .zip(&self.raw[..got * stride])
        {
            *x = (f64::from(r) * self.input_scale) as f32;
        }
        self.mixed.fill(0.0);
        if got > 0 {
            self.following.render(
                &frame.input[..got * stride],
                stride,
                got,
                self.at,
                &mut self.mixed[..got * rendered_width],
            );
        }
        for (mixed, out) in self
            .mixed
            .chunks_exact(rendered_width)
            .zip(self.interleaved.chunks_exact_mut(width))
        {
            for (target, f) in out.iter_mut().enumerate() {
                let m: f64 = mixed
                    .iter()
                    .zip(&self.fold)
                    .map(|(x, row)| x * row[target])
                    .sum();
                self.peak = self.peak.max(m.abs());
                if m.abs() > 1.0 {
                    self.clipped += 1;
                }
                *f = m.clamp(-1.0, 1.0) as f32;
            }
        }
        self.meter.push(&self.interleaved[..got * width]);
        for (ch, plane) in frame.planar.iter_mut().enumerate() {
            for (n, v) in plane.iter_mut().enumerate() {
                *v = self.interleaved[n * width + ch];
            }
        }
        frame.got = got;
        self.at += got as u64;
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame's padding that carries the core's delay out need not carry the
    /// objects' too: 1280 samples fit one syncframe with the core's 256
    /// behind, but the objects' 833 need a second, or the last 577 samples
    /// of the programme are lost to them.
    #[test]
    fn the_stream_runs_until_the_objects_are_out() {
        assert_eq!(syncframes_to_cover(1280, DELAY), 1);
        assert_eq!(syncframes_to_cover(1280, crate::joc::LATENCY), 2);
        assert_eq!(crate::joc::LATENCY, DELAY + 577);
        assert_eq!(syncframes_to_cover(1536 - 833, crate::joc::LATENCY), 1);
        assert_eq!(syncframes_to_cover(1536 - 832, crate::joc::LATENCY), 2);
        assert_eq!(syncframes_to_cover(0, DELAY), 1);
    }

    /// Ten frames and some: the core's delay is carried out in the silence
    /// that ends the eleventh, the objects' latency needs a twelfth.
    const SAMPLES: usize = 10 * FRAME_SAMPLES + 1000;

    fn master(dir: &std::path::Path) -> PathBuf {
        crate::fixtures::master(dir, SAMPLES)
    }

    fn scratch(name: &str) -> PathBuf {
        crate::fixtures::scratch("ac3", name)
    }

    /// What a test codes a master to: a name, the layout, whether E-AC-3,
    /// whether the objects are carried by joint object coding, and kb/s.
    type Case = (&'static str, Layout, bool, bool, u32);

    /// AC-3, E-AC-3 in stereo, and E-AC-3 carrying the objects.
    fn cases() -> [Case; 3] {
        [
            ("ac3", Layout::surround_5_1(), false, false, 448),
            ("eac3-stereo", Layout::stereo(), true, false, 192),
            ("joc", Layout::surround_5_1(), true, true, 448),
        ]
    }

    /// The bytes of one syncframe at `kbps`.
    fn syncframe(kbps: u32) -> usize {
        kbps as usize * 1000 / 8 * FRAME_SAMPLES / 48_000
    }

    /// `input` coded into `dir` as `case` says, on threads of their own
    /// when `threads` — the render, and with objects the encoders and the
    /// fold — or all on the test's, the estimator's own pool aside: how the
    /// encode ended, and what it wrote.
    fn coded(
        input: &std::path::Path,
        dir: &std::path::Path,
        case: &Case,
        threads: bool,
    ) -> (std::result::Result<(), String>, Vec<u8>) {
        let (name, layout, eac3, joc, kbps) = case;
        let out = dir.join(format!("{name}-{threads}"));
        let config = Config {
            input: input.to_path_buf(),
            out: out.clone(),
            frames: None,
            mono_prefix: None,
            layout: layout.clone(),
            bit_rate: Some(*kbps),
            dialnorm: -31,
            coupling: true,
            eac3: *eac3,
            joc: *joc,
            progress: false,
        };
        let ended = encode_on(config, threads).map_err(|e| e.to_string());
        (ended, std::fs::read(&out).unwrap())
    }

    /// The programme rendered on a thread of its own, and with objects
    /// coded on theirs, codes to what one thread codes: the same bytes, AC-3
    /// and E-AC-3, with objects and without, and as many syncframes as carry
    /// the programme out.
    #[test]
    fn the_render_on_its_thread_codes_what_one_thread_codes() {
        let dir = scratch("render");
        let input = master(&dir);
        for case in cases() {
            let [(inline, alone), (threaded, beside)] =
                [false, true].map(|threads| coded(&input, &dir, &case, threads));
            let (name, _, _, joc, kbps) = case;
            assert_eq!(inline, Ok(()), "{name}");
            assert_eq!(threaded, Ok(()), "{name}");
            assert!(alone == beside, "{name}: the same stream on a thread");
            let lag = if joc { crate::joc::LATENCY } else { DELAY };
            assert_eq!(alone.len() % syncframe(kbps), 0, "{name}");
            assert_eq!(
                (alone.len() / syncframe(kbps)) as u64,
                syncframes_to_cover(SAMPLES as u64, lag),
                "{name}: the programme carried out, and no further"
            );
        }
    }

    /// Audio that ends before its header says, in the sixth frame: the
    /// encode stops there with the same error, on threads or not, and leaves
    /// the same stream behind — every frame before the failed read coded, and
    /// nothing after it; with objects, two fewer than were read: the
    /// estimator holds one for the next one's payload, and the probe codes
    /// the next. Threads that leave a syncframe behind or not by
    /// how far they got are given a few runs to show it.
    #[test]
    fn a_read_that_fails_stops_the_encode_where_one_thread_stops() {
        const CUT: usize = 5 * FRAME_SAMPLES + 300;
        let dir = scratch("cut");
        let input = master(&dir);
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
            let (inline, alone) = coded(&input, &dir, &case, false);
            let (name, _, _, joc, kbps) = case;
            assert!(inline.is_err(), "{name}: the read fails");
            let coded_frames = if joc { 3 } else { 5 };
            assert_eq!(
                alone.len(),
                coded_frames * syncframe(kbps),
                "{name}: the frames read"
            );
            for run in 0..8 {
                let (threaded, beside) = coded(&input, &dir, &case, true);
                assert_eq!(threaded, inline, "{name}, run {run}: said the same way");
                assert!(alone == beside, "{name}, run {run}: the same stream left");
            }
        }
    }
}
