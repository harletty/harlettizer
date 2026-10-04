//! `harlettizer iamf --objects`: a programme's objects carried as IAMF v2.0
//! objects.
//!
//! Each object of the master becomes an element of its own — one mono
//! substream, positioned by a parameter its mix presentation declares — and
//! its path through the room becomes that parameter's blocks. The master's
//! bed travels beside them as one channel-based element: every bed channel,
//! the LFE among them, on the smallest IAMF loudspeaker layout that has them
//! all (see [`BedPlan`]). A bed channel no IAMF layout has — a top side, a
//! wide — is carried as an object that never leaves its speaker.
//!
//! Three ways to fill the elements:
//!
//! - **as they are**, each object its own element, folded block by block
//!   with `hz-cluster` when there are more objects than IAMF carries;
//! - **`--overlay K`**: the master's elements kept and its last K objects
//!   panned onto them — the computation of [`crate::overlay`], the one
//!   `encode --overlay` writes into a TrueHD stream;
//! - **`--voices-to-bed K`**: the master's last K objects rendered into the
//!   bed element on the room's cube, the rest carried as objects.
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
//! decoder to hand back: `positions.csv`, every object element's position
//! every 256 samples of every unit on the sequence's timeline, and
//! `element-<n>.f32`, the samples of channel `n` of a pushed frame as coded —
//! the bed element's channels first, in its layout's order, then one per
//! object element. A decoder's output is checked against them.
//!
//! # What does not survive
//!
//! An object's size, and its rendering mode — snapping, zone exclusions,
//! screen scaling. IAMF v2.0 positions a point and leaves the rest to the
//! renderer.

use crate::encode::Progress;
use crate::iamf::{self, Config, Measure, Output};
use crate::overlay::{self, Overlaid};
use crate::source::{Source, keyframes_of, tracks};
use hz_cluster::scene::{Scene, Source as SceneSource};
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

/// Under this an element's gain is silence — a hundred decibels down — and
/// a decoder playing it plays nothing a source could be heard through.
const MUTED: f64 = 1e-5;

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
    /// if it does — to where it asks, over the samples it asks for.
    ///
    /// Before its first update it is where and as loud as that update says,
    /// which is what `encode` seeds an element with: a master whose first
    /// update comes a few samples in still has audio before it, and a path
    /// that was silent until then dropped those samples from every object —
    /// near-silence on the masters seen, 171 samples of it, but not the
    /// master's.
    fn new(keyframes: &[Keyframe]) -> Self {
        let mut keyframes = keyframes.to_vec();
        keyframes.sort_by_key(|k| k.sample_pos);
        let mut from = keyframes.first().map_or(
            State {
                position: [0.0, 1.0, 0.0],
                gain: 0.0,
            },
            |k| State {
                position: k.position,
                gain: k.gain,
            },
        );
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

    /// The gain at sample `t`.
    fn gain(&self, t: u64, hint: &mut usize) -> f64 {
        self.segment(t, hint).at(t).gain
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
    /// A trace into `$HZ_IAMF_TRACE`, if it is set: one sample file for each
    /// of `channels`.
    fn open(channels: usize) -> Result<Option<Self>> {
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
        let samples = (0..channels)
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

    /// One chunk of every channel's samples, interleaved `width` a frame.
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

/// One track of the master, sorted by what it becomes.
enum Part {
    /// The low frequency channel.
    Lfe { channel: usize },
    /// A bed channel other than the LFE: a speaker feed, `name` the master's
    /// own name for its channel and `position` its speaker's place.
    Bed {
        channel: usize,
        name: &'static str,
        position: [f64; 3],
    },
    /// An object and its updates.
    Object {
        channel: usize,
        keyframes: Vec<Keyframe>,
    },
}

impl Part {
    fn channel(&self) -> usize {
        match self {
            Self::Lfe { channel } | Self::Bed { channel, .. } | Self::Object { channel, .. } => {
                *channel
            }
        }
    }
}

/// The master's tracks, the LFE first and the rest in track order — the
/// order `encode` numbers a programme's elements in, which an overlay's
/// arithmetic depends on.
fn parts_of(path: &std::path::Path, source: &Source) -> Result<Vec<Part>> {
    let described = source.adm();
    let mut lfe = None;
    let mut rest = Vec::new();
    for track in tracks(path, described, source.channels())? {
        match track.format.type_definition {
            TypeDefinition::DirectSpeakers => {
                let label = track.speaker_label();
                let name = speakers::master_name_for_label(label)
                    .or_else(|| speakers::by_master_name(label).map(|s| s.master));
                if speakers::is_lfe_label(label) {
                    if lfe.replace(track.source_channel).is_some() {
                        return Err(Error::unsupported(
                            path,
                            "two LFE channels; an IAMF layout carries one",
                        ));
                    }
                    continue;
                }
                let position = name
                    .and_then(hz_render::fold::bed_position)
                    .ok_or_else(|| {
                        Error::unsupported(
                            path,
                            format!(
                                "track {} is the bed channel `{label}`, which has no place in the \
                             room to put it at",
                                track.number
                            ),
                        )
                    })?;
                rest.push(Part::Bed {
                    channel: track.source_channel,
                    name: name.expect("a channel with a place has a name"),
                    position,
                });
            }
            TypeDefinition::Objects => rest.push(Part::Object {
                channel: track.source_channel,
                keyframes: keyframes_of(track.format, described.sample_rate),
            }),
            other => eprintln!(
                "note: track {} is `{other}` audio, which IAMF objects do not take; left out",
                track.number
            ),
        }
    }
    let mut parts = Vec::with_capacity(rest.len() + 1);
    parts.extend(lfe.map(|channel| Part::Lfe { channel }));
    parts.extend(rest);
    Ok(parts)
}

/// The master's own name for a channel an IAMF layout labels `label`, which
/// is how a layout's channel and a master's are matched: one channel may be
/// spelt two ways — a 5.1 surround at `M+110` is the side surround a 7.1
/// calls `M+090`.
fn name_of(label: &str) -> Option<&'static str> {
    speakers::master_name_for_label(label)
}

/// The label this workspace's room and meters know a layout's channel by.
fn canonical(label: &str) -> &'static str {
    name_of(label)
        .and_then(speakers::by_master_name)
        .map_or("LFE1", |speaker| speaker.label)
}

/// The bed element: which loudspeaker layout, which master channel feeds
/// which of its channels, and which bed channels it has no channel for.
///
/// # The rule
///
/// **The smallest IAMF layout that has every bed channel any IAMF layout
/// has** — the LFE alone as expanded layout 0, anything else the fewest
/// channels among IAMF's loudspeaker layouts 0 to 8 (mono to 7.1.4, with
/// 3.1.2), the lower code first between two of one width; a channel the
/// master lacks is silent. A bed channel none of those layouts has — a top
/// side, a wide, a second LFE — is carried as an object that stands at its
/// speaker, as the bed always was before there was a bed element.
///
/// The layouts IAMF also has and these leave out — the expanded side, rear
/// and height pairs, 3.0, 9.1.6 — are left out because no decoder to hand
/// renders them, and a bed nobody can play is not a bed.
#[derive(Debug, Clone)]
struct BedPlan {
    layout: hz_iamf::Layout,
    /// `(master channel, layout channel)`.
    routes: Vec<(usize, usize)>,
}

/// Whether a layout has a channel for the master's channel `name`.
fn slot_of(layout: &hz_iamf::Layout, name: &str) -> Option<usize> {
    layout
        .labels
        .iter()
        .position(|label| name_of(label) == Some(name))
}

/// The layout for a bed of these master channel names, as [`BedPlan`] says;
/// `None` when nothing in it has an IAMF channel.
fn layout_for(names: &[&str]) -> Option<hz_iamf::Layout> {
    let carried: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| {
            hz_iamf::layout::LOUDSPEAKER_LAYOUTS
                .iter()
                .any(|layout| slot_of(layout, name).is_some())
        })
        .collect();
    if carried.is_empty() {
        return None;
    }
    if carried.iter().all(|name| *name == "LFE") {
        return Some(hz_iamf::layout::LFE);
    }
    hz_iamf::layout::LOUDSPEAKER_LAYOUTS
        .iter()
        .find(|layout| carried.iter().all(|name| slot_of(layout, name).is_some()))
        .copied()
}

/// Whether a layout can take a voice: it has the front three a dialogue is
/// put on.
fn takes_voices(layout: &hz_iamf::Layout) -> bool {
    ["L", "R", "C"]
        .iter()
        .all(|name| slot_of(layout, name).is_some())
}

/// The bed element for `parts`, and the bed channels it has no channel for
/// (as indices into `parts`). `voices` asks for a layout that can take the
/// voices rendered into it: the master's own when it has a front left,
/// right and centre, 7.1.4 otherwise.
fn plan_bed(parts: &[Part], voices: bool) -> (Option<BedPlan>, Vec<usize>) {
    let names: Vec<(usize, &'static str)> = parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| match part {
            Part::Lfe { .. } => Some((index, "LFE")),
            Part::Bed { name, .. } => Some((index, *name)),
            Part::Object { .. } => None,
        })
        .collect();
    let mut layout = layout_for(&names.iter().map(|(_, name)| *name).collect::<Vec<_>>());
    if voices && !layout.as_ref().is_some_and(takes_voices) {
        layout = Some(hz_iamf::layout::SEVEN_ONE_FOUR);
    }
    let Some(layout) = layout else {
        return (None, names.iter().map(|(index, _)| *index).collect());
    };
    let mut routes = Vec::new();
    let mut left = Vec::new();
    for (index, name) in names {
        match slot_of(&layout, name) {
            Some(slot) => routes.push((parts[index].channel(), slot)),
            None => left.push(index),
        }
    }
    (Some(BedPlan { layout, routes }), left)
}

