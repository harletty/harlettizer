//! `harlettizer iamf --objects`: a programme's objects carried as IAMF v2.0
//! objects.
//!
//! Each object of the master becomes an element of its own — one mono
//! substream, positioned by a parameter its mix presentation declares — and
//! its path through the room becomes that parameter's blocks. What cannot be
//! an object is carried beside them: the LFE, which has no direction, in a
//! channel-based element of its own; a bed channel, which is a speaker feed,
//! as an object that never leaves its speaker.
//!
//! # A master's path, and the format's
//!
//! A master moves an object by updates: be here, this loud, and take this
//! long getting there. Between updates it holds; across a ramp it moves in a
//! straight line through the cube. So an object's path is piecewise linear,
//! and IAMF's position blocks are exactly that — runs of subblocks, each a
//! step or a line. The path is computed once from the master's updates
//! ([`Path`]), and every block is cut from it: a unit's subblocks break
//! where the path does, so a move that starts mid-unit starts on its sample,
//! and an object standing still costs one block for as long as it stands.
//!
//! The gain cannot travel the same way: an object element has a mix gain and
//! nothing finer. So it is applied to the audio — the object's samples times
//! its gain at each sample, ramps included — which is what a renderer would
//! have done with it, done earlier.
//!
//! # Looking at what was meant
//!
//! `HZ_IAMF_TRACE=<dir>` writes, for diagnosis, what the encode meant for a
//! decoder to hand back: `positions.csv`, every object's position every 256
//! samples of every unit on the sequence's timeline, and `element-<n>.f32`,
//! each element's samples as coded. A decoder's object passthrough is checked
//! against them.
//!
//! # What does not survive
//!
//! An object's size, and its rendering mode — snapping, zone exclusions,
//! screen scaling. IAMF v2.0 positions a point and leaves the rest to the
//! renderer.

use crate::encode::Progress;
use crate::iamf::{self, Config, Measure};
use crate::source::{Source, keyframes_of, tracks};
use hz_core::{Error, Result, speakers};
use hz_iamf::{Animation, Element, PositionBlock, PositionKind, Subblock};
use hz_io::adm::model::TypeDefinition;
use hz_io::container::SampleFormat;
use hz_render::{Keyframe, Layout, Mixdown};
use std::io::Write;

/// Samples between two traced positions.
const TRACE_INTERVAL: usize = 256;

/// The most channels an IA sequence carries: IAMF v2.0's advanced-2.
const BUDGET: usize = 28;

/// Where an object is and how loud, at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
struct State {
    position: [f64; 3],
    gain: f64,
}

impl State {
    fn lerp(self, to: State, a: f64) -> State {
        State {
            position: [0, 1, 2].map(|i| self.position[i] + (to.position[i] - self.position[i]) * a),
            gain: self.gain + (to.gain - self.gain) * a,
        }
    }
}

/// A stretch of an object's path: a straight line from `from` at `start` to
/// `to` at `end`, which is a standstill when they agree.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Segment {
    start: u64,
    /// `u64::MAX` for the last, which lasts.
    end: u64,
    from: State,
    to: State,
}

impl Segment {
    fn at(&self, t: u64) -> State {
        if self.end == u64::MAX || self.from == self.to {
            return self.from;
        }
        let a = (t.saturating_sub(self.start)) as f64 / (self.end - self.start) as f64;
        self.from.lerp(self.to, a.min(1.0))
    }

    fn still(&self) -> bool {
        self.from.position == self.to.position
    }
}

/// An object's path through the programme, from the master's updates.
#[derive(Debug, Clone)]
struct Path {
    segments: Vec<Segment>,
}

impl Path {
    /// The updates as a path: each one moves the object from wherever it is
    /// when the update arrives — part way along a ramp the update cuts short,
    /// if it does — to where it asks, over the samples it asks for. Silent and
    /// at its first position until its first update.
    fn new(keyframes: &[Keyframe]) -> Self {
        let mut keyframes = keyframes.to_vec();
        keyframes.sort_by_key(|k| k.sample_pos);
        let first = keyframes.first().map_or([0.0, 1.0, 0.0], |k| k.position);
        let mut from = State {
            position: first,
            gain: 0.0,
        };
        let mut to = from;
        let (mut ramp_start, mut ramp_end) = (0u64, 0u64);
        let mut last = 0u64;
        let mut segments = Vec::new();
        let value = |from: State, to: State, start: u64, end: u64, t: u64| {
            if t >= end || end == start {
                to
            } else {
                from.lerp(to, (t - start) as f64 / (end - start) as f64)
            }
        };
        let close = |segments: &mut Vec<Segment>,
                     from: State,
                     to: State,
                     start: u64,
                     end: u64,
                     last: u64,
                     t: u64| {
            // The ramp's share of [last, t), then the hold after it.
            if end > last {
                let stop = end.min(t);
                if stop > last {
                    segments.push(Segment {
                        start: last,
                        end: stop,
                        from: value(from, to, start, end, last),
                        to: value(from, to, start, end, stop),
                    });
                }
            }
            let hold = end.max(last);
            if t > hold {
                let at = value(from, to, start, end, hold);
                segments.push(Segment {
                    start: hold,
                    end: t,
                    from: at,
                    to: at,
                });
            }
        };
        for keyframe in &keyframes {
            let t = keyframe.sample_pos;
            close(&mut segments, from, to, ramp_start, ramp_end, last, t);
            let now = value(from, to, ramp_start, ramp_end, t);
            let target = State {
                position: keyframe.position,
                gain: keyframe.gain,
            };
            from = if keyframe.ramp_samples == 0 {
                target
            } else {
                now
            };
            to = target;
            ramp_start = t;
            ramp_end = t + u64::from(keyframe.ramp_samples);
            last = last.max(t);
        }
        close(
            &mut segments,
            from,
            to,
            ramp_start,
            ramp_end,
            last,
            ramp_end.max(last),
        );
        segments.push(Segment {
            start: ramp_end.max(last),
            end: u64::MAX,
            from: to,
            to,
        });
        segments.retain(|s| s.end > s.start);
        Self { segments }
    }

