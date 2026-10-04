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
//!   panned onto them — the computation of [`hz_programme::overlay`], the one
//!   `encode --overlay` writes into a TrueHD stream;
//! - **`--voices-to-bed K`**: the master's last K objects rendered on the
//!   room's cube into a dialogue element of their own, which the mix
//!   presentation labels, lets a listener turn up or down and anchors its
//!   loudness on; the rest carried as objects.
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

use crate::iamf::{self, Config, Measure, Output};
use hz_cluster::scene::Source as SceneSource;
use hz_core::{Error, Result, speakers};
use hz_iamf::{Animation, Element, PositionBlock, PositionKind, Subblock};
use hz_io::container::SampleFormat;
use hz_programme::fold::Placer;
use hz_programme::follow::Following;
use hz_programme::mix::{Mixer, Mixing};
use hz_programme::overlay::{self, Overlaid};
use hz_programme::path::Path;
use hz_programme::progress::Progress;
use hz_programme::quantise::{Levels, Quantiser};
use hz_programme::source::Source;
use hz_programme::{Part, parts_of};
use hz_render::{Keyframe, Layout, Mixdown};
use std::collections::VecDeque;
use std::io::Write;

/// Samples between two traced positions.
const TRACE_INTERVAL: usize = 256;

/// The most channels an IA sequence carries: IAMF v2.0's advanced-2.
const BUDGET: usize = 28;

/// How far either way a listener may move the dialogue element, in decibels:
/// its gain offset's range, about 0 dB.
const DIALOGUE_RANGE: f64 = 12.0;

/// Under this an element's gain is silence — a hundred decibels down — and
/// a decoder playing it plays nothing a source could be heard through.
const MUTED: f64 = 1e-5;

/// The subblocks of `[a, b)` of an object on `path`, sample times that may start before the
/// programme does — the codec's delay puts the first unit's start there,
/// and the object is where it begins until the programme does.
fn path_subblocks(path: &Path, a: i64, b: i64, hint: &mut usize, out: &mut Vec<Subblock>) {
    out.clear();
    let mut t = a;
    while t < b {
        let (end, from, to) = if t < 0 {
            let p = path.origin();
            (b.min(0), p, p)
        } else {
            let segment = *path.segment(t as u64, hint);
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

/// The bed element for `parts`, and the bed channels it has no channel for
/// (as indices into `parts`).
fn plan_bed(parts: &[Part]) -> (Option<BedPlan>, Vec<usize>) {
    let names: Vec<(usize, &'static str)> = parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| match part {
            Part::Lfe { .. } => Some((index, "LFE")),
            Part::Bed { name, .. } => Some((index, *name)),
            Part::Object { .. } => None,
        })
        .collect();
    let layout = layout_for(&names.iter().map(|(_, name)| *name).collect::<Vec<_>>());
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

/// Points a move between two of a voice's updates is looked at, beside its
/// ends: where the panning between two places reaches a speaker neither end
/// does.
const ALONG_A_MOVE: usize = 8;

/// The dialogue element's layout: the smallest of IAMF's loudspeaker layouts
/// that **holds** every place the voices go.
///
/// # What holds a place
///
/// A layout holds a position when every speaker the room's cube pans it onto
/// on 7.1.4 — the layout this workspace renders on and measures on — is a
/// speaker of that layout, at the same label: then the voice rendered into the
/// layout and the layout played on 7.1.4 is the voice rendered on 7.1.4, and
/// nothing is lost to the smaller layout. A voice at the centre is held by
/// mono; hard left, hard right and the centre by 5.1 — IAMF's 3.0 would hold
/// them in three channels, and is an expanded layout no decoder to hand
/// renders; the side and rear surrounds by 7.1 (not by 5.1, whose surrounds are
/// at ±110°, between 7.1.4's); a height by 7.1.2 or 7.1.4. 7.1.4 holds
/// everything, and is what is left when nothing smaller does.
///
/// # Which places
///
/// Every place the voices' updates put them, and the places between two
/// updates in a row — a voice that moves passes through them — at
/// [`ALONG_A_MOVE`] points a move. A dub's voices are placed once, at the
/// original's dialogue, so this is usually a handful of points.
fn dialogue_layout(parts: &[Part], voices: &[usize]) -> hz_iamf::Layout {
    let room_714 = Layout::surround_7_1_4();
    let room = hz_render::Room::new(&room_714).expect("7.1.4 is in the room");
    let mut gains = vec![0.0; room_714.channels()];
    let mut used = vec![false; room_714.channels()];
    for &voice in voices {
        let Part::Object { keyframes, .. } = &parts[voice] else {
            continue;
        };
        let mut keyframes = keyframes.clone();
        keyframes.sort_by_key(|k| k.sample_pos);
        let mut last: Option<[f64; 3]> = None;
        for keyframe in &keyframes {
            let to = keyframe.position;
            let from = last.unwrap_or(to);
            for step in 0..=ALONG_A_MOVE {
                let a = step as f64 / ALONG_A_MOVE as f64;
                let at = [0, 1, 2].map(|i| from[i] + (to[i] - from[i]) * a);
                room.gains(at, &mut gains);
                for (used, gain) in used.iter_mut().zip(&gains) {
                    *used |= gain.abs() > 1e-9;
                }
            }
            last = Some(to);
        }
    }
    hz_iamf::layout::LOUDSPEAKER_LAYOUTS
        .iter()
        .find(|layout| {
            room_714
                .speakers
                .iter()
                .zip(&used)
                .all(|(label, used)| !used || layout.index_of(label).is_some())
        })
        .copied()
        .unwrap_or(hz_iamf::layout::SEVEN_ONE_FOUR)
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
    /// The coded elements' samples: the loudest, and how many were clamped.
    levels: Levels,
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
            "`--overlay` and `--voices-to-bed` at once; one pans the voices onto the master's \
             elements and the other gives them an element of their own",
        )),
        (Some(sources), None) => run_overlay(&config, kind, source, parts, sources),
        (None, voices) => run_plain(&config, kind, source, parts, voices.unwrap_or(0)),
    }
}