/// How an object element's position is stated.
enum Placement {
    /// It never moves: a bed channel at its speaker. Its definition's
    /// default is where it is, which costs no parameter block at all.
    Still,
    /// The master's path.
    Path(Box<Moving>),
    /// A fold's element, placed block by block.
    Folded(Folded),
}

/// An element on the master's path, and where each walk along it stopped.
struct Moving {
    path: Path,
    /// Where `path`'s search last stopped, for the audio, the blocks and the
    /// measurement.
    audio: usize,
    blocks: usize,
    measure: usize,
    trace: usize,
    /// Units already covered by a block written earlier.
    covered_until: u64,
}

/// A fold's element: where each block put it, ramping there across the
/// block from where the block before left it.
struct Folded {
    /// Samples a block.
    block: usize,
    /// The block `history[0]` is the end of.
    base: usize,
    history: Vec<[f64; 3]>,
}

impl Folded {
    /// Where it is at sample `t`, before the programme where it starts and
    /// past the last block where it ended.
    fn at(&self, t: i64, one: &mut Vec<Subblock>) -> [f64; 3] {
        cluster_subblocks(&self.history, self.base, self.block, t, t + 1, one);
        match one[0].animation {
            Animation::Step(p) | Animation::Linear(p, _) => p,
        }
    }

    /// Let go of the blocks no unit from `a` on will look at: the one `a`
    /// falls in, and the one before it, which its ramp starts from.
    fn forget_before(&mut self, a: i64) {
        let keep = (a.max(0) as usize / self.block).saturating_sub(1);
        let drop = keep
            .saturating_sub(self.base)
            .min(self.history.len().saturating_sub(1));
        // In runs, so that the shift is paid once in a while and not every
        // unit.
        if drop >= 256 {
            self.history.drain(..drop);
            self.base += drop;
        }
    }
}

/// One object element of the sequence.
struct Carried {
    /// Its channel in a pushed frame.
    channel: usize,
    /// The master channel its samples are, when they are one master
    /// channel's; `None` for a fold's element, which is a mix.
    input: Option<usize>,
    /// Where its definition puts it wherever no block says otherwise.
    default: [f64; 3],
    placement: Placement,
    /// When the measurement's renderer next has to be told something.
    next_update: u64,
}

impl Carried {
    fn still(channel: usize, input: usize, at: [f64; 3]) -> Self {
        Self {
            channel,
            input: Some(input),
            default: at,
            placement: Placement::Still,
            next_update: u64::MAX,
        }
    }

    fn moving(channel: usize, input: usize, path: Path) -> Self {
        Self {
            channel,
            input: Some(input),
            default: path.origin(),
            placement: Placement::Path(Box::new(Moving {
                path,
                audio: 0,
                blocks: 0,
                measure: 0,
                trace: 0,
                covered_until: 0,
            })),
            next_update: 0,
        }
    }

    fn folded(channel: usize, block: usize) -> Self {
        Self {
            channel,
            input: None,
            // In front, where no gap in its blocks will ever let a decoder
            // find it.
            default: [0.0, 1.0, 0.0],
            placement: Placement::Folded(Folded {
                block,
                base: 0,
                history: Vec::new(),
            }),
            next_update: 0,
        }
    }
}

/// What the overlay was set up from, kept for the summary.
struct OverlaySummary {
    sources: usize,
    beds_first: Option<f64>,
    fold_depth: u32,
}

/// What a run did, beyond what every run reports.
#[derive(Default)]
struct Account {
    clipped: u64,
    peak: f64,
    blocks_written: u64,
    moves: u64,
}

pub fn run(config: Config) -> Result<()> {
    let kind = config.objects.expect("the objects mode");
    let path = config.input.as_path();
    let source = Source::open(path, config.mono_prefix.as_deref())?;
    let format = *source.pcm_format();
    if format.sample_format == SampleFormat::Float {
        return Err(Error::unsupported(
            path,
            "floating-point audio; the master formats this reads are integer PCM",
        ));
    }
    let parts = parts_of(path, &source)?;
    if parts.is_empty() {
        return Err(Error::unsupported(path, "nothing in it to carry"));
    }
    match (config.overlay, config.voices_to_bed) {
        (Some(_), Some(_)) => Err(Error::unsupported(
            path,
            "`--overlay` and `--voices-to-bed` at once; one keeps the voices out of the bed and \
             the other renders them into it",
        )),
        (Some(sources), None) => run_overlay(&config, kind, source, parts, sources),
        (None, voices) => run_plain(&config, kind, source, parts, voices.unwrap_or(0)),
    }
}

/// The master as it is, or with its last `voices` objects rendered into the
/// bed element.
fn run_plain(
    config: &Config,
    kind: PositionKind,
    source: Source,
    parts: Vec<Part>,
    voices: usize,
) -> Result<()> {
    let path = config.input.as_path();
    let objects: Vec<usize> = (0..parts.len())
        .filter(|&index| matches!(parts[index], Part::Object { .. }))
        .collect();
    if config.voices_to_bed.is_some() {
        if voices == 0 {
            return Err(Error::unsupported(
                path,
                "`--voices-to-bed 0`; with no voices to render it is `--objects` alone",
            ));
        }
        if voices > objects.len() {
            return Err(Error::unsupported(
                path,
                format!(
                    "{voices} voices out of {} objects; the voices are the master's last objects",
                    objects.len()
                ),
            ));
        }
    }
    let (rendered, carried_objects) = objects.split_at(objects.len() - voices);
    let (rendered, carried_objects) = (carried_objects.to_vec(), rendered.to_vec());
    let (bed, left) = plan_bed(&parts, voices > 0);
    let bed_width = bed.as_ref().map_or(0, |bed| bed.layout.channels());

    // How many object elements there may be: what is asked, or what is left
    // of the budget beside the bed element.
    let room = BUDGET - bed_width;
    let allowed = config.elements.unwrap_or(room);
    if allowed > room || allowed < hz_cluster::MIN_ELEMENTS {
        return Err(Error::unsupported(
            path,
            format!(
                "{allowed} object elements; an IA sequence carries {} to {room}{}",
                hz_cluster::MIN_ELEMENTS,
                match &bed {
                    Some(bed) => format!(" beside a {} bed element", bed.layout.name),
                    None => String::new(),
                }
            ),
        ));
    }
    let folding = left.len() + carried_objects.len() > allowed;
    if folding && left.len() >= allowed {
        return Err(Error::unsupported(
            path,
            format!(
                "{} bed channels no IAMF layout has and {allowed} elements; such a bed channel \
                 keeps an element of its own, and the objects need at least one",
                left.len()
            ),
        ));
    }

    let mut elements = Vec::new();
    if let Some(bed) = &bed {
        elements.push(Element::Channels(bed.layout));
    }
    let block = cluster_block(config.frame);
    let mut carried = Vec::new();
    if folding {
        for element in 0..allowed {
            carried.push(Carried::folded(bed_width + element, block));
        }
    } else {
        for &index in &left {
            let Part::Bed {
                channel, position, ..
            } = parts[index]
            else {
                unreachable!("only bed channels are left over")
            };
            carried.push(Carried::still(bed_width + carried.len(), channel, position));
        }
        for &index in &carried_objects {
            let Part::Object { channel, keyframes } = &parts[index] else {
                unreachable!("an object")
            };
            carried.push(Carried::moving(
                bed_width + carried.len(),
                *channel,
                Path::new(keyframes),
            ));
        }
    }
    for element in &carried {
        elements.push(Element::Object {
            kind,
            default: element.default,
        });
    }

    let voices_feed = if rendered.is_empty() {
        None
    } else {
        let layout = &bed.as_ref().expect("a bed takes the voices").layout;
        Some(Voices::new(layout, &parts, &rendered)?)
    };
    let fill = BedFill {
        routes: bed.as_ref().map_or_else(Vec::new, |bed| bed.routes.clone()),
        width: bed_width,
        voices: voices_feed,
        mixed: Vec::new(),
    };
    let feed = if folding {
        let sources: Vec<(usize, Pooled)> = left
            .iter()
            .map(|&index| match parts[index] {
                Part::Bed {
                    channel, position, ..
                } => (channel, Pooled::Pinned(position)),
                _ => unreachable!("only bed channels are left over"),
            })
            .chain(carried_objects.iter().map(|&index| match &parts[index] {
                Part::Object { channel, keyframes } => {
                    (*channel, Pooled::Moving(Path::new(keyframes), 0))
                }
                _ => unreachable!("an object"),
            }))
            .collect();
        Feed::Folded(Box::new(Folding::new(path, fill, sources, allowed)?))
    } else {
        Feed::Direct(fill)
    };

    let mut lines = Vec::new();
    let mut parts_said = Vec::new();
    let objects_carried = if folding { 0 } else { carried_objects.len() };
    if folding {
        parts_said.push(format!(
            "{} objects{} folded into {allowed} elements",
            carried_objects.len(),
            if left.is_empty() {
                String::new()
            } else {
                format!(" and {} bed channels (pinned)", left.len())
            }
        ));
    } else {
        parts_said.push(plural(objects_carried, "object", "objects"));
        if !left.is_empty() {
            parts_said.push(format!(
                "{} as still objects",
                plural(left.len(), "bed channel", "bed channels")
            ));
        }
    }
    if let Some(bed) = &bed {
        parts_said.push(bed_said(bed, &parts));
    }
    if voices > 0 {
        lines.push(format!(
            "  voices       the last {} rendered into the {} bed element on the room's cube{}",
            plural(voices, "object", "objects"),
            bed.as_ref().expect("a bed").layout.name,
            if layout_for(&bed_names(&parts)).is_some_and(|layout| takes_voices(&layout)) {
                ", the master's own bed layout"
            } else {
                ", 7.1.4 because the master's bed has no front three to put a voice on"
            }
        ));
    }
    drive(Drive {
        config,
        kind,
        source,
        elements,
        bed_width,
        bed: bed.map(|bed| bed.layout),
        carried,
        feed,
        programme: parts_said.join(", "),
        lines,
        overlay: None,
    })
}