    /// The segment holding sample `t`, searched from `hint` onwards: the
    /// callers walk forwards.
    fn segment(&self, t: u64, hint: &mut usize) -> &Segment {
        if self.segments[*hint].start > t {
            *hint = 0;
        }
        while self.segments[*hint].end <= t {
            *hint += 1;
        }
        &self.segments[*hint]
    }

    /// Where the object starts.
    fn origin(&self) -> [f64; 3] {
        self.segments[0].from.position
    }

    /// The subblocks of `[a, b)`, sample times that may start before the
    /// programme does — the codec's delay puts the first unit's start there,
    /// and the object is where it begins until the programme does.
    fn subblocks(&self, a: i64, b: i64, hint: &mut usize, out: &mut Vec<Subblock>) {
        out.clear();
        let mut t = a;
        while t < b {
            let (end, from, to) = if t < 0 {
                let p = self.origin();
                (b.min(0), p, p)
            } else {
                let segment = *self.segment(t as u64, hint);
                let end = if segment.end == u64::MAX {
                    b
                } else {
                    b.min(segment.end as i64)
                };
                let from = segment.at(t as u64).position;
                let to = segment.at(end as u64).position;
                (end, from, to)
            };
            let duration = (end - t) as u32;
            let animation = if from == to {
                Animation::Step(from)
            } else {
                Animation::Linear(from, to)
            };
            // A standstill that follows one at the same place is the same
            // subblock, longer.
            match (out.last_mut(), animation) {
                (Some(last), Animation::Step(p)) if last.animation == Animation::Step(p) => {
                    last.duration += duration;
                }
                _ => out.push(Subblock {
                    duration,
                    animation,
                }),
            }
            t = end;
        }
    }

    /// When the object, standing at `position` at sample `t`, next moves:
    /// `None` if it never does.
    fn still_until(&self, t: i64, position: [f64; 3], hint: &mut usize) -> Option<u64> {
        self.segment(t.max(0) as u64, hint);
        let mut i = *hint;
        while let Some(segment) = self.segments.get(i) {
            if !segment.still() || segment.from.position != position {
                return Some(segment.start);
            }
            if segment.end == u64::MAX {
                return None;
            }
            i += 1;
        }
        None
    }
}

/// What `HZ_IAMF_TRACE` writes: see the module note.
struct Trace {
    dir: std::path::PathBuf,
    positions: std::io::BufWriter<std::fs::File>,
    samples: Vec<std::io::BufWriter<std::fs::File>>,
}

impl Trace {
    /// A trace into `$HZ_IAMF_TRACE`, if it is set.
    fn open(elements: usize) -> Result<Option<Self>> {
        let Some(dir) = std::env::var_os("HZ_IAMF_TRACE") else {
            return Ok(None);
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        let file = |name: String| {
            let at = dir.join(name);
            std::fs::File::create(&at)
                .map(std::io::BufWriter::new)
                .map_err(|e| Error::io(&at, e))
        };
        let mut positions = file("positions.csv".into())?;
        writeln!(positions, "unit,offset,element,x,y,z").map_err(|e| Error::io(&dir, e))?;
        let samples = (0..elements)
            .map(|e| file(format!("element-{e}.f32")))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(Self {
            dir,
            positions,
            samples,
        }))
    }

    fn position(&mut self, unit: u64, offset: usize, element: usize, p: [f64; 3]) -> Result<()> {
        writeln!(
            self.positions,
            "{unit},{offset},{element},{},{},{}",
            p[0], p[1], p[2]
        )
        .map_err(|e| Error::io(&self.dir, e))
    }

    /// One chunk of every element's samples, interleaved `width` a frame.
    fn samples(&mut self, input: &[f32], width: usize) -> Result<()> {
        for (e, file) in self.samples.iter_mut().enumerate() {
            for frame in input.chunks_exact(width) {
                file.write_all(&frame[e].to_le_bytes())
                    .map_err(|err| Error::io(&self.dir, err))?;
            }
        }
        Ok(())
    }
}