/// The master as it is, or with its last `voices` objects rendered into a
/// dialogue element of their own beside it.
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
    let (bed, left) = plan_bed(&parts);
    let bed_width = bed.as_ref().map_or(0, |bed| bed.layout.channels());
    let dialogue = (!rendered.is_empty()).then(|| dialogue_layout(&parts, &rendered));
    let dialogue_width = dialogue.as_ref().map_or(0, hz_iamf::Layout::channels);

    // How many object elements there may be: what is asked, or what is left
    // of the budget beside the bed and dialogue elements.
    let room = BUDGET - bed_width - dialogue_width;
    let allowed = config.elements.unwrap_or(room);
    if allowed > room || allowed < hz_cluster::MIN_ELEMENTS {
        return Err(Error::unsupported(
            path,
            format!(
                "{allowed} object elements; an IA sequence carries {} to {room}{}",
                hz_cluster::MIN_ELEMENTS,
                match (&bed, &dialogue) {
                    (Some(bed), Some(dialogue)) => format!(
                        " beside a {} bed element and a {} dialogue element",
                        bed.layout.name, dialogue.name
                    ),
                    (Some(bed), None) => format!(" beside a {} bed element", bed.layout.name),
                    (None, Some(dialogue)) =>
                        format!(" beside a {} dialogue element", dialogue.name),
                    (None, None) => String::new(),
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
    // The dialogue last: its channels after every object element's.
    let dialogue_first = bed_width + carried.len();
    if let Some(layout) = dialogue {
        elements.push(Element::Channels(layout));
    }

    let fill = BedFill {
        routes: bed.as_ref().map_or_else(Vec::new, |bed| bed.routes.clone()),
        width: bed_width,
        dialogue: match dialogue {
            Some(layout) => Some((dialogue_first, Voices::new(&layout, &parts, &rendered)?)),
            None => None,
        },
    };
    // A fold's elements are mixes, rounded as an overlay's mixed elements
    // are: to `--fold-depth`, and never finer than the stream.
    let depth = config.mixing.fold_depth.min(config.bits);
    if folding && !(16..=24).contains(&config.mixing.fold_depth) {
        return Err(Error::unsupported(
            path,
            format!(
                "a fold depth of {} bits; an IA sequence's mixed elements are rounded to 16 to 24",
                config.mixing.fold_depth
            ),
        ));
    }
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
        Feed::Folded(Box::new(Folding::new(
            path,
            fill,
            sources,
            allowed,
            block,
            bed_width + carried.len() + dialogue_width,
            source.sample_rate(),
            &config.mixing,
            Quantiser::bits(config.bits).stepped(f64::from(1u32 << (config.bits - depth))),
        )?))
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
    if let Some(layout) = &dialogue {
        parts_said.push(format!(
            "{} {} dialogue element",
            // Read aloud, only an eight (or an L-F-E) takes "an".
            if layout.name.starts_with('8') {
                "an"
            } else {
                "a"
            },
            layout.name
        ));
        lines.push(format!(
            "  dialogue     the last {} rendered into a {} element of their own on the room's \
             cube, the smallest layout that holds where they go; a listener may move it \
             {DIALOGUE_RANGE} dB either way",
            plural(voices, "object", "objects"),
            layout.name,
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
        dialogue: dialogue.map(|layout| (dialogue_first, layout)),
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
        "the bed as {} {} element{}",
        if bed.layout.name.starts_with(['8', 'L']) {
            "an"
        } else {
            "a"
        },
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
/// `encode --overlay`'s computation — see [`hz_programme::overlay`] — written as
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
    let mixing = &config.mixing;
    let kept = overlay::kept(path, parts.len(), sources)?;
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
    if !(16..=24).contains(&mixing.fold_depth) {
        return Err(Error::unsupported(
            path,
            format!(
                "a fold depth of {} bits; an IA sequence's mixed elements are rounded to 16 to 24",
                mixing.fold_depth
            ),
        ));
    }
    options.check(path)?;

    // The bed element, from the kept bed channels; a source is never a bed.
    let (bed, left) = plan_bed(&parts[..kept]);
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
    let slots = overlay::spare_slots(
        &overlay::origins(&parts[kept..]),
        BUDGET - channels,
        options.spare,
    );
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
    let beds = overlay::beds_among(&overlay::described(&parts[..kept]), |position| {
        kind.quantise(position)
    });
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
    let depth = mixing.fold_depth.min(config.bits);
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
                // As `encode` states them — see [`Part::initial`].
                state: part.initial(),
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
        pending: Pending::new(width),
        exhausted: false,
        at: 0,
        mixer: Mixer::new(
            f64::from(sample_rate),
            mixing.weighing,
            mixing.dialnorm,
            mixing.headroom.limits(),
            engine_width,
        )
        .map_err(|why| Error::unsupported(path, why.to_string()))?,
        mixed: Quantiser::bits(config.bits).stepped(f64::from(1u32 << (config.bits - depth))),
        levels: Levels::default(),
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
        dialogue: None,
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

/// The voices `--voices-to-bed` renders into the dialogue element: panned on
/// the room's cube onto its layout, each moved at its own updates and
/// ramping its gains over the samples each asks for — the render the bed mode
/// makes, onto a different layout.
struct Voices {
    following: Following,
    /// The master channel each voice is.
    channels: Vec<usize>,
    /// One block of the voices' samples, interleaved, and what the mixdown
    /// makes of them, both reused.
    input: Vec<f32>,
    rendered: Vec<f64>,
}

impl Voices {
    fn new(layout: &hz_iamf::Layout, parts: &[Part], voices: &[usize]) -> Result<Self> {
        let room = Layout {
            name: layout.name,
            speakers: layout.labels.iter().map(|label| canonical(label)).collect(),
        };
        let mut following = Following::new(Mixdown::new(&room)?);
        let channels = voices
            .iter()
            .enumerate()
            .map(|(index, &part)| {
                let Part::Object { channel, keyframes } = &parts[part] else {
                    unreachable!("a voice is an object")
                };
                following.object(index, keyframes);
                *channel
            })
            .collect();
        Ok(Self {
            following,
            channels,
            input: Vec::new(),
            rendered: Vec::new(),
        })
    }

    fn channels(&self) -> usize {
        self.following.channels()
    }

    /// The voices of `got` frames from sample `at`, rendered into
    /// `self.rendered`, the dialogue element's width a frame.
    fn render(&mut self, raw: &[i32], stride: usize, got: usize, at: u64, input_scale: f64) {
        let voices = self.channels.len();
        let width = self.following.channels();
        self.input.clear();
        for frame in raw[..got * stride].chunks_exact(stride) {
            self.input.extend(
                self.channels
                    .iter()
                    .map(|&channel| (f64::from(frame[channel]) * input_scale) as f32),
            );
        }
        self.rendered.clear();
        self.rendered.resize(got * width, 0.0);
        self.following
            .render(&self.input, voices, got, at, &mut self.rendered);
    }
}

/// The channel elements: the bed's — the master's bed channels where its
/// layout has them, silence where it does not — and the dialogue's, the
/// voices rendered, when there is one.
struct BedFill {
    /// `(master channel, layout channel)`.
    routes: Vec<(usize, usize)>,
    width: usize,
    /// The dialogue element's first channel in a pushed frame, and the
    /// voices it renders.
    dialogue: Option<(usize, Voices)>,
}

/// How samples are scaled in and out: the master's integers to ±1, and ±1
/// to the codec's.
#[derive(Debug, Clone, Copy)]
struct Scale {
    input: f64,
    output: Quantiser,
}

impl Scale {
    /// `x`, at ±1, into the codec's integers: rounded, clamped, and counted
    /// when it had to be.
    #[inline]
    fn code(&self, x: f64, account: &mut Account) -> i32 {
        self.output.code(x, &mut account.levels)
    }
}

impl BedFill {
    /// `got` frames of the bed into its channels of `q`, `width` channels a
    /// frame — the master's samples as they are — and of the dialogue into
    /// its own.
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
        for frame in q[..got * width].chunks_exact_mut(width) {
            frame[..self.width].fill(0);
        }
        for &(channel, slot) in &self.routes {
            for n in 0..got {
                let x = f64::from(raw[n * stride + channel]) * scale.input;
                q[n * width + slot] = scale.code(x, account);
            }
        }
        if let Some((first, voices)) = &mut self.dialogue {
            voices.render(raw, stride, got, at, scale.input);
            let channels = voices.channels();
            for (n, frame) in voices.rendered.chunks_exact(channels).take(got).enumerate() {
                for (c, &x) in frame.iter().enumerate() {
                    q[n * width + *first + c] = scale.code(x, account);
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

/// One block a fold has read and not yet folded: the master's samples, and
/// each source's samples with its gain on them and where it is at the
/// block's end — what the look-ahead weighed it by, kept so that the fold
/// weighs the same thing.
#[derive(Default)]
struct Read {
    at: u64,
    frames: usize,
    raw: Vec<i32>,
    signals: Vec<Vec<f32>>,
    /// Where each source is, and where it is pinned when it is a bed channel.
    places: Vec<([f64; 3], Option<[f64; 3]>)>,
}

impl Read {
    /// Source `index` of this block, as a scene takes it.
    fn source(&self, index: usize) -> SceneSource {
        let (position, pinned) = self.places[index];
        SceneSource {
            position,
            pinned,
            mode: if pinned.is_some() {
                hz_cluster::class::BED
            } else {
                hz_render::Mode::default()
            },
            ..SceneSource::default()
        }
    }

    /// Every source of the block into `scene`, started and finished.
    fn weigh(&self, scene: &mut hz_cluster::scene::Scene) {
        scene.start();
        for (index, signal) in self.signals.iter().enumerate() {
            scene.push(&self.source(index), signal.iter().map(|&x| f64::from(x)));
        }
        scene.finish();
    }
}

/// Frames decided a block at a time and handed out a unit at a time.
struct Pending {
    frames: Vec<i32>,
    /// The first frame not yet handed out, and how many there are.
    from: usize,
    count: usize,
    width: usize,
}

impl Pending {
    fn new(width: usize) -> Self {
        Self {
            frames: Vec::new(),
            from: 0,
            count: 0,
            width,
        }
    }

    /// Room for `frames` more frames after the ones held, silent.
    fn block(&mut self, frames: usize) -> &mut [i32] {
        let width = self.width;
        if self.from > 0 {
            let from = self.from * width;
            self.frames.copy_within(from..from + self.count * width, 0);
            self.from = 0;
        }
        let base = self.count * width;
        self.frames.resize(base + frames * width, 0);
        self.frames[base..].fill(0);
        self.count += frames;
        &mut self.frames[base..]
    }

    /// Up to `want` of the frames held into `q`: how many.
    fn take(&mut self, want: usize, q: &mut [i32]) -> usize {
        let got = want.min(self.count);
        let width = self.width;
        let from = self.from * width;
        q[..got * width].copy_from_slice(&self.frames[from..from + got * width]);
        self.from += got;
        self.count -= got;
        got
    }
}

/// More objects than elements: folded into the elements block by block with
/// `hz-cluster` — where each element goes, and how much of each object it
/// carries.
///
/// The same fold the TrueHD encode makes, from the same crates: each block
/// placed by the energies of the window around it ([`Placer`]), which needs
/// the blocks ahead read before it is folded — so a fold reads its own
/// blocks, [`hz_cluster::smooth::SMOOTHING`]'s look-ahead in front of what it
/// folds, and hands the folded frames out a unit at a time. The mix's
/// options are the command's: the dialnorm's floor, the weighing, the
/// headroom rule, and the depth the mixed elements are rounded to.
struct Folding {
    bed: BedFill,
    sources: Vec<(usize, Pooled)>,
    placer: Placer,
    mixer: Mixer,
    /// What each block read weighs, for the look-ahead: its own scene,
    /// because a weighted scene's filter has state and this one reads ahead
    /// of the fold's.
    ahead: hz_cluster::scene::Scene,
    read: VecDeque<Read>,
    spare: Vec<Read>,
    /// Samples a block, and samples read from the master.
    block: usize,
    at: u64,
    exhausted: bool,
    pending: Pending,
    /// What a mixed sample is rounded to, in the codec's integers.
    mixed: Quantiser,
}

impl Folding {
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: &std::path::Path,
        bed: BedFill,
        sources: Vec<(usize, Pooled)>,
        count: usize,
        block: usize,
        width: usize,
        sample_rate: u32,
        mixing: &Mixing,
        mixed: Quantiser,
    ) -> Result<Self> {
        use hz_cluster::{Clusterer, Weighting};
        let fail = |why: hz_core::Error| Error::unsupported(path, why.to_string());
        let rate = f64::from(sample_rate);
        let mut mixer = Mixer::new(
            rate,
            mixing.weighing,
            mixing.dialnorm,
            mixing.headroom.limits(),
            count,
        )
        .map_err(fail)?;
        mixer.gains = vec![1.0; sources.len()];
        Ok(Self {
            bed,
            sources,
            placer: Placer::new(
                Clusterer::new(count, Weighting::Fitted)
                    .map_err(fail)?
                    .flooring(hz_cluster::floor::Floor::for_dialnorm(mixing.dialnorm))
                    .bounding(mixing.headroom.bounds()),
                hz_cluster::smooth::SMOOTHING,
            ),
            mixer,
            ahead: hz_cluster::scene::Scene::weighed(rate, mixing.weighing).map_err(fail)?,
            read: VecDeque::new(),
            spare: Vec::new(),
            block,
            at: 0,
            exhausted: false,
            pending: Pending::new(width),
            mixed,
        })
    }

    /// Up to `want` frames of every element into `q`: as many blocks read and
    /// folded as it takes, the rest held for the next unit. Fewer only at the
    /// end.
    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        source: &mut Source,
        want: usize,
        scale: &Scale,
        q: &mut [i32],
        carried: &mut [Carried],
        account: &mut Account,
    ) -> Result<usize> {
        let stride = source.channels();
        while self.pending.count < want {
            if !self.exhausted {
                self.read_block(source, stride, scale)?;
            }
            // A block is folded once the blocks its window reaches ahead are
            // in, or there are no more to come.
            if self.read.len() > self.placer.look_ahead() || self.exhausted {
                let Some(read) = self.read.pop_front() else {
                    break;
                };
                self.fold(read, stride, scale, carried, account)?;
            }
        }
        Ok(self.pending.take(want, q))
    }

    /// Read the next block, whole whatever `--frames` says, and record what
    /// it weighs.
    fn read_block(&mut self, source: &mut Source, stride: usize, scale: &Scale) -> Result<()> {
        let mut read = self.spare.pop().unwrap_or_default();
        read.raw.resize(self.block * stride, 0);
        let frames = source.fill(&mut read.raw, self.block)?;
        if frames < self.block {
            self.exhausted = true;
        }
        if frames == 0 {
            self.spare.push(read);
            return Ok(());
        }
        let t0 = self.at;
        read.at = t0;
        read.frames = frames;
        read.signals.resize_with(self.sources.len(), Vec::new);
        read.places.clear();
        for (signal, (channel, pooled)) in read.signals.iter_mut().zip(&mut self.sources) {
            signal.clear();
            let raw = &read.raw;
            read.places.push(match pooled {
                Pooled::Pinned(position) => {
                    signal.extend(
                        (0..frames)
                            .map(|n| (f64::from(raw[n * stride + *channel]) * scale.input) as f32),
                    );
                    (*position, Some(*position))
                }
                Pooled::Moving(object_path, hint) => {
                    signal.extend((0..frames).map(|n| {
                        let gain = object_path.gain(t0 + n as u64, hint);
                        (f64::from(raw[n * stride + *channel]) * scale.input * gain) as f32
                    }));
                    let end = t0 + frames as u64 - 1;
                    (object_path.segment(end, hint).at(end).position, None)
                }
            });
        }
        read.weigh(&mut self.ahead);
        let energies: Vec<f64> = self.ahead.objects().iter().map(|o| o.energy).collect();
        self.placer.record(&energies);
        self.at += frames as u64;
        self.read.push_back(read);
        Ok(())
    }

    /// Fold one block read into the frames waiting to be handed out, and
    /// append each element's place at its end to its history.
    fn fold(
        &mut self,
        mut read: Read,
        stride: usize,
        scale: &Scale,
        carried: &mut [Carried],
        account: &mut Account,
    ) -> Result<()> {
        let frames = read.frames;
        read.weigh(&mut self.mixer.scene);
        let clustering = self
            .placer
            .place(self.mixer.scene.objects())
            .map_err(|why| Error::unsupported(std::path::Path::new("the fold"), why.to_string()))?;
        std::mem::swap(&mut self.mixer.signals, &mut read.signals);
        self.mixer.fold(
            &clustering,
            self.placer.previous(),
            frames,
            scale.output.ceiling(),
        );
        self.mixer.judge(&clustering);
        std::mem::swap(&mut self.mixer.signals, &mut read.signals);

        let width = self.pending.width;
        let first = self.bed.width;
        let out = self.pending.block(frames);
        self.bed.fill(
            &read.raw, stride, frames, read.at, scale, out, width, account,
        );
        for (element, signal) in self.mixer.mixed.iter().enumerate() {
            for n in 0..frames {
                out[n * width + first + element] =
                    self.mixed.code(f64::from(signal[n]), &mut account.levels);
            }
        }
        // Where each element is going, for the blocks and the measurement,
        // which ramp there across the block.
        for (element, at_end) in clustering.positions.iter().enumerate() {
            if let Placement::Folded(folded) = &mut carried[element].placement {
                folded.history.push(*at_end);
            }
        }
        self.placer.keep(clustering);
        self.spare.push(read);
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
    /// Frames decided and not yet handed out.
    pending: Pending,
    exhausted: bool,
    /// Samples read from the master.
    at: u64,
    mixer: Mixer,
    /// What a mixed sample is rounded to, in the codec's integers, and what
    /// the mixed samples came to.
    mixed: Quantiser,
    levels: Levels,
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
        while self.pending.count < want && !self.exhausted {
            self.decide(source, stride, scale, account)?;
        }
        Ok(self.pending.take(want, q))
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
        let frames = source.fill(&mut self.raw, block)?;
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
            hz_programme::path::advance(keyframes, &mut live.next, &mut live.state, end);
        }

        // The scene, and each object's samples: every track but the LFE, the
        // kept elements first and the sources after them, as `encode` builds
        // it. The scene weighs each as the master states it; the mix takes an
        // element with its gain already in its samples, since that is where
        // an IAMF object carries it, and a source at its stated gain.
        let kept = self.overlaid.elements;
        self.mixer.scene.start();
        self.mixer.gains.clear();
        let objects = self.parts.len() - self.first;
        self.mixer.signals.resize_with(objects, Vec::new);
        for (object, part_index) in (self.first..self.parts.len()).enumerate() {
            let state = self.live[part_index].state;
            let channel = self.parts[part_index].channel();
            let signal = &mut self.mixer.signals[object];
            signal.clear();
            signal.extend(
                (0..frames)
                    .map(|n| (f64::from(self.raw[n * stride + channel]) * scale.input) as f32),
            );
            self.mixer.scene.push(
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
            self.mixer.gains.push(state.gain);
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
        self.mixer.scene.finish();

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
        self.mixer
            .overlay(
                &mut self.overlaid,
                overlay::Block {
                    first: self.first,
                    elements,
                    frames,
                },
                self.mixed.ceiling(),
            )
            .map_err(|e| Error::io(path, e))?;

        // Out, into the frames waiting to be handed out: a copied element as
        // the master's samples with its gain, a mixed one rounded to the fold
        // depth, and a bed channel the master has not got silent.
        let width = self.width;
        let out = self.pending.block(frames);
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
                    let signal = &self.mixer.mixed[element];
                    for n in 0..frames {
                        out[n * width + channel] =
                            self.mixed.code(f64::from(signal[n]), &mut self.levels);
                    }
                }
            }
        }
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
        match self {
            Self::Overlaid(overlay) => {
                return overlay.fill(source, stride, want, scale, q, account);
            }
            Self::Folded(folding) => return folding.fill(source, want, scale, q, carried, account),
            Self::Direct(_) => {}
        }
        raw.resize(want * stride, 0);
        let got = source.fill(raw, want)?;
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
            Self::Folded(_) | Self::Overlaid(_) => unreachable!("handed out above"),
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
    /// The dialogue element's first channel in a pushed frame and its layout,
    /// when the voices have one: labelled, given a gain a listener may move,
    /// and the loudness anchored on it.
    dialogue: Option<(usize, hz_iamf::Layout)>,
}

/// Route a channel element's channels, from `first` in a pushed frame, onto
/// 7.1.4 as the measurement renders them: each to its speaker, or placed at
/// its speaker's place where 7.1.4 has none.
fn route_channels(
    mixdown: &mut Mixdown,
    layout: &Layout,
    channels: &hz_iamf::Layout,
    first: usize,
) {
    for (slot, label) in channels.labels.iter().enumerate() {
        let label = canonical(label);
        match layout.index_of(label) {
            Some(channel) => {
                mixdown.speaker(first + slot, channel);
            }
            None => {
                let index = mixdown.object(first + slot);
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
        dialogue,
    } = job;
    let path = config.input.as_path();
    let format = *source.pcm_format();
    let sample_rate = source.sample_rate();
    let scale = Scale {
        input: 1.0 / f64::from(1u32 << (format.bits_per_channel - 1)),
        output: Quantiser::bits(config.bits),
    };
    let width: usize = elements.iter().map(Element::channels).sum();
    debug_assert_eq!(
        width,
        bed_width + carried.len() + dialogue.as_ref().map_or(0, |(_, l)| l.channels())
    );
    let first_object = usize::from(bed.is_some());
    // With a dialogue element the mix says which element is which, gives the
    // dialogue a gain a listener may move, and anchors its loudness on it.
    let presentation = match &dialogue {
        None => hz_iamf::Presentation::default(),
        Some(_) => hz_iamf::Presentation {
            labels: Some(hz_iamf::Labels {
                language: "en-us".into(),
                mix: "Main".into(),
                elements: (0..elements.len())
                    .map(|index| {
                        if index + 1 == elements.len() {
                            "Dialogue"
                        } else {
                            "M&E"
                        }
                        .to_string()
                    })
                    .collect(),
            }),
            gain_offsets: vec![(
                elements.len() - 1,
                hz_iamf::GainOffset::Range {
                    default: 0.0,
                    min: -DIALOGUE_RANGE,
                    max: DIALOGUE_RANGE,
                },
            )],
            anchor: Some(hz_iamf::Anchor::Dialogue),
        },
    };
    let writer = iamf::open_presenting(config, sample_rate, elements.clone(), presentation)?;
    let delay = writer.delay() as i64;

    // The measurement renders what a decoder is handed, on 7.1.4: the bed
    // element's channels to their speakers, every object on the room's cube.
    let layout = Layout::surround_7_1_4();
    let mut mixdown = Mixdown::new(&layout)?;
    if let Some(bed) = &bed {
        route_channels(&mut mixdown, &layout, bed, 0);
    }
    // The dialogue alone, rendered the same way, for the loudness anchored
    // on it; and within the whole mix, beside everything else.
    let mut dialogue_measure = match &dialogue {
        Some((first, channels)) => {
            route_channels(&mut mixdown, &layout, channels, *first);
            let mut alone = Mixdown::new(&layout)?;
            route_channels(&mut alone, &layout, channels, *first);
            Some((alone, Measure::new(sample_rate)))
        }
        None => None,
    };
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
                    *f = (f64::from(q) / scale.output.full_scale()) as f32;
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
                            path_subblocks(&moving.path, a, b, &mut moving.blocks, subblocks);
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
                if let Some((alone, dialogue)) = dialogue_measure.as_mut() {
                    rendered[..got * layout.channels()].fill(0.0);
                    alone.render(
                        &input[..got * width],
                        width,
                        got,
                        &mut rendered[..got * layout.channels()],
                    );
                    for (m, &r) in measured
                        .iter_mut()
                        .zip(&rendered[..got * layout.channels()])
                    {
                        *m = r as f32;
                    }
                    dialogue.push(&measured[..got * layout.channels()]);
                }

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
    let anchored = dialogue_measure.map(|(_, dialogue)| {
        let [stereo, native] = dialogue.finish();
        [stereo.integrated, native.integrated]
    });
    let units = writer.units();
    writer
        .finish_anchored(loudness, anchored)
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
        Feed::Folded(folding) => {
            let smoothing = folding.placer.smoothing();
            println!(
                "  fold         {:.4} mean over the presentations, {:.3} at its worst, as a \
                 fraction of the object's own gains; blocks of {}, steadied by a hold over {} \
                 behind and {} ahead",
                folding.mixer.cost.mean(),
                folding.mixer.cost.worst,
                folding.block,
                smoothing.behind,
                smoothing.ahead
            );
            println!(
                "  floor        {:.1} dBFS at the playback level a dialnorm of {:.0} implies, \
                 {:.2} objects a block under it",
                folding.mixer.floor.threshold_dbfs(),
                config.mixing.dialnorm,
                folding.mixer.cost.inaudible as f64 / folding.mixer.cost.blocks.max(1) as f64
            );
            println!(
                "  headroom     {} blocks where an element's coherent peak was bounded in the \
                 fit, {}; the elements rounded to {} bits",
                folding.placer.bounded(),
                match &folding.mixer.limiter {
                    Some(limiter) => format!(
                        "{} where the limiter brought every element down, the mix peaking at \
                         {:.2} of full scale before it",
                        limiter.limited, limiter.peak
                    ),
                    None => "no limiter".to_string(),
                },
                config.bits - folding.mixed.step().log2() as u32
            );
        }
        Feed::Overlaid(overlay) => {
            let summary = overlay_summary.as_ref().expect("an overlay's summary");
            let overlaid = &overlay.overlaid;
            println!(
                "  floor        {:.1} dBFS at the playback level a dialnorm of {:.0} implies; a \
                 source was above it in {:.1} % of the blocks, and is guarded in those",
                overlay.mixer.floor.threshold_dbfs(),
                config.mixing.dialnorm,
                100.0 * overlaid.voiced as f64 / overlay.mixer.cost.blocks.max(1) as f64
            );
            overlay::summary(overlaid, overlay.mixer.cost.blocks, summary.beds_first);
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
                match &overlay.mixer.limiter {
                    Some(limiter) => format!(
                        "{} blocks where the limiter brought every element down, the mix \
                         peaking at {:.2} of full scale before it",
                        limiter.limited, limiter.peak
                    ),
                    None => "no limiter".to_string(),
                },
                if overlay.levels.clipped > 0 {
                    format!(
                        "; {} mixed samples went outside the codec's domain and were clamped",
                        overlay.levels.clipped
                    )
                } else {
                    String::new()
                }
            );
            let remarks = overlay::remarks(
                overlaid,
                &overlay::Totals {
                    blocks: overlay.mixer.cost.blocks,
                    mean: overlay.mixer.cost.sum,
                    clipped: overlay.levels.clipped,
                    limiter: overlay.mixer.limiter.as_ref(),
                },
                overlay.block as f64 / f64::from(sample_rate),
            );
            verdict = Err(remarks);
        }
    }
    iamf::report_loudness(loudness);
    if let Some([stereo, native]) = anchored {
        println!(
            "  anchored     the dialogue element alone at {} on 7.1.4, {} on stereo, stated as \
             the loudness anchored on dialogue",
            iamf::lkfs(native),
            iamf::lkfs(stereo)
        );
    }
    println!(
        "  levels       {} samples clipped, the loudest element at {:.2} of full scale with its \
         gain",
        account.levels.clipped, account.levels.peak
    );
    if account.levels.clipped > 0 {
        eprintln!(
            "warning: {} samples clipped; an element's gain took it past full scale",
            account.levels.clipped
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

    /// Frames decided a block at a time come out a unit at a time, in order,
    /// whatever the two sizes are.
    #[test]
    fn pending_frames_come_out_in_order_across_blocks() {
        let width = 2;
        let mut pending = Pending::new(width);
        let mut next = 0i32;
        let mut seen = Vec::new();
        let mut q = vec![0i32; 7 * width];
        for _ in 0..5 {
            for sample in pending.block(3) {
                *sample = next;
                next += 1;
            }
            while pending.count >= 7 {
                let got = pending.take(7, &mut q);
                seen.extend_from_slice(&q[..got * width]);
            }
        }
        let got = pending.take(7, &mut q);
        seen.extend_from_slice(&q[..got * width]);
        assert_eq!(seen, (0..next).collect::<Vec<_>>());
        assert_eq!(pending.count, 0);
    }

    /// A unit's subblocks break where the path does, a standstill is one
    /// step, and the time before the programme holds the origin.
    #[test]
    fn subblocks_break_where_the_path_does() {
        let path = Path::new(&[keyframe(0, -1.0, 1.0, 0), keyframe(300, 1.0, 1.0, 400)]);
        let mut out = Vec::new();
        path_subblocks(&path, -312, 648, &mut 0, &mut out);
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
        let (plan, left) = plan_bed(&[Part::Lfe { channel: 0 }, object(1)]);
        assert_eq!(plan.expect("a bed").layout.name, "LFE");
        assert!(left.is_empty());

        let mut seven_one = vec![Part::Lfe { channel: 3 }];
        for (channel, name) in ["L", "R", "C", "Lss", "Rss", "Lrs", "Rrs"]
            .into_iter()
            .enumerate()
        {
            seven_one.push(bed(if channel < 3 { channel } else { channel + 1 }, name));
        }
        let (plan, left) = plan_bed(&seven_one);
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
        let (plan, left) = plan_bed(&seven_one_two);
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
        assert_eq!(plan_bed(&five_one).0.expect("a bed").layout.name, "5.1");
        // No bed at all, and nothing to plan.
        assert!(plan_bed(&[object(0)]).0.is_none());
    }

    fn voice(channel: usize, positions: &[[f64; 3]]) -> Part {
        Part::Object {
            channel,
            keyframes: positions
                .iter()
                .enumerate()
                .map(|(n, &position)| Keyframe {
                    sample_pos: n as u64 * 48_000,
                    position,
                    ..Keyframe::default()
                })
                .collect(),
        }
    }

    /// The dialogue element is the smallest layout that holds where the
    /// voices go: the centre alone is mono, the front three 5.1 (3.0 is an
    /// expanded layout no decoder to hand renders), a 7.1's places 7.1 —
    /// not 5.1, whose surrounds are not 7.1.4's — a front height 3.1.2, or 7.1.2
    /// beside the side surrounds, a top rear
    /// 7.1.4, and a voice moving from the left front to the left rear passes
    /// the side surround on the way.
    #[test]
    fn the_dialogue_element_is_the_smallest_layout_holding_the_voices() {
        let name = |voices: &[&[[f64; 3]]]| {
            let parts: Vec<Part> = voices
                .iter()
                .enumerate()
                .map(|(channel, positions)| voice(channel, positions))
                .collect();
            dialogue_layout(&parts, &(0..parts.len()).collect::<Vec<_>>()).name
        };
        let at = |label: &str| hz_render::fold::bed_position(label).expect("a speaker");
        assert_eq!(name(&[&[at("C")]]), "1.0");
        assert_eq!(name(&[&[at("L")], &[at("R")]]), "2.0");
        assert_eq!(name(&[&[at("L")], &[at("C")], &[at("R")]]), "5.1");
        let seven_one: Vec<[[f64; 3]; 1]> = ["L", "C", "R", "Rss", "Rrs", "Lrs", "Lss"]
            .iter()
            .map(|label| [at(label)])
            .collect();
        let refs: Vec<&[[f64; 3]]> = seven_one.iter().map(|p| &p[..]).collect();
        assert_eq!(name(&refs), "7.1");
        assert_eq!(name(&[&[at("L")], &[at("Lfh")]]), "3.1.2");
        assert_eq!(name(&[&[at("Lss")], &[at("Lfh")]]), "7.1.2");
        assert_eq!(name(&[&[at("C")], &[at("Lrh")]]), "7.1.4");
        // Between two front speakers is still the front.
        assert_eq!(name(&[&[[-0.4, 1.0, 0.0]]]), "5.1");
        // Front left to rear left: the side surround on the way.
        assert_eq!(name(&[&[at("L"), at("Lrs")]]), "7.1");
    }

    /// Rendered into the layout that holds it, a voice lands where the room
    /// puts it on 7.1.4: the same gains on the same speakers.
    #[test]
    fn a_held_voice_renders_as_it_would_on_seven_one_four() {
        let full = Layout::surround_7_1_4();
        let room = hz_render::Room::new(&full).expect("7.1.4");
        for (position, layout) in [
            ([0.0, 1.0, 0.0], hz_iamf::layout::MONO),
            ([-0.4, 1.0, 0.0], hz_iamf::layout::FIVE_ONE),
            ([1.0, -0.3, 0.0], hz_iamf::layout::SEVEN_ONE),
            ([-1.0, 1.0, 0.6], hz_iamf::layout::SEVEN_ONE_TWO),
        ] {
            let small = Layout {
                name: layout.name,
                speakers: layout.labels.iter().map(|label| canonical(label)).collect(),
            };
            let mut want = vec![0.0; full.channels()];
            room.gains(position, &mut want);
            let mut got = vec![0.0; small.channels()];
            hz_render::Room::new(&small)
                .expect("in the room")
                .gains(position, &mut got);
            for (label, gain) in full.speakers.iter().zip(&want) {
                let there = small.index_of(label).map_or(0.0, |c| got[c]);
                assert!(
                    (there - gain).abs() < 1e-12,
                    "{} {label}: {there} {gain}",
                    layout.name
                );
            }
        }
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