/// The names of the master's bed channels, the LFE's included.
fn bed_names(parts: &[Part]) -> Vec<&'static str> {
    parts
        .iter()
        .filter_map(|part| match part {
            Part::Lfe { .. } => Some("LFE"),
            Part::Bed { name, .. } => Some(*name),
            Part::Object { .. } => None,
        })
        .collect()
}

fn bed_said(bed: &BedPlan, parts: &[Part]) -> String {
    let master = bed_names(parts).len();
    let routed = bed.routes.len();
    let channels = bed.layout.channels();
    format!(
        "the bed as a {} element{}",
        bed.layout.name,
        match (routed == channels, master > routed) {
            (true, false) => String::new(),
            (_, false) => format!(" ({routed} of its {channels} channels the master's)"),
            (_, true) => format!(
                " ({routed} of its {channels} channels the master's, {} not in it)",
                master - routed
            ),
        }
    )
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Keep the master's elements and pan its last `sources` objects onto them:
/// `encode --overlay`'s computation — see [`crate::overlay`] — written as
/// IAMF.
///
/// The elements are numbered as `encode` numbers them — the LFE first, then
/// every other track in the master's order — so that the fit, the guards and
/// the copies are the same ones block for block. They are then carried
/// IAMF's way: the bed channels as the bed element's, the objects as object
/// elements on the master's own paths, with the gain a master states for an
/// element in its samples, since an IAMF object has nowhere else to carry
/// one; so a source added to an element is added as it is, where a TrueHD
/// stream divides it by the gain its decoder will apply.
fn run_overlay(
    config: &Config,
    kind: PositionKind,
    source: Source,
    parts: Vec<Part>,
    sources: usize,
) -> Result<()> {
    let path = config.input.as_path();
    let options = &config.overlaying;
    if sources == 0 {
        return Err(Error::unsupported(
            path,
            "`--overlay 0`; an overlay with no sources is a plain encode of the master, which is \
             what leaving the flag off already does",
        ));
    }
    let Some(kept) = parts.len().checked_sub(sources).filter(|kept| *kept > 0) else {
        return Err(Error::unsupported(
            path,
            format!(
                "{sources} overlay sources out of {} objects; the sources are the master's *last* \
                 objects and the ones before them are the elements they are panned onto, so there \
                 has to be at least one of those",
                parts.len()
            ),
        ));
    };
    if let Some(bed) = parts[kept..]
        .iter()
        .position(|part| !matches!(part, Part::Object { .. }))
    {
        return Err(Error::unsupported(
            path,
            format!(
                "overlay source {bed} is a bed channel; the sources are the master's last \
                 objects"
            ),
        ));
    }
    if !parts[..kept]
        .iter()
        .any(|part| !matches!(part, Part::Lfe { .. }))
    {
        return Err(Error::unsupported(
            path,
            "no element to overlay onto but the LFE, which has no direction to pan a source to",
        ));
    }
    if !(16..=24).contains(&options.fold_depth) {
        return Err(Error::unsupported(
            path,
            format!(
                "a fold depth of {} bits; an IA sequence's mixed elements are rounded to 16 to 24",
                options.fold_depth
            ),
        ));
    }
    if options.beds_asked < 0.0 {
        return Err(Error::unsupported(
            path,
            format!(
                "a bed reach of {}; it is a distance, so nought declines the preference and \
                 anything below that is a typed minus sign",
                options.beds_asked
            ),
        ));
    }

    // The bed element, from the kept bed channels; a source is never a bed.
    let (bed, left) = plan_bed(&parts[..kept], false);
    let bed_width = bed.as_ref().map_or(0, |bed| bed.layout.channels());
    // Each kept element's channel in a pushed frame: the bed element's, or an
    // object element's after it, in the master's order.
    let mut to_channel = Vec::with_capacity(kept);
    let mut carried = Vec::new();
    for (index, part) in parts[..kept].iter().enumerate() {
        let routed = bed.as_ref().and_then(|bed| {
            bed.routes
                .iter()
                .find(|(channel, _)| *channel == part.channel())
                .map(|(_, slot)| *slot)
        });
        match (part, routed) {
            (_, Some(slot)) => to_channel.push(slot),
            (
                Part::Bed {
                    channel, position, ..
                },
                None,
            ) => {
                debug_assert!(left.contains(&index));
                to_channel.push(bed_width + carried.len());
                carried.push(Carried::still(
                    bed_width + carried.len(),
                    *channel,
                    *position,
                ));
            }
            (Part::Object { channel, keyframes }, None) => {
                to_channel.push(bed_width + carried.len());
                carried.push(Carried::moving(
                    bed_width + carried.len(),
                    *channel,
                    Path::new(keyframes),
                ));
            }
            (Part::Lfe { .. }, None) => unreachable!("a bed element takes the LFE"),
        }
    }
    let channels = bed_width + carried.len();
    if channels > BUDGET || 1 + carried.len() > BUDGET {
        return Err(Error::unsupported(
            path,
            format!(
                "{} kept elements in {channels} channels; an IA sequence carries {BUDGET}, and an \
                 overlay keeps the master's elements and folds none — `--objects` alone folds \
                 them",
                kept
            ),
        ));
    }
    // Spare elements: what is left of the budget, for the sources nearest
    // the front, when asked for.
    let origins: Vec<Option<[f64; 3]>> = parts[kept..]
        .iter()
        .map(|part| match part {
            Part::Object { keyframes, .. } => {
                Some(keyframes.first().copied().unwrap_or_default().position)
            }
            _ => None,
        })
        .collect();
    let slots = overlay::spare_slots(&origins, BUDGET - channels, options.spare);
    for &index in &slots {
        let Part::Object { channel, keyframes } = &parts[kept + index] else {
            unreachable!("a source is an object")
        };
        to_channel.push(bed_width + carried.len());
        carried.push(Carried::moving(
            bed_width + carried.len(),
            *channel,
            Path::new(keyframes),
        ));
    }

    // Bed channels judged as `encode` judges them, on this stream's own grid:
    // what the positions are coded as.
    let described: Vec<Option<(&[Keyframe], bool)>> = parts[..kept]
        .iter()
        .map(|part| match part {
            Part::Lfe { .. } => None,
            Part::Bed { .. } => Some((&[][..], true)),
            Part::Object { keyframes, .. } => Some((&keyframes[..], false)),
        })
        .collect();
    let beds = overlay::beds_among(&described, |position| kind.quantise(position));
    let declared_beds = parts[..kept]
        .iter()
        .filter(|part| matches!(part, Part::Bed { .. }))
        .count();
    let engine_channels = (0..kept)
        .chain(slots.iter().map(|index| kept + index))
        .map(|index| Some(parts[index].channel()))
        .collect();
    let sample_rate = source.sample_rate();
    let block = overlay::block_samples(sample_rate);
    let overlaid = Overlaid::new(
        path,
        overlay::Setup {
            sources,
            elements: kept,
            slots,
            beds,
            declared_beds,
            channels: engine_channels,
            bounds: options.bounds,
            beds_first: options.beds_first,
            report: options.report.as_deref(),
            seconds_a_block: block as f64 / f64::from(sample_rate),
        },
    )?;

    let mut elements = Vec::new();
    if let Some(bed) = &bed {
        elements.push(Element::Channels(bed.layout));
    }
    for element in &carried {
        elements.push(Element::Object {
            kind,
            default: element.default,
        });
    }
    // The gain each element's own samples carry: the master's path for an
    // object, unity for a bed channel.
    let gains = (0..kept)
        .chain(overlaid.slots.iter().map(|index| kept + index))
        .map(|index| match &parts[index] {
            Part::Object { keyframes, .. } => Some((Path::new(keyframes), 0)),
            _ => None,
        })
        .collect();
    let width = bed_width + carried.len();
    let full_scale = f64::from(1u32 << (config.bits - 1));
    let depth = options.fold_depth.min(config.bits);
    let first = usize::from(matches!(parts.first(), Some(Part::Lfe { .. })));
    let engine_width = kept + overlaid.slots.len();

    let mut said = Vec::new();
    let objects = carried
        .iter()
        .filter(|c| matches!(c.placement, Placement::Path(_)))
        .count();
    said.push(plural(objects, "object element", "object elements"));
    if !left.is_empty() {
        said.push(format!(
            "{} as still objects",
            plural(left.len(), "bed channel", "bed channels")
        ));
    }
    if let Some(bed) = &bed {
        said.push(bed_said(bed, &parts[..kept]));
    }

    let feed = Feed::Overlaid(Box::new(Overlay {
        live: parts
            .iter()
            .map(|part| Live {
                next: 0,
                // As `encode` states them: an object at its first update, a
                // bed channel at its speaker in the bed class, the LFE at
                // nothing in particular.
                state: match part {
                    Part::Object { keyframes, .. } => {
                        keyframes.first().copied().unwrap_or_default()
                    }
                    Part::Bed { position, .. } => Keyframe {
                        position: *position,
                        mode: hz_cluster::class::BED,
                        ..Keyframe::default()
                    },
                    Part::Lfe { .. } => Keyframe::default(),
                },
            })
            .collect(),
        overlaid,
        parts,
        gains,
        to_channel,
        first,
        block,
        width,
        raw: Vec::new(),
        pending: Vec::new(),
        pending_from: 0,
        pending_frames: 0,
        exhausted: false,
        at: 0,
        scene: Scene::weighed(f64::from(sample_rate), options.weighing)
            .map_err(|why| Error::unsupported(path, why.to_string()))?,
        signals: Vec::new(),
        object_gains: Vec::new(),
        mixed: vec![Vec::new(); engine_width],
        from: Vec::new(),
        limiter: options.limit.then(hz_cluster::mix::Limiter::new),
        floor: hz_cluster::floor::Floor::for_dialnorm(options.dialnorm),
        renderers: hz_cluster::metric::delivery()
            .map_err(|why| Error::unsupported(path, why.to_string()))?,
        full_scale,
        step: f64::from(1u32 << (config.bits - depth)),
        peak: 0.0,
        clipped: 0,
        blocks: 0,
        mean: 0.0,
        worst: 0.0,
    }));
    drive(Drive {
        config,
        kind,
        source,
        elements,
        bed_width,
        bed: bed.map(|bed| bed.layout),
        carried,
        feed,
        programme: said.join(", "),
        lines: Vec::new(),
        overlay: Some(OverlaySummary {
            sources,
            beds_first: options.beds_first,
            fold_depth: depth,
        }),
    })
}

/// Where each element of an overlay stands as a block ends, and which of its
/// updates is next: the state `encode` places an overlay's carriers and
/// sources on — the latest update in force, its target and not part way along
/// its ramp.
struct Live {
    next: usize,
    state: Keyframe,
}

/// The voices `--voices-to-bed` renders into the bed element: panned on the
/// room's cube onto the bed's own layout, each moved at its own updates and
/// ramping its gains over the samples each asks for — the render the bed mode
/// makes, onto a different layout.
struct Voices {
    mixdown: Mixdown,
    tracks: Vec<VoiceTrack>,
    /// One block of the voices' samples, interleaved, and what the mixdown
    /// makes of them, both reused.
    input: Vec<f32>,
    rendered: Vec<f64>,
}

struct VoiceTrack {
    channel: usize,
    keyframes: Vec<Keyframe>,
    next: usize,
}

impl Voices {
    fn new(layout: &hz_iamf::Layout, parts: &[Part], voices: &[usize]) -> Result<Self> {
        let room = Layout {
            name: layout.name,
            speakers: layout.labels.iter().map(|label| canonical(label)).collect(),
        };
        let mut mixdown = Mixdown::new(&room)?;
        let tracks = voices
            .iter()
            .enumerate()
            .map(|(index, &part)| {
                mixdown.object(index);
                let Part::Object { channel, keyframes } = &parts[part] else {
                    unreachable!("a voice is an object")
                };
                let mut keyframes = keyframes.clone();
                keyframes.sort_by_key(|k| k.sample_pos);
                // Heard from the first sample, at its first update — see
                // [`Path::new`].
                if let Some(first) = keyframes.first() {
                    mixdown.update(
                        index,
                        &Keyframe {
                            ramp_samples: 0,
                            ..*first
                        },
                    );
                }
                VoiceTrack {
                    channel: *channel,
                    keyframes,
                    next: 0,
                }
            })
            .collect();
        Ok(Self {
            mixdown,
            tracks,
            input: Vec::new(),
            rendered: Vec::new(),
        })
    }

    /// The voices of `got` frames from sample `at`, rendered into
    /// `self.rendered`, the bed's width a frame. Split at every update a voice
    /// makes inside them, so that each lands on its own sample.
    fn render(&mut self, raw: &[i32], stride: usize, got: usize, at: u64, input_scale: f64) {
        let voices = self.tracks.len();
        let width = self.mixdown.channels();
        self.input.clear();
        for frame in raw[..got * stride].chunks_exact(stride) {
            self.input.extend(
                self.tracks
                    .iter()
                    .map(|track| (f64::from(frame[track.channel]) * input_scale) as f32),
            );
        }
        self.rendered.clear();
        self.rendered.resize(got * width, 0.0);
        let mut done = 0;
        while done < got {
            let now = at + done as u64;
            let mut next_update = u64::MAX;
            for (index, track) in self.tracks.iter_mut().enumerate() {
                while let Some(state) = track.keyframes.get(track.next) {
                    if state.sample_pos > now {
                        next_update = next_update.min(state.sample_pos);
                        break;
                    }
                    self.mixdown.update(index, state);
                    track.next += 1;
                }
            }
            let end = next_update.saturating_sub(at).min(got as u64) as usize;
            self.mixdown.render(
                &self.input[done * voices..got * voices],
                voices,
                end - done,
                &mut self.rendered[done * width..got * width],
            );
            done = end;
        }
    }
}

/// The bed element's channels: the master's bed channels where the layout
/// has them, silence where it does not, and the voices rendered in when
/// there are any.
struct BedFill {
    /// `(master channel, layout channel)`.
    routes: Vec<(usize, usize)>,
    width: usize,
    voices: Option<Voices>,
    /// One block of the bed before it is rounded, when voices are added to
    /// it.
    mixed: Vec<f64>,
}

/// How samples are scaled in and out: the master's integers to ±1, and ±1
/// to the codec's.
#[derive(Debug, Clone, Copy)]
struct Scale {
    input: f64,
    full: f64,
    low: f64,
    high: f64,
}

impl Scale {
    /// `x`, at ±1, into the codec's integers: rounded, clamped, and counted
    /// when it had to be.
    #[inline]
    fn code(&self, x: f64, account: &mut Account) -> i32 {
        account.peak = account.peak.max(x.abs());
        let scaled = (x * self.full).round();
        if scaled < self.low || scaled > self.high {
            account.clipped += 1;
        }
        scaled.clamp(self.low, self.high) as i32
    }
}

impl BedFill {
    /// `got` frames of the bed into the first `self.width` channels of `q`,
    /// `width` channels a frame. A bed channel nothing is added to is the
    /// master's sample as it is.
    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        raw: &[i32],
        stride: usize,
        got: usize,
        at: u64,
        scale: &Scale,
        q: &mut [i32],
        width: usize,
        account: &mut Account,
    ) {
        if self.width == 0 {
            return;
        }
        for frame in q[..got * width].chunks_exact_mut(width) {
            frame[..self.width].fill(0);
        }
        match &mut self.voices {
            None => {
                for &(channel, slot) in &self.routes {
                    for n in 0..got {
                        let x = f64::from(raw[n * stride + channel]) * scale.input;
                        q[n * width + slot] = scale.code(x, account);
                    }
                }
            }
            Some(voices) => {
                voices.render(raw, stride, got, at, scale.input);
                self.mixed.clear();
                self.mixed.extend_from_slice(&voices.rendered);
                for &(channel, slot) in &self.routes {
                    for n in 0..got {
                        self.mixed[n * self.width + slot] +=
                            f64::from(raw[n * stride + channel]) * scale.input;
                    }
                }
                for n in 0..got {
                    for slot in 0..self.width {
                        q[n * width + slot] =
                            scale.code(self.mixed[n * self.width + slot], account);
                    }
                }
            }
        }
    }
}