/// One element of the sequence, as the encode feeds it.
struct Carried {
    /// Its channel in an interleaved input frame.
    input: usize,
    /// How it moves and how loud it is; `None` for the LFE and the bed
    /// channels, which stand still at unity.
    path: Option<Path>,
    /// Where `path`'s search last stopped, for the audio and for the blocks.
    audio_hint: usize,
    block_hint: usize,
    measure_hint: usize,
    /// Units already covered by a block written earlier.
    covered_until: u64,
    /// When the measurement's renderer next has to be told something.
    next_update: u64,
}

pub fn run(config: Config) -> Result<()> {
    let kind = config.objects.expect("the objects mode");
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

    // The master's tracks, sorted into what each becomes.
    let mut lfe = None;
    let mut beds = Vec::new();
    let mut objects = Vec::new();
    let described = source.adm();
    for track in tracks(path, described, stride)? {
        match track.format.type_definition {
            TypeDefinition::DirectSpeakers => {
                let label = track.speaker_label();
                let label = speakers::by_master_name(label).map_or(label, |s| s.label);
                if speakers::is_lfe_label(label) {
                    if lfe.replace(track.source_channel).is_some() {
                        return Err(Error::unsupported(
                            path,
                            "two LFE channels; IAMF's LFE element carries one",
                        ));
                    }
                } else {
                    let position = hz_render::fold::bed_position(label).ok_or_else(|| {
                        Error::unsupported(
                            path,
                            format!(
                                "track {} is the bed channel `{label}`, which has no place in \
                                 the room to put it at",
                                track.number
                            ),
                        )
                    })?;
                    beds.push((track.source_channel, position));
                }
            }
            TypeDefinition::Objects => objects.push((
                track.source_channel,
                Path::new(&keyframes_of(track.format, described.sample_rate)),
            )),
            other => eprintln!(
                "note: track {} is `{other}` audio, which IAMF objects do not take; left out",
                track.number
            ),
        }
    }
    if lfe.is_none() && beds.is_empty() && objects.is_empty() {
        return Err(Error::unsupported(path, "nothing in it to carry"));
    }
    // How many object elements there may be: what is asked, or what is left
    // of the budget beside the LFE.
    let room = BUDGET - usize::from(lfe.is_some());
    let allowed = config.elements.unwrap_or(room);
    if allowed > room || allowed < hz_cluster::MIN_ELEMENTS {
        return Err(Error::unsupported(
            path,
            format!(
                "{allowed} object elements; an IA sequence carries {} to {room}{}",
                hz_cluster::MIN_ELEMENTS,
                if lfe.is_some() { " beside the LFE" } else { "" }
            ),
        ));
    }
    if beds.len() + objects.len() > allowed {
        if beds.len() >= allowed {
            return Err(Error::unsupported(
                path,
                format!(
                    "{} bed channels and {allowed} elements; a bed channel keeps an element of \
                     its own, and the objects need at least one",
                    beds.len()
                ),
            ));
        }
        return run_clustered(Clustered {
            config: &config,
            kind,
            source,
            input_scale,
            lfe,
            beds,
            objects,
            elements: allowed,
        });
    }
    let channels = usize::from(lfe.is_some()) + beds.len() + objects.len();

    // The elements: the LFE first, then the bed channels, then the objects.
    let mut elements = Vec::with_capacity(channels);
    let mut carried = Vec::with_capacity(channels);
    let carry = |input, path| Carried {
        input,
        path,
        audio_hint: 0,
        block_hint: 0,
        measure_hint: 0,
        covered_until: 0,
        next_update: 0,
    };
    if let Some(input) = lfe {
        elements.push(Element::Channels(hz_iamf::layout::LFE));
        carried.push(carry(input, None));
    }
    for &(input, position) in &beds {
        elements.push(Element::Object {
            kind,
            default: position,
        });
        carried.push(carry(input, None));
    }
    for (input, object_path) in objects {
        elements.push(Element::Object {
            kind,
            default: object_path.origin(),
        });
        carried.push(carry(input, Some(object_path)));
    }
    let mut writer = iamf::open(&config, sample_rate, elements.clone())?;
    let delay = writer.delay() as i64;

    // The measurement renders what a decoder is handed, on 7.1.4: the LFE
    // to its speaker, every object on the room's cube.
    let layout = Layout::surround_7_1_4();
    let mut mixdown = Mixdown::new(&layout)?;
    for (index, element) in elements.iter().enumerate() {
        match element {
            Element::Channels(_) => {
                mixdown.speaker(index, layout.index_of("LFE1").expect("7.1.4 has an LFE"));
            }
            Element::Object { default, .. } => {
                let source = mixdown.object(index);
                mixdown.update(
                    source,
                    &Keyframe {
                        position: *default,
                        ..Keyframe::default()
                    },
                );
            }
        }
    }
    let mut measure = Measure::new(sample_rate);

    let frame = config.frame;
    let width = elements.len();
    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    // Units on the sequence's timeline, the codec's delay and its flush
    // included: what a standstill to the end has to cover.
    let units_total = (total + delay as u64).div_ceil(frame as u64) + 1;
    let mut progress = Progress::new(config.progress, total);

    let mut raw = vec![0i32; frame * stride];
    let mut quantised = vec![0i32; frame * width];
    let mut input = vec![0f32; frame * width];
    let mut rendered = vec![0f64; frame * layout.channels()];
    let mut measured = vec![0f32; frame * layout.channels()];
    let full_scale = f64::from(1u32 << (config.bits - 1));
    let (low, high) = (-full_scale, full_scale - 1.0);
    let (mut clipped, mut peak) = (0u64, 0.0f64);

    let mut subblocks: Vec<Vec<Subblock>> = vec![Vec::new(); width];
    let mut trace = Trace::open(width)?;
    let (mut blocks_written, mut moves) = (0u64, 0u64);

    let mut at = 0u64;
    let mut unit = 0u64;
    while at < total {
        let want = (total - at).min(frame as u64) as usize;
        let got = iamf::fill(&mut source, &mut raw, want, stride)?;
        if got == 0 {
            break;
        }
        let last = at + got as u64 >= total || got < frame;

        // Each element's samples, its gain applied, in the codec's integers.
        for (e, element) in carried.iter_mut().enumerate() {
            for i in 0..got {
                let gain = match &element.path {
                    Some(p) => {
                        p.segment(at + i as u64, &mut element.audio_hint)
                            .at(at + i as u64)
                            .gain
                    }
                    None => 1.0,
                };
                let x = f64::from(raw[i * stride + element.input]) * input_scale * gain;
                peak = peak.max(x.abs());
                let scaled = (x * full_scale).round();
                if scaled < low || scaled > high {
                    clipped += 1;
                }
                let q = scaled.clamp(low, high);
                quantised[i * width + e] = q as i32;
                input[i * width + e] = (q / full_scale) as f32;
            }
        }

        // The blocks that start with this unit, on the sequence's timeline:
        // the unit holds the master's samples from `at - delay` on.
        let (a, b) = (
            unit as i64 * frame as i64 - delay,
            (unit as i64 + 1) * frame as i64 - delay,
        );
        let mut stated = Vec::new();
        for (e, element) in carried.iter_mut().enumerate() {
            let Some(object_path) = &element.path else {
                continue;
            };
            if element.covered_until > unit {
                continue;
            }
            let out = &mut subblocks[e];
            object_path.subblocks(a, b, &mut element.block_hint, out);
            if let [
                Subblock {
                    animation: Animation::Step(p),
                    ..
                },
            ] = out.as_slice()
            {
                // Standing still: one block for as long as it does, in whole
                // units, to the end if it never moves again.
                let p = *p;
                let units = match object_path.still_until(a, p, &mut element.block_hint) {
                    None => units_total.saturating_sub(unit).max(1),
                    Some(next) => ((next as i64 - a) / frame as i64).max(1) as u64,
                };
                out[0].duration = (units * frame as u64) as u32;
                element.covered_until = unit + units;
            } else {
                moves += 1;
                element.covered_until = unit + 1;
                if last {
                    // The codec's flush comes after the last unit, and a unit
                    // no block covers would put the object at its default.
                    let end = match out.last().map(|s| s.animation) {
                        Some(Animation::Step(p) | Animation::Linear(_, p)) => p,
                        None => object_path.origin(),
                    };
                    out.push(Subblock {
                        duration: (2 * frame) as u32,
                        animation: Animation::Step(end),
                    });
                }
            }
            stated.push(e);
        }
        let blocks: Vec<PositionBlock> = stated
            .iter()
            .map(|&e| PositionBlock {
                element: e,
                subblocks: &subblocks[e],
            })
            .collect();
        blocks_written += blocks.len() as u64;
        if let Some(trace) = trace.as_mut() {
            for (e, element) in carried.iter_mut().enumerate() {
                let Element::Object { default, .. } = elements[e] else {
                    continue;
                };
                for offset in (0..frame).step_by(TRACE_INTERVAL) {
                    let t = a + offset as i64;
                    let p = match &element.path {
                        None => default,
                        Some(path) if t < 0 => path.origin(),
                        Some(path) => {
                            let mut hint = 0;
                            path.segment(t as u64, &mut hint).at(t as u64).position
                        }
                    };
                    trace.position(unit, offset, e, p)?;
                }
            }
            trace.samples(&input[..got * width], width)?;
        }
        writer
            .push(&quantised[..got * width], &blocks)
            .map_err(|e| Error::io(&config.out, e))?;

        // And measured as a decoder renders it: split where any object's path
        // breaks, its renderer told where the object is going and how long it
        // takes to get there.
        rendered[..got * layout.channels()].fill(0.0);
        let mut done = 0usize;
        while done < got {
            let now = at + done as u64;
            let mut next = at + got as u64;
            for (e, element) in carried.iter_mut().enumerate() {
                let Some(object_path) = &element.path else {
                    continue;
                };
                if element.next_update <= now {
                    let segment = *object_path.segment(now, &mut element.measure_hint);
                    let (target, ramp) = if segment.still() || segment.end == u64::MAX {
                        (segment.from.position, 0)
                    } else {
                        (segment.to.position, (segment.end - now) as u32)
                    };
                    mixdown.update(
                        e,
                        &Keyframe {
                            position: target,
                            ramp_samples: ramp,
                            ..Keyframe::default()
                        },
                    );
                    element.next_update = segment.end;
                }
                next = next.min(element.next_update);
            }
            let end = (next - at) as usize;
            mixdown.render(
                &input[done * width..got * width],
                width,
                end - done,
                &mut rendered[done * layout.channels()..got * layout.channels()],
            );
            done = end;
        }
        for (m, &r) in measured
            .iter_mut()
            .zip(&rendered[..got * layout.channels()])
        {
            *m = r as f32;
        }
        measure.push(&measured[..got * layout.channels()]);

        at += got as u64;
        unit += 1;
        if let Some(progress) = progress.as_mut() {
            progress.at(at);
        }
        if last {
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

    let moving = carried.iter().filter(|c| c.path.is_some()).count();
    let mut parts = vec![format!(
        "{moving} object{}",
        if moving == 1 { "" } else { "s" }
    )];
    if !beds.is_empty() {
        parts.push(format!(
            "{} bed channel{} as still objects",
            beds.len(),
            if beds.len() == 1 { "" } else { "s" }
        ));
    }
    if lfe.is_some() {
        parts.push("an LFE element".into());
    }
    let profile = match (elements.len(), lfe.is_some()) {
        (n, _) if n > 18 => "advanced-2",
        (_, true) => "advanced-1",
        _ => "base-advanced",
    };
    println!(
        "  programme    {}: {} elements, IAMF v2.0 {profile}",
        parts.join(", "),
        elements.len()
    );
    println!(
        "  encoded      {units} temporal units of {frame}, {at} samples, {}",
        iamf::coding(&config)
    );
    println!(
        "  positions    {}, {blocks_written} parameter blocks, {moves} of them for a unit an object moved in",
        match kind {
            PositionKind::Cart16 => "Cartesian, 16 bits",
            PositionKind::Cart8 => "Cartesian, 8 bits",
            PositionKind::Polar => "polar",
        }
    );
    iamf::report_loudness(loudness);
    println!(
        "  headroom     {clipped} samples clipped, the loudest object at {peak:.2} of full scale \
         with its gain"
    );
    if clipped > 0 {
        eprintln!("warning: {clipped} samples clipped; an object's gain took it past full scale");
    }
    iamf::report_written(&config, at, sample_rate)
}

/// What a clustering encode is handed: everything the master was sorted
/// into, and how many object elements it may use.
struct Clustered<'a> {
    config: &'a Config,
    kind: PositionKind,
    source: Source,
    input_scale: f64,
    lfe: Option<usize>,
    beds: Vec<(usize, [f64; 3])>,
    objects: Vec<(usize, Path)>,
    elements: usize,
}

/// Samples in one clustering block: the most that divides the unit and is
/// no longer than the 1280 the TrueHD encode decides on (26.7 ms) — long
/// enough that a block's power is an estimate rather than noise, short
/// enough that an element's ramp still tracks what it carries — so that a
/// block never straddles two units.
fn cluster_block(frame: usize) -> usize {
    let mut blocks = frame.div_ceil(1280).max(1);
    while !frame.is_multiple_of(blocks) {
        blocks += 1;
    }
    frame / blocks
}

/// The subblocks of `[a, b)` for an element whose position at the end of
/// block `j` is `history[j]`, ramping there across the block from where the
/// block before left it — the path a decoder is given and the one the
/// weights take. Before the programme it is where it starts, and past the
/// last block — the padding of the last unit — where it ended.
fn cluster_subblocks(history: &[[f64; 3]], block: usize, a: i64, b: i64, out: &mut Vec<Subblock>) {
    out.clear();
    let held = (history.len() * block) as i64;
    let last = history[history.len() - 1];
    let at = |t: i64, j: usize| -> [f64; 3] {
        let to = history[j];
        let from = history[j.saturating_sub(1)];
        let into = (t - (j * block) as i64) as f64 / block as f64;
        [0, 1, 2].map(|i| from[i] + (to[i] - from[i]) * into)
    };
    let mut t = a;
    while t < b {
        let (end, from, to) = if t < 0 {
            (b.min(0), history[0], history[0])
        } else if t >= held {
            (b, last, last)
        } else {
            let j = t as usize / block;
            let end = b.min(((j + 1) * block) as i64);
            (end, at(t, j), at(end, j))
        };
        let duration = (end - t) as u32;
        let animation = if from == to {
            Animation::Step(from)
        } else {
            Animation::Linear(from, to)
        };
        match (out.last_mut(), animation) {
            (Some(last), Animation::Step(p)) if last.animation == Animation::Step(p) => {
                last.duration += duration;
            }
            _ => out.push(Subblock {
                duration,
                animation,
            }),
        }
        t = end;
    }
}

/// More objects than elements: fold them into the elements block by block
/// with `hz-cluster` — where each element goes, and how much of each object
/// it carries — and carry the elements as the objects.
///
/// The same fold the TrueHD encode makes, from the same crate, less one
/// thing: it places each block's elements on that block's own energies, with
/// none of the look-ahead the TrueHD encode smooths them with.
fn run_clustered(job: Clustered<'_>) -> Result<()> {
    use hz_cluster::scene::{Loudness as Weighing, Scene, Source as SceneSource};
    use hz_cluster::{Clusterer, Clustering, Weighting};

    let Clustered {
        config,
        kind,
        mut source,
        input_scale,
        lfe,
        beds,
        mut objects,
        elements: count,
    } = job;
    let path = config.input.as_path();
    let sample_rate = source.sample_rate();
    let stride = source.channels();
    let first = usize::from(lfe.is_some());

    let mut elements = Vec::with_capacity(first + count);
    if lfe.is_some() {
        elements.push(Element::Channels(hz_iamf::layout::LFE));
    }
    // Where an element is before the first block says: in front, where no
    // gap in its blocks will ever let a decoder find it.
    elements.extend(std::iter::repeat_n(
        Element::Object {
            kind,
            default: [0.0, 1.0, 0.0],
        },
        count,
    ));
    let mut writer = iamf::open(config, sample_rate, elements.clone())?;
    let delay = writer.delay() as i64;

    let fail = |why: hz_core::Error| Error::unsupported(path, why.to_string());
    let mut scene = Scene::weighed(f64::from(sample_rate), Weighing::Flat).map_err(fail)?;
    let mut clusterer = Clusterer::new(count, Weighting::Fitted)
        .map_err(fail)?
        .flooring(hz_cluster::floor::Floor::for_dialnorm(-31.0));
    let mut limiter = hz_cluster::mix::Limiter::new();
    let renderers = hz_cluster::metric::delivery().map_err(fail)?;
    let floor = hz_cluster::floor::Floor::for_dialnorm(-31.0);

    // The sources the fold takes: the bed channels pinned where they stand,
    // then the objects.
    let sources = beds.len() + objects.len();
    let mut signals: Vec<Vec<f32>> = vec![Vec::new(); sources];
    let ones = vec![1.0; sources];
    let mut mixed: Vec<Vec<f32>> = vec![Vec::new(); count];
    let mut from: Vec<Vec<f64>> = Vec::new();
    let mut previous: Option<Clustering> = None;
    let mut hints = vec![0usize; objects.len()];
    let mut history: Vec<Vec<[f64; 3]>> = vec![Vec::new(); count];

    let layout = Layout::surround_7_1_4();
    let mut mixdown = Mixdown::new(&layout)?;
    if lfe.is_some() {
        mixdown.speaker(0, layout.index_of("LFE1").expect("7.1.4 has an LFE"));
    }
    for element in 0..count {
        mixdown.object(first + element);
    }
    let mut measure = Measure::new(sample_rate);
    let mut trace = Trace::open(first + count)?;

    let frame = config.frame;
    let block = cluster_block(frame);
    let width = first + count;
    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    let mut progress = Progress::new(config.progress, total);
    let mut raw = vec![0i32; frame * stride];
    let mut quantised = vec![0i32; frame * width];
    let mut input = vec![0f32; frame * width];
    let mut rendered = vec![0f64; frame * layout.channels()];
    let mut measured = vec![0f32; frame * layout.channels()];
    let full_scale = f64::from(1u32 << (config.bits - 1));
    let (low, high) = (-full_scale, full_scale - 1.0);
    let ceiling = high / full_scale;
    let (mut clipped, mut limited) = (0u64, 0u64);
    let (mut error_sum, mut error_worst, mut judged) = (0.0f64, 0.0f64, 0u64);
    let mut subblocks: Vec<Vec<Subblock>> = vec![Vec::new(); count];
    let mut blocks_written = 0u64;

    let mut at = 0u64;
    let mut unit = 0u64;
    while at < total {
        let want = (total - at).min(frame as u64) as usize;
        let got = iamf::fill(&mut source, &mut raw, want, stride)?;
        if got == 0 {
            break;
        }
        let last = at + got as u64 >= total || got < frame;
        rendered[..got * layout.channels()].fill(0.0);

        let mut start = 0;
        while start < got {
            let len = block.min(got - start);
            let t0 = at + start as u64;
            // The scene of this block: every source's samples with its gain
            // on them, and where it is at the block's end.
            scene.start();
            for (i, &(channel, position)) in beds.iter().enumerate() {
                let signal = &mut signals[i];
                signal.clear();
                signal.extend((0..len).map(|n| {
                    (f64::from(raw[(start + n) * stride + channel]) * input_scale) as f32
                }));
                scene.push(
                    &SceneSource {
                        position,
                        pinned: Some(position),
                        mode: hz_cluster::class::BED,
                        ..SceneSource::default()
                    },
                    signal.iter().map(|&x| f64::from(x)),
                );
            }
            for (o, (channel, object_path)) in objects.iter_mut().enumerate() {
                let signal = &mut signals[beds.len() + o];
                signal.clear();
                let hint = &mut hints[o];
                signal.extend((0..len).map(|n| {
                    let t = t0 + n as u64;
                    let gain = object_path.segment(t, hint).at(t).gain;
                    (f64::from(raw[(start + n) * stride + *channel]) * input_scale * gain) as f32
                }));
                let end = t0 + len as u64 - 1;
                let position = object_path.segment(end, hint).at(end).position;
                scene.push(
                    &SceneSource {
                        position,
                        ..SceneSource::default()
                    },
                    signal.iter().map(|&x| f64::from(x)),
                );
            }
            scene.finish();

            let clustering = clusterer
                .cluster(scene.objects(), previous.as_ref())
                .map_err(fail)?;
            from.clear();
            match &previous {
                Some(before) if before.weights.len() == clustering.weights.len() => {
                    from.extend(before.weights.iter().cloned());
                }
                _ => from.extend(clustering.weights.iter().cloned()),
            }
            hz_cluster::mix::mix(&signals, &ones, &from, &clustering.weights, len, &mut mixed);
            if limiter.apply(&mut mixed, ceiling) {
                limited += 1;
            }
            if let Ok(report) =
                hz_cluster::metric::error_with(scene.objects(), &clustering, &renderers, &floor)
            {
                error_sum += report.mean;
                error_worst = error_worst.max(report.worst);
                judged += 1;
            }

            // Into the codec's integers; the LFE copied, not mixed.
            for n in 0..len {
                let frame_at = (start + n) * width;
                if let Some(channel) = lfe {
                    let x = f64::from(raw[(start + n) * stride + channel]) * input_scale;
                    let q = (x * full_scale).round().clamp(low, high);
                    quantised[frame_at] = q as i32;
                    input[frame_at] = (q / full_scale) as f32;
                }
                for (element, signal) in mixed.iter().enumerate() {
                    let scaled = (f64::from(signal[n]) * full_scale).round();
                    if scaled < low || scaled > high {
                        clipped += 1;
                    }
                    let q = scaled.clamp(low, high);
                    quantised[frame_at + first + element] = q as i32;
                    input[frame_at + first + element] = (q / full_scale) as f32;
                }
            }

            // Where each element is going, for the blocks and for the
            // measurement's renderer, which ramps there across the block.
            let fresh = previous.is_none();
            for (element, at_end) in clustering.positions.iter().enumerate() {
                history[element].push(*at_end);
                mixdown.update(
                    first + element,
                    &Keyframe {
                        position: *at_end,
                        ramp_samples: if fresh { 0 } else { len as u32 },
                        ..Keyframe::default()
                    },
                );
            }
            mixdown.render(
                &input[start * width..(start + len) * width],
                width,
                len,
                &mut rendered[start * layout.channels()..(start + len) * layout.channels()],
            );
            previous = Some(clustering);
            start += len;
        }

        // The unit's blocks on the sequence's timeline, as the objects mode
        // cuts them; with the codec's flush covered after the last one.
        let (a, b) = (
            unit as i64 * frame as i64 - delay,
            (unit as i64 + 1) * frame as i64 - delay,
        );
        for (element, out) in subblocks.iter_mut().enumerate() {
            cluster_subblocks(&history[element], block, a, b, out);
            if last {
                let end = *history[element].last().expect("a block was clustered");
                out.push(Subblock {
                    duration: (2 * frame) as u32,
                    animation: Animation::Step(end),
                });
            }
        }
        let blocks: Vec<PositionBlock> = (0..count)
            .map(|element| PositionBlock {
                element: first + element,
                subblocks: &subblocks[element],
            })
            .collect();
        blocks_written += blocks.len() as u64;
        if let Some(trace) = trace.as_mut() {
            let mut one = Vec::new();
            for (element, positions) in history.iter().enumerate() {
                for offset in (0..frame).step_by(TRACE_INTERVAL) {
                    let t = a + offset as i64;
                    cluster_subblocks(positions, block, t, t + 1, &mut one);
                    let p = match one[0].animation {
                        Animation::Step(p) | Animation::Linear(p, _) => p,
                    };
                    trace.position(unit, offset, first + element, p)?;
                }
            }
            trace.samples(&input[..got * width], width)?;
        }
        writer
            .push(&quantised[..got * width], &blocks)
            .map_err(|e| Error::io(&config.out, e))?;

        for (m, &r) in measured
            .iter_mut()
            .zip(&rendered[..got * layout.channels()])
        {
            *m = r as f32;
        }
        measure.push(&measured[..got * layout.channels()]);

        at += got as u64;
        unit += 1;
        if let Some(progress) = progress.as_mut() {
            progress.at(at);
        }
        if last {
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

    let profile = if width > 18 {
        "advanced-2"
    } else if lfe.is_some() {
        "advanced-1"
    } else {
        "base-advanced"
    };
    println!(
        "  programme    {} objects and {} bed channel{}{} into {count} elements{}: IAMF v2.0 {profile}",
        objects.len(),
        beds.len(),
        if beds.len() == 1 { "" } else { "s" },
        if beds.is_empty() { "" } else { " (pinned)" },
        if lfe.is_some() {
            " beside an LFE element"
        } else {
            ""
        }
    );
    println!(
        "  encoded      {units} temporal units of {frame}, {at} samples, {}",
        iamf::coding(config)
    );
    println!(
        "  fold         {:.4} mean over the presentations, {:.3} at its worst, as a fraction of \
         the object's own gains; blocks of {block}, no look-ahead",
        if judged > 0 {
            error_sum / judged as f64
        } else {
            0.0
        },
        error_worst
    );
    println!(
        "  positions    {}, {blocks_written} parameter blocks",
        match kind {
            PositionKind::Cart16 => "Cartesian, 16 bits",
            PositionKind::Cart8 => "Cartesian, 8 bits",
            PositionKind::Polar => "polar",
        }
    );
    iamf::report_loudness(loudness);
    println!(
        "  headroom     {limited} blocks where the limiter brought every element down, \
         {clipped} samples clipped"
    );
    if clipped > 0 {
        eprintln!("warning: {clipped} samples clipped");
    }
    iamf::report_written(config, at, sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyframe(sample_pos: u64, x: f64, gain: f64, ramp_samples: u32) -> Keyframe {
        Keyframe {
            sample_pos,
            position: [x, 1.0, 0.0],
            gain,
            ramp_samples,
            ..Keyframe::default()
        }
    }

    fn x_at(path: &Path, t: u64) -> f64 {
        path.segment(t, &mut 0).at(t).position[0]
    }

    /// Holds, ramps, a ramp cut short by the next update, and a jump.
    #[test]
    fn a_path_follows_the_updates() {
        let path = Path::new(&[
            keyframe(0, -1.0, 1.0, 0),
            keyframe(100, 1.0, 1.0, 200),
            // Cuts the ramp at its midpoint, x = 0, and heads back.
            keyframe(200, -1.0, 0.5, 100),
            keyframe(400, 0.5, 1.0, 0),
        ]);
        assert_eq!(x_at(&path, 50), -1.0);
        assert!((x_at(&path, 150) + 0.5).abs() < 1e-12);
        assert!(x_at(&path, 200).abs() < 1e-12);
        assert!((x_at(&path, 250) + 0.5).abs() < 1e-12);
        assert_eq!(x_at(&path, 300), -1.0);
        assert_eq!(x_at(&path, 399), -1.0);
        assert_eq!(x_at(&path, 400), 0.5);
        assert_eq!(x_at(&path, 1_000_000), 0.5);
        let gain = |t| path.segment(t, &mut 0).at(t).gain;
        assert!((gain(250) - 0.75).abs() < 1e-12);
    }

    /// An object is silent before its first update.
    #[test]
    fn an_object_is_silent_until_it_starts() {
        let path = Path::new(&[keyframe(480, 0.3, 1.0, 0)]);
        assert_eq!(path.segment(0, &mut 0).at(0).gain, 0.0);
        assert_eq!(path.segment(480, &mut 0).at(480).gain, 1.0);
        assert_eq!(path.origin(), [0.3, 1.0, 0.0]);
    }

    /// A unit's subblocks break where the path does, a standstill is one
    /// step, and the time before the programme holds the origin.
    #[test]
    fn subblocks_break_where_the_path_does() {
        let path = Path::new(&[keyframe(0, -1.0, 1.0, 0), keyframe(300, 1.0, 1.0, 400)]);
        let mut out = Vec::new();
        path.subblocks(-312, 648, &mut 0, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].duration, 612);
        assert_eq!(out[0].animation, Animation::Step([-1.0, 1.0, 0.0]));
        assert_eq!(out[1].duration, 348);
        let Animation::Linear(from, to) = out[1].animation else {
            panic!("a move");
        };
        assert_eq!(from[0], -1.0);
        assert!((to[0] - (-1.0 + 2.0 * 348.0 / 400.0)).abs() < 1e-12);
        let total: u32 = out.iter().map(|s| s.duration).sum();
        assert_eq!(total, 960);
    }

    #[test]
    fn a_standstill_knows_when_it_ends() {
        let path = Path::new(&[keyframe(0, -1.0, 1.0, 0), keyframe(5000, 1.0, 1.0, 0)]);
        assert_eq!(path.still_until(0, [-1.0, 1.0, 0.0], &mut 0), Some(5000));
        assert_eq!(path.still_until(6000, [1.0, 1.0, 0.0], &mut 0), None);
    }

    #[test]
    fn a_cluster_block_divides_the_unit() {
        assert_eq!(cluster_block(4096), 1024);
        assert_eq!(cluster_block(960), 960);
        assert_eq!(cluster_block(1920), 960);
        assert_eq!(cluster_block(2880), 960);
        assert_eq!(cluster_block(4608), 1152);
    }

    /// An element ramps across each block to where the block put it.
    #[test]
    fn an_element_ramps_to_each_block_end() {
        let history = [[-1.0, 1.0, 0.0], [1.0, 1.0, 0.0], [1.0, 1.0, 0.0]];
        let mut out = Vec::new();
        cluster_subblocks(&history, 100, -50, 300, &mut out);
        // Before the programme and the first block: where it starts.
        assert_eq!(out[0].duration, 150);
        assert_eq!(out[0].animation, Animation::Step([-1.0, 1.0, 0.0]));
        // Across the second block, to the right.
        assert_eq!(out[1].duration, 100);
        assert_eq!(
            out[1].animation,
            Animation::Linear([-1.0, 1.0, 0.0], [1.0, 1.0, 0.0])
        );
        // And still across the third.
        assert_eq!(out[2].animation, Animation::Step([1.0, 1.0, 0.0]));
        assert_eq!(out.iter().map(|s| s.duration).sum::<u32>(), 350);
    }
}