/// What a fold takes: a bed channel no layout has, pinned at its speaker, or
/// an object on its path (and where the walk along it stopped).
enum Pooled {
    Pinned([f64; 3]),
    Moving(Path, usize),
}

/// More objects than elements: folded into the elements block by block with
/// `hz-cluster` — where each element goes, and how much of each object it
/// carries.
///
/// The same fold the TrueHD encode makes, from the same crate, less one
/// thing: it places each block's elements on that block's own energies, with
/// none of the look-ahead the TrueHD encode smooths them with.
struct Folding {
    bed: BedFill,
    sources: Vec<(usize, Pooled)>,
    scene: Scene,
    clusterer: hz_cluster::Clusterer,
    limiter: hz_cluster::mix::Limiter,
    renderers: Vec<Box<dyn hz_render::Renderer>>,
    floor: hz_cluster::floor::Floor,
    signals: Vec<Vec<f32>>,
    ones: Vec<f64>,
    mixed: Vec<Vec<f32>>,
    from: Vec<Vec<f64>>,
    previous: Option<hz_cluster::Clustering>,
    error_sum: f64,
    error_worst: f64,
    judged: u64,
    limited: u64,
}

impl Folding {
    fn new(
        path: &std::path::Path,
        bed: BedFill,
        sources: Vec<(usize, Pooled)>,
        count: usize,
    ) -> Result<Self> {
        use hz_cluster::scene::Loudness as Weighing;
        use hz_cluster::{Clusterer, Weighting};
        let fail = |why: hz_core::Error| Error::unsupported(path, why.to_string());
        let n = sources.len();
        Ok(Self {
            bed,
            sources,
            // The sample rate does not reach a flat weighing; the fold is
            // steered by plain power, as it was.
            scene: Scene::weighed(48_000.0, Weighing::Flat).map_err(fail)?,
            clusterer: Clusterer::new(count, Weighting::Fitted)
                .map_err(fail)?
                .flooring(hz_cluster::floor::Floor::for_dialnorm(-31.0)),
            limiter: hz_cluster::mix::Limiter::new(),
            renderers: hz_cluster::metric::delivery().map_err(fail)?,
            floor: hz_cluster::floor::Floor::for_dialnorm(-31.0),
            signals: vec![Vec::new(); n],
            ones: vec![1.0; n],
            mixed: vec![Vec::new(); count],
            from: Vec::new(),
            previous: None,
            error_sum: 0.0,
            error_worst: 0.0,
            judged: 0,
            limited: 0,
        })
    }

    /// Fold `got` frames read from `at` into the object elements, a block at
    /// a time, each element's place at the end of each block appended to its
    /// history.
    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        raw: &[i32],
        stride: usize,
        got: usize,
        at: u64,
        scale: &Scale,
        q: &mut [i32],
        width: usize,
        carried: &mut [Carried],
        account: &mut Account,
    ) -> Result<()> {
        use hz_cluster::scene::Source as SceneSource;
        let first = self.bed.width;
        let block = match &carried[0].placement {
            Placement::Folded(folded) => folded.block,
            _ => unreachable!("a fold's elements are folded"),
        };
        let ceiling = scale.high / scale.full;
        let mut start = 0;
        while start < got {
            let len = block.min(got - start);
            let t0 = at + start as u64;
            // The scene of this block: every source's samples with its gain
            // on them, and where it is at the block's end.
            self.scene.start();
            for (signal, (channel, pooled)) in self.signals.iter_mut().zip(&mut self.sources) {
                signal.clear();
                let (position, pinned) = match pooled {
                    Pooled::Pinned(position) => {
                        signal.extend((0..len).map(|n| {
                            (f64::from(raw[(start + n) * stride + *channel]) * scale.input) as f32
                        }));
                        (*position, Some(*position))
                    }
                    Pooled::Moving(object_path, hint) => {
                        signal.extend((0..len).map(|n| {
                            let t = t0 + n as u64;
                            let gain = object_path.gain(t, hint);
                            (f64::from(raw[(start + n) * stride + *channel]) * scale.input * gain)
                                as f32
                        }));
                        let end = t0 + len as u64 - 1;
                        (object_path.segment(end, hint).at(end).position, None)
                    }
                };
                self.scene.push(
                    &SceneSource {
                        position,
                        pinned,
                        mode: if pinned.is_some() {
                            hz_cluster::class::BED
                        } else {
                            hz_render::Mode::default()
                        },
                        ..SceneSource::default()
                    },
                    signal.iter().map(|&x| f64::from(x)),
                );
            }
            self.scene.finish();

            let clustering = self
                .clusterer
                .cluster(self.scene.objects(), self.previous.as_ref())
                .map_err(|why| {
                    Error::unsupported(std::path::Path::new("the fold"), why.to_string())
                })?;
            self.from.clear();
            match &self.previous {
                Some(before) if before.weights.len() == clustering.weights.len() => {
                    self.from.extend(before.weights.iter().cloned());
                }
                _ => self.from.extend(clustering.weights.iter().cloned()),
            }
            hz_cluster::mix::mix(
                &self.signals,
                &self.ones,
                &self.from,
                &clustering.weights,
                len,
                &mut self.mixed,
            );
            if self.limiter.apply(&mut self.mixed, ceiling) {
                self.limited += 1;
            }
            if let Ok(report) = hz_cluster::metric::error_with(
                self.scene.objects(),
                &clustering,
                &self.renderers,
                &self.floor,
            ) {
                self.error_sum += report.mean;
                self.error_worst = self.error_worst.max(report.worst);
                self.judged += 1;
            }
            for (element, signal) in self.mixed.iter().enumerate() {
                for n in 0..len {
                    q[(start + n) * width + first + element] =
                        scale.code(f64::from(signal[n]), account);
                }
            }
            // Where each element is going, for the blocks and the
            // measurement, which ramp there across the block.
            for (element, at_end) in clustering.positions.iter().enumerate() {
                if let Placement::Folded(folded) = &mut carried[element].placement {
                    folded.history.push(*at_end);
                }
            }
            self.previous = Some(clustering);
            start += len;
        }
        Ok(())
    }
}

/// `--overlay`: the master's elements kept and its last few objects panned
/// onto them, a block of [`overlay::block_samples`] at a time — the block
/// `encode` decides an overlay on, so that the two write the same one — and
/// handed out a unit at a time.
struct Overlay {
    overlaid: Overlaid,
    /// The LFE first, then every other track in the master's order: the
    /// kept elements, then the sources.
    parts: Vec<Part>,
    live: Vec<Live>,
    /// The gain each element's own samples carry — an object's path; `None`
    /// for a bed channel, which carries none — over the kept elements and
    /// then the spare slots.
    gains: Vec<Option<(Path, usize)>>,
    /// Each element's channel in a pushed frame.
    to_channel: Vec<usize>,
    /// One when the first element is the LFE.
    first: usize,
    block: usize,
    width: usize,
    raw: Vec<i32>,
    /// Frames decided and not yet handed out, `width` a frame, from
    /// `pending_from`.
    pending: Vec<i32>,
    pending_from: usize,
    pending_frames: usize,
    exhausted: bool,
    /// Samples read from the master.
    at: u64,
    scene: Scene,
    signals: Vec<Vec<f32>>,
    object_gains: Vec<f64>,
    mixed: Vec<Vec<f32>>,
    from: Vec<Vec<f64>>,
    limiter: Option<hz_cluster::mix::Limiter>,
    floor: hz_cluster::floor::Floor,
    renderers: Vec<Box<dyn hz_render::Renderer>>,
    full_scale: f64,
    /// What a mixed sample is rounded to, in the codec's integers.
    step: f64,
    peak: f64,
    clipped: u64,
    blocks: u64,
    mean: f64,
    worst: f64,
}

impl Overlay {
    /// Up to `want` frames into `q`, `width` a frame: as many blocks decided
    /// as it takes, the rest held for the next unit. Fewer only at the end.
    fn fill(
        &mut self,
        source: &mut Source,
        stride: usize,
        want: usize,
        scale: &Scale,
        q: &mut [i32],
        account: &mut Account,
    ) -> Result<usize> {
        while self.pending_frames < want && !self.exhausted {
            self.decide(source, stride, scale, account)?;
        }
        let got = want.min(self.pending_frames);
        let width = self.width;
        let from = self.pending_from * width;
        q[..got * width].copy_from_slice(&self.pending[from..from + got * width]);
        self.pending_from += got;
        self.pending_frames -= got;
        Ok(got)
    }

    /// Read and decide one block. A block is read whole whatever `--frames`
    /// says, as `encode` reads it, so that the last one is decided on the
    /// same samples; only what is handed out stops at the limit.
    fn decide(
        &mut self,
        source: &mut Source,
        stride: usize,
        scale: &Scale,
        account: &mut Account,
    ) -> Result<()> {
        let block = self.block;
        self.raw.resize(block * stride, 0);
        let frames = iamf::fill(source, &mut self.raw, block, stride)?;
        if frames == 0 {
            self.exhausted = true;
            return Ok(());
        }
        if frames < block {
            self.exhausted = true;
        }
        let path = std::path::Path::new("the overlay");
        let at = self.at;
        let end = at + frames as u64 - 1;
        // Every element to the update in force at the block's end.
        for (live, part) in self.live.iter_mut().zip(&self.parts) {
            let Part::Object { keyframes, .. } = part else {
                continue;
            };
            while live.next < keyframes.len() && keyframes[live.next].sample_pos <= end {
                live.state = keyframes[live.next];
                live.next += 1;
            }
        }

        // The scene, and each object's samples: every track but the LFE, the
        // kept elements first and the sources after them, as `encode` builds
        // it. The scene weighs each as the master states it; the mix takes an
        // element with its gain already in its samples, since that is where
        // an IAMF object carries it, and a source at its stated gain.
        let kept = self.overlaid.elements;
        self.scene.start();
        self.object_gains.clear();
        let objects = self.parts.len() - self.first;
        self.signals.resize_with(objects, Vec::new);
        for (object, part_index) in (self.first..self.parts.len()).enumerate() {
            let state = self.live[part_index].state;
            let channel = self.parts[part_index].channel();
            let signal = &mut self.signals[object];
            signal.clear();
            signal.extend(
                (0..frames)
                    .map(|n| (f64::from(self.raw[n * stride + channel]) * scale.input) as f32),
            );
            self.scene.push(
                &SceneSource {
                    position: state.position,
                    gain: state.gain,
                    size: state.spread,
                    importance: state.importance,
                    pinned: None,
                    mode: state.mode,
                },
                signal.iter().map(|sample| f64::from(*sample)),
            );
            self.object_gains.push(state.gain);
            if part_index < kept
                && let Some((gain_path, hint)) = &mut self.gains[part_index]
            {
                for (n, sample) in signal.iter_mut().enumerate() {
                    let gain = gain_path.gain(at + n as u64, hint);
                    if gain != 1.0 {
                        *sample = (f64::from(*sample) * gain) as f32;
                    }
                }
            }
        }
        self.scene.finish();

        // What a decoder applies to each element's samples: nothing more —
        // the gain is in them already — unless the master silenced it.
        self.overlaid.kept.clear();
        for (part, live) in self.parts[..kept].iter().zip(&self.live) {
            self.overlaid.kept.push(match part {
                Part::Lfe { .. } => None,
                _ => Some(overlay::Kept {
                    position: live.state.position,
                    stated: if live.state.gain > MUTED { 1.0 } else { 0.0 },
                }),
            });
        }
        self.overlaid.voices.clear();
        for live in &self.live[kept..] {
            self.overlaid.voices.push(overlay::Voice::of(
                &live.state,
                if live.state.gain > MUTED { 1.0 } else { 0.0 },
            ));
        }
        let elements = self.to_channel.len();
        let spent = self
            .overlaid
            .span(
                overlay::Block {
                    first: self.first,
                    elements,
                    frames,
                },
                overlay::Buffers {
                    scene: &self.scene,
                    signals: &self.signals,
                    gains: &mut self.object_gains,
                    mixed: &mut self.mixed,
                    from: &mut self.from,
                    limiter: self.limiter.as_mut(),
                    ceiling: (self.full_scale - 1.0) / self.full_scale,
                    floor: &self.floor,
                    renderers: &self.renderers,
                },
            )
            .map_err(|e| Error::io(path, e))?;
        self.blocks += 1;
        if let Some((mean, worst)) = spent.cost {
            self.mean += mean;
            self.worst = self.worst.max(worst);
        }

        // Out, into the frames waiting to be handed out: a copied element as
        // the master's samples with its gain, a mixed one rounded to the fold
        // depth, and a bed channel the master has not got silent.
        let width = self.width;
        if self.pending_from > 0 {
            let from = self.pending_from * width;
            self.pending
                .copy_within(from..from + self.pending_frames * width, 0);
            self.pending_from = 0;
        }
        let base = self.pending_frames * width;
        self.pending.resize(base + frames * width, 0);
        self.pending[base..].fill(0);
        let out = &mut self.pending[base..];
        for element in 0..elements {
            let channel = self.to_channel[element];
            match self.overlaid.copy_from[element] {
                Some(input) => match &mut self.gains[element] {
                    Some((gain_path, hint)) => {
                        for n in 0..frames {
                            let x = f64::from(self.raw[n * stride + input])
                                * scale.input
                                * gain_path.gain(at + n as u64, hint);
                            out[n * width + channel] = scale.code(x, account);
                        }
                    }
                    None => {
                        for n in 0..frames {
                            let x = f64::from(self.raw[n * stride + input]) * scale.input;
                            out[n * width + channel] = scale.code(x, account);
                        }
                    }
                },
                None => {
                    let signal = &self.mixed[element];
                    for n in 0..frames {
                        out[n * width + channel] = overlay::quantise(
                            signal[n],
                            self.full_scale,
                            self.step,
                            &mut self.peak,
                            &mut self.clipped,
                        );
                    }
                }
            }
        }
        self.pending_frames += frames;
        self.at += frames as u64;
        Ok(())
    }
}

/// What fills each unit's elements.
enum Feed {
    /// The bed element and every object as it is.
    Direct(BedFill),
    /// The bed element, and the objects folded into the object elements.
    Folded(Box<Folding>),
    /// The master's elements kept, its last few objects panned onto them.
    Overlaid(Box<Overlay>),
}

impl Feed {
    /// Up to `want` frames of every element into `q`, `width` a frame, from
    /// sample `at`: how many, fewer only at the end.
    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        source: &mut Source,
        raw: &mut Vec<i32>,
        want: usize,
        at: u64,
        scale: &Scale,
        q: &mut [i32],
        width: usize,
        carried: &mut [Carried],
        account: &mut Account,
    ) -> Result<usize> {
        let stride = source.channels();
        if let Self::Overlaid(overlay) = self {
            return overlay.fill(source, stride, want, scale, q, account);
        }
        raw.resize(want * stride, 0);
        let got = iamf::fill(source, raw, want, stride)?;
        if got == 0 {
            return Ok(0);
        }
        match self {
            Self::Direct(bed) => {
                bed.fill(raw, stride, got, at, scale, q, width, account);
                // Each object's samples, its gain applied, in the codec's
                // integers; a bed channel left over stands at unity.
                for element in carried.iter_mut() {
                    let input = element.input.expect("an object carries one channel");
                    let channel = element.channel;
                    match &mut element.placement {
                        Placement::Path(moving) => {
                            for n in 0..got {
                                let gain = moving.path.gain(at + n as u64, &mut moving.audio);
                                let x = f64::from(raw[n * stride + input]) * scale.input * gain;
                                q[n * width + channel] = scale.code(x, account);
                            }
                        }
                        Placement::Still => {
                            for n in 0..got {
                                let x = f64::from(raw[n * stride + input]) * scale.input;
                                q[n * width + channel] = scale.code(x, account);
                            }
                        }
                        Placement::Folded(_) => unreachable!("nothing is folded"),
                    }
                }
                Ok(got)
            }
            Self::Folded(folding) => {
                folding
                    .bed
                    .fill(raw, stride, got, at, scale, q, width, account);
                folding.fill(raw, stride, got, at, scale, q, width, carried, account)?;
                Ok(got)
            }
            Self::Overlaid(_) => unreachable!("handed out above"),
        }
    }
}

/// Everything a run was set up with, handed to [`drive`].
struct Drive<'a> {
    config: &'a Config,
    kind: PositionKind,
    source: Source,
    elements: Vec<Element>,
    bed_width: usize,
    bed: Option<hz_iamf::Layout>,
    carried: Vec<Carried>,
    feed: Feed,
    /// The summary's first line: what the elements are.
    programme: String,
    /// More of the summary, the mode's own.
    lines: Vec<String>,
    overlay: Option<OverlaySummary>,
}

/// One temporal unit on its way to the writer, and back to be refilled.
struct Unit {
    samples: Vec<i32>,
    frames: usize,
    /// The position blocks that start with it: `(element, subblocks)`, the
    /// first `stated` of them.
    blocks: Vec<(usize, Vec<Subblock>)>,
    stated: usize,
}

/// The sequence header's profile, by name, for these elements — the rule
/// [`hz_iamf::Writer`] applies.
fn profile_name(elements: &[Element]) -> &'static str {
    let channels: usize = elements.iter().map(Element::channels).sum();
    let objects = elements
        .iter()
        .filter(|e| matches!(e, Element::Object { .. }))
        .count();
    match elements.len().max(channels) {
        n if n > 18 => "advanced-2",
        _ if objects == elements.len() => "base-advanced",
        _ => "advanced-1",
    }
}

/// Write the sequence: each unit's elements from the feed, the position
/// blocks that start in it, the trace, the measurement. The codec runs on a
/// thread of its own, a unit behind, as a folding `encode`'s encoder runs
/// behind its fold: the two want different things and neither waits on the
/// other for anything but the samples.
fn drive(job: Drive<'_>) -> Result<()> {
    let Drive {
        config,
        kind,
        mut source,
        elements,
        bed_width,
        bed,
        mut carried,
        mut feed,
        programme,
        lines,
        overlay: overlay_summary,
    } = job;
    let path = config.input.as_path();
    let format = *source.pcm_format();
    let sample_rate = source.sample_rate();
    let full = f64::from(1u32 << (config.bits - 1));
    let scale = Scale {
        input: 1.0 / f64::from(1u32 << (format.bits_per_channel - 1)),
        full,
        low: -full,
        high: full - 1.0,
    };
    let width: usize = elements.iter().map(Element::channels).sum();
    debug_assert_eq!(width, bed_width + carried.len());
    let first_object = usize::from(bed.is_some());
    let writer = iamf::open(config, sample_rate, elements.clone())?;
    let delay = writer.delay() as i64;

    // The measurement renders what a decoder is handed, on 7.1.4: the bed
    // element's channels to their speakers, every object on the room's cube.
    let layout = Layout::surround_7_1_4();
    let mut mixdown = Mixdown::new(&layout)?;
    if let Some(bed) = &bed {
        for (slot, label) in bed.labels.iter().enumerate() {
            let label = canonical(label);
            match layout.index_of(label) {
                Some(channel) => {
                    mixdown.speaker(slot, channel);
                }
                None => {
                    let index = mixdown.object(slot);
                    mixdown.update(
                        index,
                        &Keyframe {
                            position: hz_render::fold::bed_position(label)
                                .expect("a layout's channel has a place"),
                            ..Keyframe::default()
                        },
                    );
                }
            }
        }
    }
    let measured_as: Vec<usize> = carried
        .iter()
        .map(|element| {
            let index = mixdown.object(element.channel);
            mixdown.update(
                index,
                &Keyframe {
                    position: element.default,
                    ..Keyframe::default()
                },
            );
            index
        })
        .collect();
    let mut measure = Measure::new(sample_rate);

    let frame = config.frame;
    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    // Units on the sequence's timeline, the codec's delay and its flush
    // included: what a standstill to the end has to cover.
    let units_total = (total + delay as u64).div_ceil(frame as u64) + 1;
    let mut progress = Progress::new(config.progress, total);

    let mut raw = Vec::with_capacity(frame * source.channels());
    let mut input = vec![0f32; frame * width];
    let mut rendered = vec![0f64; frame * layout.channels()];
    let mut measured = vec![0f32; frame * layout.channels()];
    let mut account = Account::default();
    let mut trace = Trace::open(width)?;
    let mut one = Vec::new();

    let (to_writer, from_main) = std::sync::mpsc::sync_channel::<Unit>(2);
    let (free, spare) = std::sync::mpsc::channel::<Unit>();
    for _ in 0..3 {
        let _ = free.send(Unit {
            samples: vec![0; frame * width],
            frames: 0,
            blocks: (0..carried.len()).map(|_| (0, Vec::new())).collect(),
            stated: 0,
        });
    }

    let (written, looped) = std::thread::scope(|scope| {
        let handle = scope.spawn(move || -> std::io::Result<Output> {
            let mut writer = writer;
            while let Ok(unit) = from_main.recv() {
                let blocks: Vec<PositionBlock> = unit.blocks[..unit.stated]
                    .iter()
                    .map(|(element, subblocks)| PositionBlock {
                        element: *element,
                        subblocks,
                    })
                    .collect();
                writer.push(&unit.samples[..unit.frames * width], &blocks)?;
                drop(blocks);
                if free.send(unit).is_err() {
                    break;
                }
            }
            Ok(writer)
        });

        let mut looped = || -> Result<u64> {
            let mut at = 0u64;
            let mut unit_index = 0u64;
            while at < total {
                let Ok(mut unit) = spare.recv() else {
                    break;
                };
                let want = (total - at).min(frame as u64) as usize;
                let got = feed.fill(
                    &mut source,
                    &mut raw,
                    want,
                    at,
                    &scale,
                    &mut unit.samples,
                    width,
                    &mut carried,
                    &mut account,
                )?;
                if got == 0 {
                    break;
                }
                let last = at + got as u64 >= total || got < want;
                for (f, &q) in input[..got * width]
                    .iter_mut()
                    .zip(&unit.samples[..got * width])
                {
                    *f = (f64::from(q) / full) as f32;
                }

                // The blocks that start with this unit, on the sequence's
                // timeline: the unit holds the master's samples from
                // `at - delay` on.
                let (a, b) = (
                    unit_index as i64 * frame as i64 - delay,
                    (unit_index as i64 + 1) * frame as i64 - delay,
                );
                unit.stated = 0;
                for (e, element) in carried.iter_mut().enumerate() {
                    let out = &mut unit.blocks[unit.stated];
                    out.0 = first_object + e;
                    match &mut element.placement {
                        Placement::Still => continue,
                        Placement::Path(moving) => {
                            if moving.covered_until > unit_index {
                                continue;
                            }
                            let subblocks = &mut out.1;
                            moving.path.subblocks(a, b, &mut moving.blocks, subblocks);
                            if let [
                                Subblock {
                                    animation: Animation::Step(p),
                                    ..
                                },
                            ] = subblocks.as_slice()
                            {
                                // Standing still: one block for as long as
                                // it does, in whole units, to the end if it
                                // never moves again.
                                let p = *p;
                                let units = match moving.path.still_until(a, p, &mut moving.blocks)
                                {
                                    None => units_total.saturating_sub(unit_index).max(1),
                                    Some(next) => ((next as i64 - a) / frame as i64).max(1) as u64,
                                }
                                .min(longest_still(frame, sample_rate));
                                subblocks[0].duration = (units * frame as u64) as u32;
                                moving.covered_until = unit_index + units;
                            } else {
                                account.moves += 1;
                                moving.covered_until = unit_index + 1;
                                if last {
                                    // The codec's flush comes after the last
                                    // unit, and a unit no block covers would
                                    // put the object at its default.
                                    let end = match subblocks.last().map(|s| s.animation) {
                                        Some(Animation::Step(p) | Animation::Linear(_, p)) => p,
                                        None => moving.path.origin(),
                                    };
                                    subblocks.push(Subblock {
                                        duration: (2 * frame) as u32,
                                        animation: Animation::Step(end),
                                    });
                                }
                            }
                        }
                        Placement::Folded(folded) => {
                            cluster_subblocks(
                                &folded.history,
                                folded.base,
                                folded.block,
                                a,
                                b,
                                &mut out.1,
                            );
                            if last {
                                let end = *folded.history.last().expect("a block was folded");
                                out.1.push(Subblock {
                                    duration: (2 * frame) as u32,
                                    animation: Animation::Step(end),
                                });
                            }
                        }
                    }
                    unit.stated += 1;
                }
                account.blocks_written += unit.stated as u64;

                if let Some(trace) = trace.as_mut() {
                    for (e, element) in carried.iter_mut().enumerate() {
                        for offset in (0..frame).step_by(TRACE_INTERVAL) {
                            let t = a + offset as i64;
                            let p = match &mut element.placement {
                                Placement::Still => element.default,
                                Placement::Path(moving) if t < 0 => moving.path.origin(),
                                Placement::Path(moving) => {
                                    moving
                                        .path
                                        .segment(t as u64, &mut moving.trace)
                                        .at(t as u64)
                                        .position
                                }
                                Placement::Folded(folded) => folded.at(t, &mut one),
                            };
                            trace.position(unit_index, offset, first_object + e, p)?;
                        }
                    }
                    trace.samples(&input[..got * width], width)?;
                }
                unit.frames = got;
                if to_writer.send(unit).is_err() {
                    break;
                }

                // And measured as a decoder renders it: split where any
                // object's path breaks, its renderer told where the object is
                // going and how long it takes to get there.
                rendered[..got * layout.channels()].fill(0.0);
                let mut done = 0usize;
                while done < got {
                    let now = at + done as u64;
                    let mut next = at + got as u64;
                    for (element, &index) in carried.iter_mut().zip(&measured_as) {
                        if element.next_update <= now {
                            let (target, ramp, until) = match &mut element.placement {
                                Placement::Still => continue,
                                Placement::Path(moving) => {
                                    let segment = *moving.path.segment(now, &mut moving.measure);
                                    if segment.still() || segment.end == u64::MAX {
                                        (segment.from.position, 0, segment.end)
                                    } else {
                                        (
                                            segment.to.position,
                                            (segment.end - now) as u32,
                                            segment.end,
                                        )
                                    }
                                }
                                Placement::Folded(folded) => {
                                    let j = now as usize / folded.block;
                                    let end = ((j + 1) * folded.block) as u64;
                                    let target = folded.history[j - folded.base];
                                    (target, if j == 0 { 0 } else { (end - now) as u32 }, end)
                                }
                            };
                            mixdown.update(
                                index,
                                &Keyframe {
                                    position: target,
                                    ramp_samples: ramp,
                                    ..Keyframe::default()
                                },
                            );
                            element.next_update = until;
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

                // A fold's history is only looked at a unit or two back.
                let next_a = (unit_index as i64 + 1) * frame as i64 - delay;
                for element in carried.iter_mut() {
                    if let Placement::Folded(folded) = &mut element.placement {
                        folded.forget_before(next_a);
                    }
                }

                at += got as u64;
                unit_index += 1;
                if let Some(progress) = progress.as_mut() {
                    progress.at(at);
                }
                if last {
                    break;
                }
            }
            Ok(at)
        };
        let looped = looped();
        drop(to_writer);
        let written = handle.join().expect("the writer thread does not panic");
        (written, looped)
    });
    let at = looped?;
    let writer = written.map_err(|e| Error::io(&config.out, e))?;
    if at == 0 {
        return Err(Error::unsupported(path, "no audio to encode"));
    }

    let loudness = measure.finish();
    let units = writer.units();
    writer
        .finish(loudness)
        .map_err(|e| Error::io(&config.out, e))?;

    let channels: usize = elements.iter().map(Element::channels).sum();
    println!(
        "  programme    {programme}: {} elements, {channels} channels, IAMF v2.0 {}",
        elements.len(),
        profile_name(&elements)
    );
    println!(
        "  encoded      {units} temporal units of {frame}, {at} samples, {}",
        iamf::coding(config)
    );
    println!(
        "  positions    {}, {} parameter blocks, {} of them for a unit an object moved in",
        match kind {
            PositionKind::Cart16 => "Cartesian, 16 bits",
            PositionKind::Cart8 => "Cartesian, 8 bits",
            PositionKind::Polar => "polar",
        },
        account.blocks_written,
        account.moves
    );
    for line in &lines {
        println!("{line}");
    }
    let mut verdict = Ok(());
    match &feed {
        Feed::Direct(_) => {}
        Feed::Folded(folding) => println!(
            "  fold         {:.4} mean over the presentations, {:.3} at its worst, as a fraction \
             of the object's own gains; blocks of {}, no look-ahead; {} blocks where the limiter \
             brought every element down",
            if folding.judged > 0 {
                folding.error_sum / folding.judged as f64
            } else {
                0.0
            },
            folding.error_worst,
            cluster_block(frame),
            folding.limited
        ),
        Feed::Overlaid(overlay) => {
            let summary = overlay_summary.as_ref().expect("an overlay's summary");
            let overlaid = &overlay.overlaid;
            println!(
                "  floor        {:.1} dBFS at the playback level a dialnorm of {:.0} implies; a \
                 source was above it in {:.1} % of the blocks, and is guarded in those",
                overlay.floor.threshold_dbfs(),
                config.overlaying.dialnorm,
                100.0 * overlaid.voiced as f64 / overlay.blocks.max(1) as f64
            );
            overlay::summary(overlaid, overlay.blocks, summary.beds_first);
            println!(
                "  depth        the elements a source reached rounded to {} bits{}; blocks of {}, \
                 as encode decides an overlay on, {} sources",
                summary.fold_depth,
                if summary.fold_depth == config.bits {
                    ", which is every bit the mix made"
                } else {
                    ""
                },
                overlay.block,
                summary.sources
            );
            println!(
                "  headroom     {}{}",
                match &overlay.limiter {
                    Some(limiter) => format!(
                        "{} blocks where the limiter brought every element down, the mix \
                         peaking at {:.2} of full scale before it",
                        limiter.limited, limiter.peak
                    ),
                    None => "no limiter".to_string(),
                },
                if overlay.clipped > 0 {
                    format!(
                        "; {} mixed samples went outside the codec's domain and were clamped",
                        overlay.clipped
                    )
                } else {
                    String::new()
                }
            );
            let remarks = overlay::remarks(
                overlaid,
                &overlay::Totals {
                    blocks: overlay.blocks,
                    mean: overlay.mean,
                    clipped: overlay.clipped,
                    limiter: overlay.limiter.as_ref(),
                },
                overlay.block as f64 / f64::from(sample_rate),
            );
            verdict = Err(remarks);
        }
    }
    iamf::report_loudness(loudness);
    println!(
        "  levels       {} samples clipped, the loudest element at {:.2} of full scale with its \
         gain",
        account.clipped, account.peak
    );
    if account.clipped > 0 {
        eprintln!(
            "warning: {} samples clipped; an element's gain took it past full scale",
            account.clipped
        );
    }
    iamf::report_written(config, at, sample_rate)?;
    // The guards' verdict last, as `encode` gives it: a caller reading the
    // tail of a log should find the answer there.
    match verdict {
        Ok(()) => Ok(()),
        Err(remarks) => overlay::verdict(&remarks, &config.out),
    }
}

/// Parameter ticks a block may last and still be decoded to the sample.
///
/// libiamf scales a parameter's durations to the sample clock by
/// `(rate + 0.1) / parameter_rate` and truncates — iamf-rs does the same,
/// after it — so at a parameter rate of the sample rate a block of `d` ticks
/// lasts `⌊d · (1 + 0.1 / rate)⌋` samples: exactly `d` below ten seconds of
/// ticks (480 000 at 48 kHz), one sample more from there, and every block
/// after it starts that much late. An object standing still for minutes as
/// one block put its next move up to tens of samples late; the decoder's
/// positions every 256 showed it a whole step late. No block is that long: a
/// standstill is restated every ten seconds, which costs a few bytes a
/// minute.
const EXACT_SECONDS: u64 = 10;

/// The most whole units of `frame` samples a standstill block covers.
fn longest_still(frame: usize, sample_rate: u32) -> u64 {
    ((EXACT_SECONDS * u64::from(sample_rate) - 1) / frame as u64).max(1)
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
/// block `base + j` is `history[j]`, ramping there across the block from
/// where the block before left it — the path a decoder is given and the one
/// the weights take. Before the programme it is where it starts, and past the
/// last block — the padding of the last unit — where it ended.
fn cluster_subblocks(
    history: &[[f64; 3]],
    base: usize,
    block: usize,
    a: i64,
    b: i64,
    out: &mut Vec<Subblock>,
) {
    out.clear();
    let held = ((base + history.len()) * block) as i64;
    let last = history[history.len() - 1];
    let at = |t: i64, j: usize| -> [f64; 3] {
        let to = history[j - base];
        let from = history[j.saturating_sub(1).max(base) - base];
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
            let j = (t as usize / block).max(base);
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

    /// Before its first update an object is where and as loud as that update
    /// says — its audio from the first sample, as `encode` carries it — and
    /// a ramp the first update asks for has nothing to ramp from.
    #[test]
    fn an_object_plays_from_the_first_sample() {
        let path = Path::new(&[keyframe(171, 0.3, 0.5, 0)]);
        assert_eq!(path.segment(0, &mut 0).at(0).gain, 0.5);
        assert_eq!(path.segment(171, &mut 0).at(171).gain, 0.5);
        assert_eq!(path.origin(), [0.3, 1.0, 0.0]);
        let ramped = Path::new(&[keyframe(171, 0.3, 0.5, 1000)]);
        assert_eq!(ramped.segment(500, &mut 0).at(500).gain, 0.5);
        assert_eq!(ramped.still_until(0, [0.3, 1.0, 0.0], &mut 0), None);
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

    /// No standstill block reaches the duration libiamf's rate scaling
    /// lengthens: at 48 kHz, 117 units of 4096 (479 232 ticks) and 499 of
    /// 960.
    #[test]
    fn a_standstill_block_stays_inside_what_a_decoder_times_exactly() {
        for rate in [44_100u32, 48_000, 96_000] {
            for frame in [480, 960, 1920, 2880, 4096, 4608] {
                let ticks = longest_still(frame, rate) * frame as u64;
                // The decoder's own arithmetic times it to the sample, and
                // one unit more would not be.
                let scaled = |ticks: u64| {
                    (ticks as f64 * ((f64::from(rate) + 0.1) / f64::from(rate))) as u64
                };
                assert_eq!(scaled(ticks), ticks, "{rate} {frame}");
                assert_ne!(
                    scaled(ticks + frame as u64),
                    ticks + frame as u64,
                    "{rate} {frame}"
                );
            }
        }
        assert_eq!(longest_still(4096, 48_000), 117);
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
        cluster_subblocks(&history, 0, 100, -50, 300, &mut out);
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

    /// A history whose early blocks were let go of reads the blocks it kept
    /// exactly as the whole one does.
    #[test]
    fn a_history_let_go_of_reads_the_same() {
        let whole: Vec<[f64; 3]> = (0..600)
            .map(|j| [(j as f64 * 0.01).sin(), 1.0, (j % 7) as f64 / 7.0])
            .collect();
        let mut folded = Folded {
            block: 100,
            base: 0,
            history: whole.clone(),
        };
        folded.forget_before(40_000);
        assert_eq!(folded.base, 399);
        let (mut kept, mut all) = (Vec::new(), Vec::new());
        for a in [40_000i64, 40_050, 45_123, 59_000] {
            cluster_subblocks(&folded.history, folded.base, 100, a, a + 4096, &mut kept);
            cluster_subblocks(&whole, 0, 100, a, a + 4096, &mut all);
            assert_eq!(kept, all, "{a}");
        }
    }

    fn bed(channel: usize, name: &'static str) -> Part {
        Part::Bed {
            channel,
            name,
            position: hz_render::fold::bed_position(name).expect("a bed channel"),
        }
    }

    fn object(channel: usize) -> Part {
        Part::Object {
            channel,
            keyframes: vec![keyframe(0, 0.0, 1.0, 0)],
        }
    }

    /// The bed element is the smallest layout that has the master's bed:
    /// the LFE alone its own expanded layout, a 7.1 a 7.1 with every channel
    /// routed, and a top side — which no layout a decoder renders has — left
    /// to stand at its speaker as an object, the rest still a 7.1.
    #[test]
    fn the_bed_element_is_the_smallest_layout_holding_the_bed() {
        let (plan, left) = plan_bed(&[Part::Lfe { channel: 0 }, object(1)], false);
        assert_eq!(plan.expect("a bed").layout.name, "LFE");
        assert!(left.is_empty());

        let mut seven_one = vec![Part::Lfe { channel: 3 }];
        for (channel, name) in ["L", "R", "C", "Lss", "Rss", "Lrs", "Rrs"]
            .into_iter()
            .enumerate()
        {
            seven_one.push(bed(if channel < 3 { channel } else { channel + 1 }, name));
        }
        let (plan, left) = plan_bed(&seven_one, false);
        let plan = plan.expect("a bed");
        assert_eq!(plan.layout.name, "7.1");
        assert!(left.is_empty());
        // Every master channel lands on the channel of the same name.
        for (channel, slot) in &plan.routes {
            let part = seven_one
                .iter()
                .find(|p| p.channel() == *channel)
                .expect("routed");
            let name = match part {
                Part::Lfe { .. } => "LFE",
                Part::Bed { name, .. } => name,
                Part::Object { .. } => unreachable!(),
            };
            assert_eq!(name_of(plan.layout.labels[*slot]), Some(name));
        }

        let mut seven_one_two = seven_one;
        seven_one_two.push(bed(8, "Lts"));
        seven_one_two.push(bed(9, "Rts"));
        let (plan, left) = plan_bed(&seven_one_two, false);
        assert_eq!(plan.expect("a bed").layout.name, "7.1");
        assert_eq!(left, vec![8, 9]);

        // A 5.1 bed written with the side surrounds is a 5.1: the surrounds
        // are one channel whichever angle names them.
        let five_one = [
            Part::Lfe { channel: 3 },
            bed(0, "L"),
            bed(1, "R"),
            bed(2, "C"),
            bed(4, "Lss"),
            bed(5, "Rss"),
        ];
        assert_eq!(
            plan_bed(&five_one, false).0.expect("a bed").layout.name,
            "5.1"
        );
        // No bed at all, and nothing to plan.
        assert!(plan_bed(&[object(0)], false).0.is_none());
    }

    /// Voices need a front to land on: an LFE-only bed becomes 7.1.4, the
    /// LFE routed into it; a bed with the front three keeps its layout.
    #[test]
    fn voices_go_into_the_masters_bed_or_a_seven_one_four() {
        let (plan, _) = plan_bed(&[Part::Lfe { channel: 0 }, object(1)], true);
        let plan = plan.expect("a bed");
        assert_eq!(plan.layout.name, "7.1.4");
        assert_eq!(plan.routes, vec![(0, 3)]);
        let (plan, _) = plan_bed(&[object(0)], true);
        assert_eq!(plan.expect("a bed").layout.name, "7.1.4");
        let front = [
            Part::Lfe { channel: 3 },
            bed(0, "L"),
            bed(1, "R"),
            bed(2, "C"),
        ];
        assert_eq!(plan_bed(&front, true).0.expect("a bed").layout.name, "5.1");
        let stereo = [bed(0, "L"), bed(1, "R")];
        assert_eq!(
            plan_bed(&stereo, false).0.expect("a bed").layout.name,
            "2.0"
        );
        assert_eq!(
            plan_bed(&stereo, true).0.expect("a bed").layout.name,
            "7.1.4"
        );
    }

    /// Every layout's channels have a place in the room under the label the
    /// room knows them by, so the voices and the measurement can render onto
    /// any of them.
    #[test]
    fn every_bed_layout_renders_in_the_room() {
        for layout in hz_iamf::layout::LOUDSPEAKER_LAYOUTS {
            let room = Layout {
                name: layout.name,
                speakers: layout.labels.iter().map(|label| canonical(label)).collect(),
            };
            assert!(hz_render::Room::new(&room).is_ok(), "{}", layout.name);
            for label in layout.labels {
                assert!(name_of(label).is_some(), "{} in {}", label, layout.name);
            }
        }
    }

    /// The header's profile, as the writer decides it.
    #[test]
    fn the_profile_is_named_as_the_writer_decides_it() {
        let object = Element::Object {
            kind: PositionKind::Cart16,
            default: [0.0, 1.0, 0.0],
        };
        assert_eq!(profile_name(&[object; 5]), "base-advanced");
        let mut with_bed = vec![Element::Channels(hz_iamf::layout::LFE)];
        with_bed.extend([object; 13]);
        assert_eq!(profile_name(&with_bed), "advanced-1");
        let mut wide = vec![Element::Channels(hz_iamf::layout::SEVEN_ONE_FOUR)];
        wide.extend([object; 13]);
        assert_eq!(profile_name(&wide), "advanced-2");
    }
}
