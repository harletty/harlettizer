//! Encoding a programme as a TrueHD stream.
//!
//! Everything this needs already existed and nothing joined it up. [`hz_io`]
//! reads a master set and projects its delta stream of events into absolute
//! per-update state; [`hz_meta`] writes object audio metadata to the bit;
//! [`hz_mlp`] writes the immersive bitstream. This is the join: it walks the
//! projected updates alongside the audio and hands the encoder, for each
//! access unit that needs one, the payload describing where everything is.
//!
//! # What decides when a payload is written
//!
//! An access unit is forty samples. Object updates are hundreds of times
//! rarer than that — a shipped stream carries one payload per thirty or forty
//! units — so writing one every unit would spend bitrate restating a
//! programme that has not moved. A payload goes out when some element's state
//! changes, and otherwise every [`REFRESH`] units so that a decoder joining
//! mid-stream is never long without one.
//!
//! A **folding** encode is decided on the same clock. It used to run the
//! clusterer once per access unit and write a payload every time, which cost
//! +22 % on a master whose fold is the identity and handed a decoder a
//! metadata file a thousand times larger than the one that went in. It now
//! decides once per [`BLOCK_UNITS`] — one block of object metadata, the span
//! the reference's own payloads cover — and writes on the same two conditions
//! as everything else: the payload changed, or it is due. Which also makes the
//! energy that steers the clustering an average over a block instead of over
//! forty samples, and lets the weights cross-fade over the same span the
//! payload gives a decoder to ramp its positions across.
//!
//! # As many elements as the master has
//!
//! An object programme is any channel count above the 7.1 bed, and shipped
//! streams carry twelve, fourteen and sixteen. So a master's elements are
//! coded as they come, and the stream says how many there are; padding them
//! out to sixteen would spend framing on channels carrying nothing.
//!
//! Nine is the fewest the shape allows, since the fourth substream has to
//! carry something. A master with fewer elements than that is padded up to
//! nine with silent elements marked inactive, which is what the syntax has
//! the flag for.

use crate::source::{Source, keyframes_of, tracks};
use hz_cluster::mix::Limiter;
use hz_cluster::scene::{Loudness as Weighing, Scene, Source as SceneSource};
use hz_cluster::smooth::{SMOOTHING, Smoother, Smoothing};
use hz_cluster::{Clusterer, Clustering, Weighting};
use hz_core::speakers;
use hz_core::{Error, Result};
use hz_io::adm::model::TypeDefinition;
use hz_meta::oamd::{Block, Gain, Object, ObjectAudioMetadata, Position, Ramp, Render, Size};
use hz_mlp::hierarchy::Presentation;
use hz_mlp::{Config as StreamConfig, Encoder, SampleBits};
use hz_render::{Keyframe, Layout, ObjectFold};
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;

/// The Evolution payload identifier object audio metadata travels under.
const OAMD_PAYLOAD: u32 = 11;

/// The fewest elements an object programme can be written with.
///
/// Below this the fourth substream, which is what carries elements at all,
/// would have nothing in it.
const MIN_ELEMENTS: usize = 9;

/// How many access units one block of object metadata covers.
///
/// The reference streams write a payload every thirty to forty access units —
/// 940 over 36 106 units on one slice measured, 38.4 apart. Thirty-two is
/// inside that and divides the encoder's own restart interval exactly, so a
/// metadata block never straddles two intervals. At forty-eight kilohertz it
/// is 1280 samples, or 26.7 ms: long enough that the block's power is an
/// estimate rather than a sample of noise, short enough that a decoder's
/// position ramp still tracks a moving object.
const BLOCK_UNITS: usize = 32;

/// Samples in one access unit at the base rate of a family.
const BASE_UNIT_SAMPLES: f64 = 40.0;

/// How long one access unit lasts, in seconds, at a given sample rate.
///
/// Written as a duration because the ruler that wants it —
/// `hz_cluster::motion` — is a listener's and measures in time.
///
/// The format's frame is forty samples at the *base* rate of each family and
/// doubles with every doubling of it, so the duration is a property of the
/// family and not of the rate: a twelve-hundredth of a second across 48, 96
/// and 192 kHz, and a 1102.5th across 44.1, 88.2 and 176.4. This was a
/// constant twelve-hundredth, which is right for one family and seven per
/// cent out for the other — and every figure `motion` reports is a rate, so
/// it carried straight into them.
fn unit_seconds(sample_rate: u32) -> f64 {
    let base = if sample_rate % 44100 == 0 && sample_rate % 48000 != 0 {
        44100.0
    } else {
        48000.0
    };
    BASE_UNIT_SAMPLES / base
}

/// Restate the programme at least this often, in access units.
///
/// The reference streams measured for [`docs/fold.md`](../../../docs/fold.md)
/// refresh their dynamic range word every 128 units; this follows them rather
/// than picking a number.
const REFRESH: u64 = 128;

pub struct Config {
    pub input: PathBuf,
    pub out: PathBuf,
    /// Stop after this many frames of audio.
    pub frames: Option<u64>,
    /// Read a master set's waveforms from `<prefix>_<n>.wav` rather than
    /// from its interleaved audio: see [`hz_io::container::mono`].
    pub mono_prefix: Option<PathBuf>,
    /// Search less hard: see [`hz_mlp::Effort`].
    pub fast: bool,
    /// Fold the master's objects into this many elements rather than giving
    /// each one an element of its own.
    ///
    /// A delivery bitstream carries twelve, fourteen or sixteen, and a mix has
    /// as many objects as it needs. Without this the stream is one element per
    /// object, which is only possible because every master seen is already
    /// the output of somebody else's clustering.
    pub cluster: Option<usize>,
    /// Keep the master's elements as they are and pan its last this-many
    /// objects onto them — see [`hz_cluster::overlay`]. Mutually exclusive
    /// with [`Config::cluster`].
    pub overlay: Option<usize>,
    /// How far a bed-only fit may land from a source before the moving
    /// elements are let in — `hz_cluster::overlay::BEDS_FIRST`. `None`
    /// declines the preference and fits over every element. Only means
    /// anything with `--overlay`.
    pub overlay_beds: Option<f64>,
    /// What the caller actually typed for it, so that a negative reach can be
    /// refused rather than read as the preference declined.
    pub overlay_beds_asked: f64,
    /// What an overlay refuses to write — see [`Bounds`], which documents
    /// every one of these and why it is where it is. Nought turns one off.
    /// Only mean anything with `--overlay`.
    pub overlay_drift: f64,
    pub overlay_spread: f64,
    pub overlay_wobble: f64,
    pub overlay_level: f64,
    pub overlay_cost: f64,
    /// Whether a source the fit could not place acceptably is routed to the
    /// nearest bed element outright. Only means anything with `--overlay`.
    pub overlay_fallback: bool,
    /// Whether a source may take a spare element of its own when the master
    /// brings fewer than sixteen — see [`spare_slots`]. Off, the stream is
    /// exactly as wide as the master and every source is panned. Only means
    /// anything with `--overlay`.
    pub overlay_spare: bool,
    /// Write, per block, exactly what the overlay did with it — see
    /// [`Overlaid::report`]. For `cargo xtask overlay-check`, which cannot
    /// check the arithmetic without the weights that produced it.
    pub overlay_report: Option<PathBuf>,
    /// What dynamic range word the stream states — see [`Drc`].
    pub drc: Option<Drc>,
    /// How a bed channel other than the low frequency one is folded — see
    /// [`Beds`]. Only means anything with `--cluster`.
    pub beds: Beds,
    /// The programme's dialnorm in decibels, which sets the floor under what
    /// an object has to carry to be heard at the playback level — see
    /// `hz_cluster::floor`. Only means anything with `--cluster`.
    pub dialnorm: f64,
    /// How the fold keeps its elements inside the codec's domain — see
    /// [`Headroom`]. Only means anything with `--cluster`.
    pub headroom: Headroom,
    /// How a block's power is weighed before it steers the fold — see
    /// `hz_cluster::scene::Loudness`. The default is the constant every
    /// measurement in `docs/clustering.md` was made under; the perceptual
    /// rule is there to be listened to. Only means anything with `--cluster`.
    pub weighing: Weighing,
    /// Blocks behind the one being placed whose energies steady it — see
    /// `hz_cluster::smooth`. The blocks ahead are the constant's; only the
    /// reach behind is a knob, because it is the half of that window the
    /// synthesised scenes and a real contested fold disagree about. Only
    /// means anything with `--cluster`.
    pub smooth_behind: usize,
    /// How much better somewhere else has to be before an element goes there
    /// — the dead band of `hz_cluster::place::HOLDING`. Nought turns it off.
    /// Only means anything with `--cluster`.
    pub fold_hold: f64,
    /// Make the stream carry real folds rather than the leading elements: the
    /// channels become a hierarchy of presentations. See the command's help.
    pub presentations: bool,
    /// Report progress on standard error — see [`Progress`].
    pub progress: bool,
    /// What the folded elements are rounded to, in bits — see [`FOLD_BITS`]
    /// for why they are rounded at all, and [`FOLD_DEPTHS`] for the bounds.
    pub fold_depth: u32,
    /// Passes of the search that places each element on the fold's own cost —
    /// see [`hz_cluster::place`]. Zero declines it and takes the positions the
    /// clustering of directions settled on.
    ///
    /// A knob and not a flag because there is a whole curve here, and the two
    /// ends of it are not the only useful points: the error stops falling at
    /// four passes, but two already have most of it for half the time.
    pub fold_search: usize,
    /// What this machine's settings file says — the key Evolution frames'
    /// protection fields are signed with, when it holds one. See
    /// [`hz_mlp::protection`].
    pub settings: hz_io::settings::Settings,
}

/// How far along the encode is, on standard error, for a caller driving a bar.
///
/// # Why it is a flag and why it is stderr
///
/// The summary this command prints is its *answer* and goes to standard
/// output; progress is scaffolding that is meaningless once the thing has
/// finished, so it goes to standard error and only when asked for. A caller
/// reading both streams — which is what a process host does — can then tell
/// the two apart by their shape rather than by guessing.
///
/// # One line per whole percent
///
/// Not one per access unit: a two-hour programme is 216 000 of them and a bar
/// cannot show more than a hundred positions anyway. So a line goes out only
/// when the whole percent changes, which caps the output at a hundred lines
/// whatever the length, and the format is fixed and dull —
/// `progress <n>%` — because something is going to parse it with a regular
/// expression.
///
/// The denominator is the input's own frame count, which both containers state
/// in their header. So this is a real fraction of the work and not an estimate
/// from how much has been written, which would depend on how well the material
/// compresses.
struct Progress {
    total: u64,
    last: u64,
}

impl Progress {
    /// `None` when the flag is off, or when the input does not say how long it
    /// is — a percentage of an unknown total is a number made up.
    fn new(wanted: bool, total: u64) -> Option<Self> {
        (wanted && total > 0).then_some(Self {
            total,
            last: u64::MAX,
        })
    }

    /// The percent to report for `at` frames done, or `None` if the whole
    /// percent has not moved since the last one.
    ///
    /// Separated from printing it so that a test can watch the sequence: what
    /// matters about this is that it starts at nothing, ends at everything,
    /// never goes backwards and never repeats itself, and none of that is
    /// visible from the outside once it has gone to standard error.
    fn stepped(&mut self, at: u64) -> Option<u64> {
        let percent = at.min(self.total) * 100 / self.total;
        (percent != self.last).then(|| {
            self.last = percent;
            percent
        })
    }

    /// Report `at` frames of `total` done, if that has moved the whole percent.
    fn at(&mut self, at: u64) {
        if let Some(percent) = self.stepped(at) {
            eprintln!("progress {percent}%");
        }
    }
}

/// How a bed channel other than the low frequency one is carried by a fold.
///
/// A bed channel is a speaker feed: audio that belongs at one speaker and
/// nowhere else. Folded, it is an object that never moves from that
/// speaker's place in the cube — see `hz_render::fold::bed_position` — and
/// there are two things a fold can do with it. **Pin** it, so it takes an
/// element of its own at that place and is left out of the fit entirely,
/// which costs nothing while there is an element to spare and starves the
/// objects when there is not; or leave it **free**, an ordinary object of
/// the bed class — see `hz_cluster::class::BED` — that snaps to its speaker
/// and shares the elements with everything else by the metric's own rule.
///
/// The low frequency channel is neither: it has no place in the cube and it
/// keeps an element of its own whatever this says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Beds {
    /// Pinned while an element is left for every object as well, free
    /// otherwise: the elements have to be shared, and a bed that takes them
    /// all leaves the objects nothing.
    #[default]
    Auto,
    Pinned,
    Free,
}

/// How a fold keeps its elements inside the codec's domain.
///
/// An element is a sum, and two coherent objects in one element pass full
/// scale. The fit can **bound** an element's coherent peak to the domain by
/// solving the objects it carries under an upper bound on their weights —
/// see `hz_cluster::headroom` — and a **limiter** with one gain for every
/// element can hold whatever is left, ahead of the peak. The limiter alone
/// by default, because the metric says so: the bound is on the coherent
/// worst case, which forty tones in nine contributions an element pass in
/// every block while the mix itself passes full scale in few, and it costs
/// a fifth of the fold's mean for half the clips; the limiter takes every
/// clip for nothing the metric sees, and what it costs — a gain that dips
/// at the peaks — the report states. See `docs/clustering.md`. Both, the
/// bound alone, or neither, for the measurement; with neither, the mix is
/// clamped and the clips counted, which is what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Headroom {
    Both,
    Bound,
    #[default]
    Limit,
    Off,
}

impl Headroom {
    fn bounds(self) -> bool {
        matches!(self, Self::Both | Self::Bound)
    }

    fn limits(self) -> bool {
        matches!(self, Self::Both | Self::Limit)
    }
}

/// One element of the programme, and what the master says about it.
enum Part {
    /// The low frequency channel, which has no position of its own.
    Lfe { source_channel: usize },
    /// A dynamic object and the updates that place it — or a bed channel,
    /// which is an object that never moves from its speaker's place and may
    /// be pinned there. See [`Beds`].
    Object {
        source_channel: usize,
        keyframes: Vec<Keyframe>,
        /// The speaker's place in the cube, for a bed channel.
        bed: Option<[f64; 3]>,
    },
    /// Padding, so that the programme is sixteen elements wide.
    Absent,
}

/// What a block-at-a-time encode does with the objects it reads.
///
/// Both shapes read a whole block, decide a weight matrix, mix it and write
/// one payload, and everything in [`Fold`] serves both. What differs is where
/// the weights come from: a clustering *places* elements and folds every
/// object into them; an overlay takes the elements the master already has and
/// pans the last few objects onto them. See [`hz_cluster::overlay`].
enum Shape {
    Clustered(Box<Clustered>),
    Overlaid(Box<Overlaid>),
}

/// Folding the master's objects into fewer elements — `--cluster`.
struct Clustered {
    clusterer: Clusterer,
    /// Elements the objects are folded into, which is the stream's elements
    /// less the low frequency channel when there is one.
    clustered: usize,
    /// The previous block's clustering: what the weights ramp *from*, and what
    /// the next one warm-starts from.
    previous: Option<Clustering>,
    /// The scene as the *placement* sees it — this block's geometry carrying
    /// the energies of the window around it. See [`hz_cluster::smooth`].
    placing: Vec<hz_cluster::Object>,
    /// The energies of the blocks around the one being folded, and the rule
    /// that reads them.
    smoother: Smoother,
    /// Whether the bed channels take elements of their own — see [`Beds`] —
    /// and how many there are.
    pin_beds: bool,
    beds: usize,
    /// What of the elements' movement a listener would notice, taken on the
    /// positions as they are written and not as the fold holds them — see
    /// `hz_cluster::motion`. The harness has the same ruler, and this is the
    /// one that reports on the stream that ships.
    motion: hz_cluster::motion::Motion,
    written: Vec<[f64; 3]>,
    /// What each element carries, for the ruler above. Refilled rather than
    /// rebuilt: it was an allocation a block.
    carried: Vec<f64>,
    /// How many blocks the fit bounded an element's coherent peak in.
    bounded: u64,
}

/// What an overlay refuses to write, and what it only remarks on.
///
/// # Why an encoder refuses at all
///
/// Every other thing this encoder measures it *reports*: the fold's cost, the
/// wobble, the clipped samples. A caller reads the summary and decides. An
/// overlay is different in one way that matters — it is run by a pipeline, on
/// a programme nobody has listened to yet, and the failure it can produce is
/// **a line of dialogue in the wrong place or missing from a downmix**. That
/// is not a number a log should carry quietly past a caller who did not think
/// to grep for it. So the bounds below stop the encode: `error: …` on standard
/// error and a non-zero exit, which a process host already turns into a failed
/// step.
///
/// Each bound is a flag, and nought turns that one off — because a bound
/// chosen here is chosen without a programme to choose it on, and a caller who
/// has listened knows better than this does. See `docs/encode.md`.
///
/// # Voiced blocks
///
/// Every count below is over the blocks in which the source was **above the
/// audibility floor** — see [`hz_cluster::floor`]. A dub is silent most of the
/// time, and a guard that counted silent blocks would measure how much of the
/// programme nobody is speaking in.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    /// How far a source may be from where it asked to be, in degrees, before
    /// the block counts against it. Refused past [`OVERLAY_SHARE`] of the
    /// voiced blocks, or on any run longer than [`OVERLAY_RUN`].
    ///
    /// Thirty degrees is not a localisation bound — it is far past one. It is
    /// the point at which a voice is in a different part of the room from the
    /// picture, which is what a dub cannot ship with. Half of it is remarked
    /// on, which is about where the rear blur stops forgiving.
    drift: f64,
    /// The least the strongest element carrying a source may hold, as a
    /// fraction of the source's own level. Under it the source is diffuse:
    /// no one element is rendering it, so it arrives from everywhere the fit
    /// reached and moves whenever any of those elements does.
    ///
    /// A half is one element holding three quarters of the power. Refused past
    /// [`OVERLAY_SHARE`] of the voiced blocks.
    spread: f64,
    /// How much of the source's movement may be movement nobody asked for, as
    /// a percentage of the windows it was heard in — `hz_cluster::motion`'s
    /// `wobbling`, measured on where the carriers actually put the source.
    ///
    /// The sources of a dub are **static**, so every out-and-back in that
    /// measurement is the breathing the mode's own documentation warns about:
    /// a still voice carried by moving elements. This is the guard that
    /// catches it, and it is the only one that needs a time axis to see the
    /// defect at all.
    wobble: f64,
    /// How far a source's rendered level may be from what it asked for, in
    /// decibels, on any presentation the stream is played through.
    ///
    /// The guard the note that specified this mode called for in loudness
    /// terms, and it is computed from geometry rather than from BS.1770 over
    /// the audio: what a source comes out at on a presentation is a linear
    /// function of its weights and the elements' positions, which
    /// `hz_cluster::metric` already reports per presentation as `energy`. No
    /// filter, no block of audio, exact rather than estimated — and it
    /// catches the case the note was worried about, a voice landed on an
    /// element with a small stereo fold coefficient and vanishing from the
    /// downmix.
    level: f64,
    /// The worst a source's fold may cost, as a fraction of its own gain
    /// vector — `hz_cluster::metric`'s own measure, restricted to the sources.
    /// The elements are not in it: they are not folded.
    cost: f64,
    /// Whether a source the fit could not place acceptably is routed to the
    /// nearest bed element outright. See [`Overlaid::fell_back`].
    fallback: bool,
}

/// The bounds as they ship — see [`Bounds`], which says where each comes
/// from. One place, so the flag's default and the struct's cannot drift apart.
pub const OVERLAY_DRIFT: f64 = 30.0;
pub const OVERLAY_SPREAD: f64 = 0.5;
pub const OVERLAY_WOBBLE: f64 = 5.0;
pub const OVERLAY_LEVEL: f64 = 2.0;
pub const OVERLAY_COST: f64 = 0.5;

impl Default for Bounds {
    fn default() -> Self {
        Self {
            drift: OVERLAY_DRIFT,
            spread: OVERLAY_SPREAD,
            wobble: OVERLAY_WOBBLE,
            level: OVERLAY_LEVEL,
            cost: OVERLAY_COST,
            fallback: true,
        }
    }
}

/// What share of the voiced blocks a bound may be exceeded in before the
/// encode is refused.
///
/// A twentieth. Not nought, because a bound crossed in one block of a
/// programme is a block, and a dub that is refused for one block is a dub
/// nobody can ship; not more, because a twentieth of the voiced blocks is
/// already several seconds of dialogue in the wrong place over a feature.
const OVERLAY_SHARE: f64 = 0.05;

/// The longest unbroken run a bound may be exceeded for, in seconds, whatever
/// the share.
///
/// Two seconds is a sentence. A share says how much of a programme is wrong
/// and says nothing about whether it is wrong *all at once*, and those are
/// different failures: a twentieth scattered over a feature is a fault nobody
/// localises, and a twentieth in one place is a scene.
const OVERLAY_RUN: f64 = 2.0;

/// Keeping the master's elements and panning its last few objects onto them
/// — `--overlay`. See [`hz_cluster::overlay`] for what this is for.
struct Overlaid {
    overlay: hz_cluster::overlay::Overlay,
    /// How many trailing objects of the master are sources rather than
    /// elements.
    sources: usize,
    /// Elements the master brought, the low frequency channel included. The
    /// sources that took a spare slot are written after these.
    elements: usize,
    /// One source's weights over this block's carriers, and the elements'
    /// metadata for the payload. Both are refilled rather than rebuilt: a
    /// block is 27 ms, and an allocation a block is an allocation forty times
    /// a second for the length of a feature.
    row: Vec<f64>,
    /// Which elements this block mixed, for the limiter.
    live: Vec<bool>,
    /// The sources that were panned rather than given an element, and their
    /// rows, rebuilt each block — what the metric judges.
    panned: Vec<hz_cluster::Object>,
    /// Whether each source took a spare element, by index. The same fact as
    /// [`Overlaid::slots`], in the shape three hot loops a block want it in.
    slotted: Vec<bool>,
    /// Which source took which spare element of its own, in the order the
    /// slots are written. See [`Config::overlay`] and `docs/encode.md`.
    slots: Vec<usize>,
    /// The elements a source may be panned onto this block: active, not
    /// muted, and not the low frequency channel. Refilled every block.
    carriers: Vec<hz_cluster::overlay::Carrier>,
    /// Which elements are bed channels — see [`beds_among`], which works it
    /// out rather than trusting the master to declare it. Decided once, since
    /// it is a property of the whole programme and not of a block.
    beds: Vec<bool>,
    /// How many of them the master declared, so the summary can say which
    /// regime a run was in.
    declared_beds: usize,
    /// `weights[source][element]` as the fit gave them: what the held set is
    /// judged on and what the mix is built from.
    ///
    /// Two buffers that **swap** at the end of every block, so that a block
    /// costs no allocation. Between blocks `previous` therefore holds the
    /// latest answer and `weights` is the spare the next fit will fill —
    /// which is the right way round for the next block and the wrong way
    /// round for anything reading them afterwards.
    weights: Vec<Vec<f64>>,
    previous: Vec<Vec<f64>>,
    /// The sources as the fit sees them, rebuilt each block from the master's
    /// state and the block's own energies.
    fitting: Vec<hz_cluster::Object>,
    /// The mix matrix over every object of the master: an element carrying
    /// itself, a source spread over the carriers and divided by their gains.
    mix: Vec<Vec<f64>>,
    /// Where every element is this block, for the presentations.
    positions: Vec<[f64; 3]>,
    /// And the gain the payload states for each, in the same order: what a
    /// decoder rendering the elements applies, so what the presentations
    /// have to fold them at to sound like it.
    stated: Vec<f64>,
    /// Which elements a source reaches this block or reached last — the rest
    /// are copied through rather than mixed, and so are not rounded either.
    touched: Vec<bool>,
    /// Per element, the source channel it is copied from, or `None` when it is
    /// one of the few that this block actually mixes.
    copy_from: Vec<Option<usize>>,
    /// Element-blocks copied through untouched, and element-blocks mixed.
    copied: u64,
    remixed: u64,
    /// Blocks where an audible source had no carrier at all.
    stranded: u64,
    /// How far the carriers put the audible sources from where they asked to
    /// be, summed and at its worst — see `hz_cluster::overlay::drift`.
    drift: f64,
    drifted: u64,
    worst_drift: f64,
    /// What the encode refuses to write, and what it only remarks on.
    bounds: Bounds,
    /// Source-blocks in which each bound was exceeded, over the voiced ones.
    /// The denominator is [`Overlaid::drifted`]: a source is counted once a
    /// block, in the blocks it was audible in.
    over_drift: u64,
    /// And over half of it, which is what the remark is about — a remark that
    /// quotes the refusal's share beside the half bound contradicts itself.
    over_half_drift: u64,
    over_spread: u64,
    over_level: u64,
    over_cost: u64,
    /// The longest unbroken run of blocks a source was past the drift bound
    /// for, and the run each source is in now. In blocks; the summary turns
    /// them into seconds.
    run: Vec<u64>,
    worst_run: u64,
    /// The same for a source with nowhere at all to go, and the run it is in.
    stranded_run: Vec<u64>,
    worst_stranded_run: u64,
    /// How far a source's rendered level strayed on any presentation, at its
    /// worst, in decibels; and the worst a source's fold cost, with what each
    /// presentation cost in the block that was worst.
    worst_level: f64,
    worst_cost: f64,
    worst_cost_where: String,
    /// Where the carriers actually put each source, block after block, and
    /// what of that movement nobody asked for — `hz_cluster::motion`. The
    /// sources are static, so all of it is invented.
    motion: hz_cluster::motion::Motion,
    resultants: Vec<[f64; 3]>,
    energies: Vec<f64>,
    /// Where each source was last heard, held through the silence after it so
    /// that a phrase boundary is not read as movement. `None` before a source
    /// has ever been audible.
    resting: Vec<Option<[f64; 3]>>,
    /// Whether each source was above the floor this block and the last, which
    /// is what decides an element is copied rather than what its weights say.
    audible: Vec<bool>,
    was_audible: Vec<bool>,
    /// Blocks in which nothing at all was mixed — every element copied — which
    /// is the share of the *running time* the fast path took, as against the
    /// share of element-blocks it took.
    whole: u64,
    /// Blocks in which at least one source was above the audibility floor,
    /// which is what every share below is a share *of*. A dub is silent most
    /// of the time, and a guard counting silent blocks would measure how much
    /// of the programme nobody is speaking in.
    voiced: u64,
    /// Source-blocks routed to the nearest bed because the fit could not place
    /// them acceptably — see `--overlay-fallback`.
    fell_back: u64,
    /// The clustering the sources are judged as, rebuilt each block: the
    /// elements where the master put them, and the sources' own weights.
    judged: Clustering,
    /// Where the block-by-block account goes, when one was asked for.
    ///
    /// # Why an encoder writes down its own workings
    ///
    /// The invariants this mode rests on are statements about *arithmetic*:
    /// an element no source reached is the master's samples unchanged, and one
    /// a source reached is those samples plus the sources under the weights
    /// the fit chose, ramped across the block. A checker holding only the
    /// master and the decoded stream can verify the first and can only guess
    /// at the second — it would have to re-derive the fit, which is the very
    /// thing being checked.
    ///
    /// So the encoder says what it did: which elements it copied, which it
    /// mixed, and with what. `cargo xtask overlay-check` then recomputes the
    /// mix with `hz_cluster::mix::mix` — the same function, not a second
    /// implementation of it — and compares sample for sample.
    ///
    /// Off by default and free when off: a run that asks for no account
    /// touches none of this.
    report: Option<std::io::BufWriter<std::fs::File>>,
    /// Blocks and samples written so far, so the account can say where it is
    /// without the span loop having to tell it.
    reported_blocks: u64,
    reported_at: u64,
}

/// What a folding encode carries between blocks.
struct Fold {
    shape: Shape,
    /// Every presentation the stream will be played through, and what the fold
    /// has cost over them so far. An encoder that folds a scene and does not
    /// say what it cost is asking to be trusted.
    renderers: Vec<Box<dyn hz_render::Renderer>>,
    blocks: u64,
    mean: f64,
    worst: f64,
    /// Samples the mix pushed outside the codec's domain.
    clipped: u64,
    /// What a mixed sample is rounded to — see [`FOLD_BITS`].
    step: f64,
    /// The loudest sample the fold has produced so far, as a fraction of full
    /// scale. What decides whether a cascade can be carried: it stores more
    /// than it was given, and the programme's own peak says how much of the
    /// codec's domain is left to store it in.
    peak: f64,
    /// The last payload handed to the encoder, so that a block which says the
    /// same thing says nothing.
    last_payload: Vec<u8>,
    /// Everything a block needs, kept across blocks.
    ///
    /// These were ten allocations an access unit in the hot path. They are now
    /// reused buffers, refilled once a metadata block.
    ///
    /// The scene is held rather than built because the energy it computes is
    /// K-weighted and a filter has state — and because it is the *same* scene
    /// `cargo xtask cluster` builds, which is the only way the harness's
    /// report is a report about this stream. See [`hz_cluster::scene`].
    scene: Scene,
    signals: Vec<Vec<f32>>,
    gains: Vec<f64>,
    mixed: Vec<Vec<f32>>,
    from: Vec<Vec<f64>>,
    /// What an object has to carry to be heard — see `hz_cluster::floor`.
    floor: hz_cluster::floor::Floor,
    /// Objects under the room's floor, summed over the blocks.
    inaudible: u64,
    /// The limiter on what the bound leaves, when there is one — see
    /// [`Headroom`].
    limiter: Option<Limiter>,
}

impl Fold {
    /// Blocks of energy the placement wants to see beyond the one it is
    /// folding. An overlay wants none: it places nothing, so nothing about it
    /// depends on what a later block does.
    fn look_ahead(&self) -> usize {
        match &self.shape {
            Shape::Clustered(clustered) => clustered.smoother.smoothing().ahead,
            Shape::Overlaid(_) => 0,
        }
    }

    /// Whether a bed channel takes an element of its own — see [`Beds`].
    fn pin_beds(&self) -> bool {
        match &self.shape {
            Shape::Clustered(clustered) => clustered.pin_beds,
            Shape::Overlaid(_) => false,
        }
    }

    /// What the span just read does, for the window the placement looks over.
    fn record(&mut self, energies: &[f64]) {
        if let Shape::Clustered(clustered) = &mut self.shape {
            clustered.smoother.record(energies);
        }
    }

    /// Where the elements the stream carries are, the low frequency channel
    /// left out because it has no position — which is the order
    /// [`presentations_of`] reads them in.
    fn positions(&self) -> Option<&[[f64; 3]]> {
        match &self.shape {
            Shape::Clustered(clustered) => clustered.previous.as_ref().map(|at| &at.positions[..]),
            Shape::Overlaid(overlaid) => Some(&overlaid.positions[..]),
        }
    }

    /// The gain the payload states for each of those elements, in the same
    /// order, where it states any but unity. A cluster is a mix the fold made
    /// at the level it renders at, so it has none; an overlay's elements are
    /// the master's, at the master's gains.
    fn stated(&self) -> Option<&[f64]> {
        match &self.shape {
            Shape::Clustered(_) => None,
            Shape::Overlaid(overlaid) => Some(&overlaid.stated[..]),
        }
    }
}

/// A span read but not yet folded, waiting to see what the next ones do.
///
/// As many of these are held as the fold looks ahead — see
/// [`hz_cluster::smooth::SMOOTHING`] — and that is the whole of the
/// look-ahead: a span's own samples, how many of them are real, and where
/// every object was at its end.
struct Waiting {
    block: Vec<i32>,
    frames: usize,
    /// Which part each live entry is, and its state at the end of each access
    /// unit of the span — one row per unit, the last of which is the span's.
    ///
    /// A *fold* wants only that last row: it decides one answer for the whole
    /// block and gives a decoder the block to reach it. An **overlay** wants
    /// them all, because the elements' metadata is the master's and the
    /// master states it per unit. Reading it once a block put every keyframe
    /// on a block boundary — one at sample 48 000 landed at 47 360, thirteen
    /// milliseconds early — and skipped any that shared a block with another.
    units: Vec<Vec<(usize, Keyframe)>>,
    /// The sample the span starts at, from which each unit's own start — and
    /// so where inside a unit the master moved something — is counted.
    at: u64,
}

impl Waiting {
    /// The state at the end of the span, which is what a fold is placed on.
    fn states(&self) -> &[(usize, Keyframe)] {
        self.units.last().map_or(&[], |states| &states[..])
    }
}

/// What each object does over one span, in the unit the clustering weighs by.
///
/// Built by the same `hz_cluster::scene` the fold itself uses, as each span
/// is read and before it is folded, so that the number the placement looks
/// ahead at is the number the fold steers by — a second way of computing it
/// would be the very defect that module exists to close. Only the energy: the
/// geometry a span is folded with is its own.
///
/// Its own `Scene`, because the filter a weighted scene carries has state and
/// the two are reading different spans. Both see a contiguous stream, one an
/// span ahead of the other.
#[allow(clippy::too_many_arguments)]
fn energies_of(
    scene: &mut Scene,
    block: &[i32],
    source_channels: usize,
    frames: usize,
    live: &[Live],
    parts: &[Part],
    pin_beds: bool,
    out: &mut Vec<f64>,
) {
    scene.start();
    for entry in live {
        let Part::Object {
            source_channel,
            bed,
            ..
        } = &parts[entry.part_index]
        else {
            continue;
        };
        scene.push(
            &SceneSource {
                position: entry.state.position,
                gain: entry.state.gain,
                size: entry.state.spread,
                importance: entry.state.importance,
                pinned: bed.filter(|_| pin_beds),
                mode: entry.state.mode,
            },
            (0..frames).map(|sample| {
                f64::from(block[sample * source_channels + source_channel]) / FULL_SCALE
            }),
        );
    }
    out.clear();
    out.extend(scene.finish().iter().map(|object| object.energy));
}

/// The shell a block's sources are judged in: only the positions and the
/// weights are read by `hz_cluster::metric`, and both are refilled every
/// block. Held rather than built so that judging a block costs no allocation.
fn blank_clustering() -> Clustering {
    Clustering {
        positions: Vec::new(),
        directions: Vec::new(),
        weights: Vec::new(),
        owners: Vec::new(),
        modes: Vec::new(),
        bounded: 0,
    }
}

/// Everything a block-at-a-time encode carries that is the same whichever
/// shape it is, so that the two constructions state only what differs.
fn blank_fold(config: &Config, sample_rate: f64, width: usize) -> Result<Fold> {
    Ok(Fold {
        // Replaced by the caller; a shape is the one thing this cannot guess.
        shape: Shape::Clustered(Box::new(Clustered {
            clusterer: Clusterer::new(hz_cluster::MIN_ELEMENTS, Weighting::Fitted)
                .map_err(|why| Error::unsupported(&config.input, why.to_string()))?,
            clustered: 0,
            previous: None,
            placing: Vec::new(),
            smoother: Smoother::new(SMOOTHING),
            pin_beds: false,
            beds: 0,
            motion: hz_cluster::motion::Motion::new(
                hz_cluster::motion::HEARING,
                BLOCK_UNITS as f64 * unit_seconds(sample_rate as u32),
            ),
            written: Vec::new(),
            carried: Vec::new(),
            bounded: 0,
        })),
        renderers: hz_cluster::metric::delivery()
            .map_err(|why| Error::unsupported(&config.input, why.to_string()))?,
        blocks: 0,
        mean: 0.0,
        worst: 0.0,
        clipped: 0,
        step: fold_step(config.fold_depth),
        peak: 0.0,
        last_payload: Vec::new(),
        scene: Scene::weighed(sample_rate, config.weighing)
            .map_err(|why| Error::unsupported(&config.input, why.to_string()))?,
        signals: Vec::new(),
        gains: Vec::new(),
        mixed: vec![Vec::new(); width],
        from: Vec::new(),
        floor: hz_cluster::floor::Floor::for_dialnorm(config.dialnorm),
        inaudible: 0,
        limiter: config.headroom.limits().then(Limiter::new),
    })
}

/// Which of the master's elements are bed channels, whether or not the master
/// says so.
///
/// # Why this cannot be read off the declaration
///
/// `parts_of` marks an element a bed when the master declares it one — an ADM
/// `DirectSpeakers` track, or a `bedInstances` entry. **A dub's master
/// declares no such thing.** A programme decoded back out of a delivery
/// stream carries its 7.1 bed as ordinary objects that never move from their
/// speakers' places, and declares only the low frequency channel as a bed;
/// that is what a decoder can reconstruct, and it is what arrives here.
///
/// So on the one input this mode exists for, every bed-related mechanism was
/// inert: the beds-first preference needs one bed and one non-bed to have
/// anything to choose between, `nearest_bed` returned nothing, the fallback
/// was a no-op, and a source with every carrier muted had nowhere to be
/// rescued to. Every test that exercised a bed did so on a synthetic master
/// that declares one, so none of them saw it.
///
/// # What counts as a bed
///
/// An element that **never moves** and sits **exactly at a speaker's place**,
/// both judged on the wire's own grid — the positions a payload can code, a
/// sixty-second of the room across and a fifteenth up. Two positions that
/// code to the same triple are the same position as far as any decoder is
/// concerned, so that is the right resolution to ask the question at, and it
/// needs no tolerance invented here.
///
/// A static object somewhere that is *not* a speaker's place is not a bed: it
/// is an object that happens to be still, and pinning a source to it would be
/// choosing a place the mix never called a speaker.
fn beds_among(parts: &[Part], elements: usize) -> Vec<bool> {
    let places: Vec<hz_meta::oamd::Position> = hz_core::speakers::SPEAKERS
        .iter()
        .filter(|speaker| !speaker.is_lfe())
        .filter_map(|speaker| hz_render::fold::bed_position(speaker.master))
        .map(hz_meta::oamd::Position::from_master)
        .collect();

    (0..elements)
        .map(|element| match parts.get(element) {
            Some(Part::Object { keyframes, bed, .. }) => {
                if bed.is_some() {
                    return true;
                }
                let Some(first) = keyframes.first() else {
                    return false;
                };
                let at = hz_meta::oamd::Position::from_master(first.position);
                keyframes
                    .iter()
                    .all(|frame| hz_meta::oamd::Position::from_master(frame.position) == at)
                    && places.contains(&at)
            }
            _ => false,
        })
        .collect()
}

/// Which sources take a spare element of their own, in the order those
/// elements are written.
///
/// A stream carries sixteen elements and a master being re-voiced usually
/// brings sixteen, so this is normally empty and everything is panned. When
/// the master brings fewer, a source can have an element to itself instead —
/// which is exactly what a plain encode would have written for it — and then
/// it is not panned at all, costs nothing, and is carried bit for bit.
///
/// # Only when asked for
///
/// A spare element makes the stream wider than the master: a twelve-element
/// original re-voiced with four sources would ship as sixteen. By default a
/// re-voiced programme keeps the original's width — `allowed` is false, this
/// is empty however much room there was, and every source is panned onto
/// the elements the master brought. `--overlay-spare` is what lets the
/// sources take the room.
///
/// # Which sources get them
///
/// The ones nearest the front centre, which is where a dub's dialogue is and
/// so where an error is least forgiven. Stated as an angle rather than as a
/// channel name on purpose: the sources are objects at speaker places and
/// what makes one the centre is where it is, not what a master called it.
fn spare_slots(parts: &[Part], elements: usize, sources: usize, allowed: bool) -> Vec<usize> {
    let spare = hz_mlp::format::MAX_CHANNELS.saturating_sub(elements);
    if !allowed || spare == 0 || sources == 0 {
        return Vec::new();
    }
    let ahead = |index: usize| -> f64 {
        let Some(Part::Object { keyframes, .. }) = parts.get(elements + index) else {
            return f64::MIN;
        };
        let at = keyframes.first().copied().unwrap_or_default().position;
        let length = (at[0] * at[0] + at[1] * at[1] + at[2] * at[2]).sqrt();
        if length < 1e-12 { -1.0 } else { at[1] / length }
    };
    let mut order: Vec<usize> = (0..sources).collect();
    // Nearest the front first, and a stable tie broken by the master's own
    // order so that the same input always writes the same stream.
    order.sort_by(|a, b| {
        ahead(*b)
            .partial_cmp(&ahead(*a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });
    order.truncate(spare.min(sources));
    order
}

/// Where one element is now, and which update said so.
struct Live {
    part_index: usize,
    /// Next keyframe to act on.
    next: usize,
    state: Keyframe,
}

pub fn run(config: Config) -> Result<()> {
    let mut source = Source::open(&config.input, config.mono_prefix.as_deref())?;
    if config.cluster.is_some() && config.overlay.is_some() {
        return Err(Error::unsupported(
            &config.input,
            "`--cluster` and `--overlay` at once; one folds the master's objects into fewer              elements and the other keeps them, so a stream is one or the other".to_string(),
        ));
    }
    // Both shapes read a whole block, carry a bed other than the LFE, and take
    // a master wider than a plain encode would: a clustering because it folds
    // it down, an overlay because the sources ride after the elements.
    let folding = config.cluster.is_some() || config.overlay.is_some();
    let parts = parts_of(&config.input, &source, folding)?;
    let source_channels = source.channels();
    let lfe_channel = parts.iter().find_map(|part| match part {
        Part::Lfe { source_channel } => Some(*source_channel),
        _ => None,
    });
    // Folding: the low frequency channel is a bed and is not clustered, so it
    // takes the first element and the objects share what is left.
    let beds = parts
        .iter()
        .filter(|part| matches!(part, Part::Object { bed: Some(_), .. }))
        .count();
    let mut folded = match config.cluster {
        Some(wanted) => {
            let clustered = wanted - usize::from(lfe_channel.is_some());
            let moving = parts
                .iter()
                .filter(|part| matches!(part, Part::Object { bed: None, .. }))
                .count();
            // See [`Beds`]: pinned while every object can still have an
            // element of its own beside the bed, free otherwise.
            let pin_beds = match config.beds {
                Beds::Pinned => true,
                Beds::Free => false,
                Beds::Auto => beds + moving <= clustered,
            };
            if pin_beds && beds >= clustered {
                return Err(Error::unsupported(
                    &config.input,
                    format!(
                        "{beds} bed channels pinned into {clustered} elements leaves nothing \
                         for the objects; fold with more elements or `--beds free`"
                    ),
                ));
            }
            if !(hz_cluster::MIN_ELEMENTS..=hz_cluster::MAX_ELEMENTS).contains(&clustered) {
                return Err(Error::unsupported(
                    &config.input,
                    format!("{wanted} elements to fold into"),
                ));
            }
            if !FOLD_DEPTHS.contains(&config.fold_depth) {
                return Err(Error::unsupported(
                    &config.input,
                    format!(
                        "a fold depth of {} bits; {} to {} — below that the format's own \
                         dead-bit field cannot say what was rounded away, and above it there \
                         is nothing finer to keep",
                        config.fold_depth,
                        FOLD_DEPTHS.start(),
                        FOLD_DEPTHS.end()
                    ),
                ));
            }
            // A band is a fraction of what staying costs, so at one it asks
            // moving to be better than free and every element is frozen
            // wherever the first block put it — silently, since a frozen fold
            // still writes a stream and still reports an error the metric is
            // content with.
            if !(0.0..1.0).contains(&config.fold_hold) {
                return Err(Error::unsupported(
                    &config.input,
                    format!(
                        "a dead band of {}; it is a fraction of what staying costs, so nought \
                         to just under one — at one nothing can ever pay to move and every \
                         element freezes where the first block put it",
                        config.fold_hold
                    ),
                ));
            }
            if !FOLD_SEARCHES.contains(&config.fold_search) {
                return Err(Error::unsupported(
                    &config.input,
                    format!(
                        "{} passes of the placement search; {} to {} — beyond that it looks \
                         finer than the wire codes a position to and moves nothing",
                        config.fold_search,
                        FOLD_SEARCHES.start(),
                        FOLD_SEARCHES.end()
                    ),
                ));
            }
            Some(Fold {
                shape: Shape::Clustered(Box::new(Clustered {
                    clusterer: Clusterer::new(clustered, Weighting::Fitted)
                        .map_err(|why| Error::unsupported(&config.input, why.to_string()))?
                        .searching(hz_cluster::place::Search {
                            passes: config.fold_search,
                            holding: config.fold_hold,
                            ..hz_cluster::place::Search::default()
                        })
                        .flooring(hz_cluster::floor::Floor::for_dialnorm(config.dialnorm))
                        .bounding(config.headroom.bounds()),
                    clustered,
                    previous: None,
                    placing: Vec::new(),
                    smoother: Smoother::new(Smoothing {
                        behind: config.smooth_behind,
                        ..SMOOTHING
                    }),
                    pin_beds,
                    beds,
                    motion: hz_cluster::motion::Motion::new(
                        hz_cluster::motion::HEARING,
                        BLOCK_UNITS as f64 * unit_seconds(source.sample_rate()),
                    ),
                    written: Vec::new(),
                    carried: Vec::new(),
                    bounded: 0,
                })),
                ..blank_fold(&config, f64::from(source.sample_rate()), clustered)?
            })
        }
        None => match config.overlay {
            // Keeping the master's elements and panning its last few objects
            // onto them. The stream is as wide as the master's elements —
            // plus, when `--overlay-spare` asks for it, whatever spare slots
            // the sources could take.
            Some(sources) => {
                if sources == 0 {
                    return Err(Error::unsupported(
                        &config.input,
                        "`--overlay 0`; an overlay with no sources is a plain encode of the \
                         master, which is what leaving the flag off already does"
                            .to_string(),
                    ));
                }
                let element_parts = parts.len().checked_sub(sources).filter(|kept| *kept > 0);
                let Some(element_parts) = element_parts else {
                    return Err(Error::unsupported(
                        &config.input,
                        format!(
                            "{sources} overlay sources out of {} objects; the sources are the \
                             master's *last* objects and the ones before them are the elements \
                             they are panned onto, so there has to be at least one of those",
                            parts.len()
                        ),
                    ));
                };
                if !(hz_cluster::MIN_ELEMENTS..=hz_mlp::format::MAX_CHANNELS)
                    .contains(&element_parts)
                {
                    return Err(Error::unsupported(
                        &config.input,
                        format!(
                            "{element_parts} elements to overlay onto; {} to {}",
                            hz_cluster::MIN_ELEMENTS,
                            hz_mlp::format::MAX_CHANNELS
                        ),
                    ));
                }
                if !FOLD_DEPTHS.contains(&config.fold_depth) {
                    return Err(Error::unsupported(
                        &config.input,
                        format!(
                            "a fold depth of {} bits; {} to {}",
                            config.fold_depth,
                            FOLD_DEPTHS.start(),
                            FOLD_DEPTHS.end()
                        ),
                    ));
                }
                // Only the asked-for value can be negative: the command line
                // turns anything not above nought into `None`, so testing the
                // option as well was testing a thing that cannot happen.
                if config.overlay_beds_asked < 0.0 {
                    return Err(Error::unsupported(
                        &config.input,
                        format!(
                            "a bed reach of {}; it is a distance, so nought declines the \
                             preference and anything below that is a typed minus sign",
                            config.overlay_beds_asked
                        ),
                    ));
                }
                let bounds = Bounds {
                    drift: config.overlay_drift,
                    spread: config.overlay_spread,
                    wobble: config.overlay_wobble,
                    level: config.overlay_level,
                    cost: config.overlay_cost,
                    fallback: config.overlay_fallback,
                };
                let slots = spare_slots(&parts, element_parts, sources, config.overlay_spare);
                let width = element_parts + slots.len();
                Some(Fold {
                    shape: Shape::Overlaid(Box::new(Overlaid {
                        overlay: hz_cluster::overlay::Overlay::new()
                            .map_err(|why| Error::unsupported(&config.input, why.to_string()))?
                            .preferring(config.overlay_beds),
                        sources,
                        elements: element_parts,
                        row: Vec::new(),
                        live: Vec::new(),
                        slotted: {
                            let mut flags = vec![false; sources];
                            for index in &slots {
                                flags[*index] = true;
                            }
                            flags
                        },
                        panned: Vec::new(),
                        slots,
                        beds: beds_among(&parts, element_parts),
                        declared_beds: parts[..element_parts]
                            .iter()
                            .filter(|part| matches!(part, Part::Object { bed: Some(_), .. }))
                            .count(),
                        carriers: Vec::new(),
                        weights: Vec::new(),
                        previous: vec![vec![0.0; width]; sources],
                        fitting: Vec::new(),
                        mix: Vec::new(),
                        positions: Vec::new(),
                        stated: Vec::new(),
                        touched: Vec::new(),
                        copy_from: Vec::new(),
                        copied: 0,
                        remixed: 0,
                        stranded: 0,
                        drift: 0.0,
                        drifted: 0,
                        worst_drift: 0.0,
                        bounds,
                        over_drift: 0,
                        over_half_drift: 0,
                        over_spread: 0,
                        over_level: 0,
                        over_cost: 0,
                        run: vec![0; sources],
                        worst_run: 0,
                        stranded_run: Vec::new(),
                        worst_stranded_run: 0,
                        worst_level: 0.0,
                        worst_cost: 0.0,
                        worst_cost_where: String::new(),
                        motion: hz_cluster::motion::Motion::new(
                            hz_cluster::motion::HEARING,
                            BLOCK_UNITS as f64 * unit_seconds(source.sample_rate()),
                        ),
                        resultants: Vec::new(),
                        energies: Vec::new(),
                        resting: Vec::new(),
                        audible: Vec::new(),
                        was_audible: Vec::new(),
                        whole: 0,
                        voiced: 0,
                        fell_back: 0,
                        judged: blank_clustering(),
                        report: match &config.overlay_report {
                            Some(path) => Some(std::io::BufWriter::new(
                                std::fs::File::create(path).map_err(|e| Error::io(path, e))?,
                            )),
                            None => None,
                        },
                        reported_blocks: 0,
                        reported_at: 0,
                    })),
                    ..blank_fold(&config, f64::from(source.sample_rate()), width)?
                })
            }
            None => None,
        },
    };
    let elements = match &folded {
        Some(fold) => match &fold.shape {
            Shape::Clustered(clustered) => clustered.clustered + usize::from(lfe_channel.is_some()),
            Shape::Overlaid(overlaid) => overlaid.elements + overlaid.slots.len(),
        },
        None => parts.len(),
    };

    let mut encoder = Encoder::new(StreamConfig {
        sample_rate: source.sample_rate(),
        channels: elements,
        bits: SampleBits::TwentyFour,
    })
    .map_err(|why| Error::unsupported(&config.input, why.0))?;
    if config.fast {
        encoder.set_effort(hz_mlp::Effort::Fast);
    }
    if let Some(key) = &config.settings.evolution_key {
        encoder.set_evolution_key(key);
    }

    // The dynamic range word, on every substream a decoder may stop after.
    //
    // Without it the stream states no schedule at all, which is what a decoder
    // asked for compressed output finds: nothing to apply. The reference
    // streams state one on all four of their substreams and restate it every
    // 128 units — 282 words over 36 106 units on one slice measured — so
    // that is the cadence, and it is the same one this encoder already
    // refreshes its object metadata on.
    //
    // **What gain to ask for is not decided here.** A/52 specifies the wire
    // format of a gain and the intent behind it and stops; the named
    // characteristics are the reference encoder's own curves, and matching
    // them means measuring them. So the default states unity — "no reduction
    // asked for" — which is a stated profile rather than an absent one, and
    // `--drc` states something else when a caller knows better. See
    // `docs/drc.md`.
    // Stated per unit from the measured curve, unless a caller asked for a
    // constant — see [`Loudness`].
    let mut loudness = match config.drc {
        Some(Drc::Measured) => {
            let widths: Vec<usize> = encoder
                .presentations()
                .iter()
                .copied()
                .filter(|width| *width > 0)
                .collect();
            Some(Loudness::new(
                &widths,
                f64::from(source.sample_rate()) / encoder.frame_size() as f64,
            ))
        }
        Some(Drc::Constant(db)) => {
            let gain = hz_mlp::format::DynamicRange::from_db(db, DRC_REFRESH).ok_or_else(|| {
                Error::unsupported(
                    &config.input,
                    format!("a dynamic range of {db} dB; the field spans about ±24"),
                )
            })?;
            for substream in 0..hz_mlp::format::MAX_SUBSTREAMS {
                encoder.set_dynamic_range(substream, Some(gain));
            }
            None
        }
        None => None,
    };
    let frame = encoder.frame_size();

    // Seeded with each object's first update rather than with a default, so
    // that the payload written before any update has arrived already puts
    // everything where the master will first say it is. Starting at the centre
    // and jumping on the first update would be a move the master never asked
    // for.
    let mut live: Vec<Live> = parts
        .iter()
        .enumerate()
        .map(|(part_index, part)| Live {
            part_index,
            next: 0,
            state: match part {
                Part::Object { keyframes, .. } => keyframes.first().copied().unwrap_or_default(),
                _ => Keyframe::default(),
            },
        })
        .collect();

    // A folding encode reads a whole block of object metadata at a time,
    // because the clustering that block declares is decided on all of it; a
    // plain one reads an access unit, which is all it ever needed.
    let span_units = if folded.is_some() { BLOCK_UNITS } else { 1 };
    let span = frame * span_units;
    let mut block = vec![0i32; span * source_channels];
    let mut unit = vec![0i32; frame * elements];
    // Written as the units come out rather than held whole: a two-hour
    // programme is a couple of gigabytes and there is no reason for any of it
    // to be in memory at once.
    let file = std::fs::File::create(&config.out).map_err(|e| Error::io(&config.out, e))?;
    let mut stream = std::io::BufWriter::new(file);
    let mut at = 0u64;
    let mut units = 0u64;
    let mut since_payload = REFRESH;
    // Whether the channels are a hierarchy of presentations rather than the
    // elements. Read once, because the folding thread borrows it.
    let carries_folds = config.presentations;
    let mut payloads = 0u64;
    let limit = config.frames.unwrap_or(u64::MAX);
    // What the encode is a fraction *of*: the input's own length, capped by
    // `--frames` when that is what the run will actually do.
    let mut progress = Progress::new(config.progress, source.frames().min(limit));
    if let Some(progress) = &mut progress {
        progress.at(0);
    }

    // Without a fold the presentations follow the master's own elements, on
    // the cadence a folding encode restates them at: once the elements have
    // moved, and no more often than once a block. The encoder takes whichever
    // it was last given at its next restart, so folds stated every unit would
    // be thrown away all but once an interval.
    let mut folds_due = carries_folds;
    let mut fold_positions = Vec::with_capacity(parts.len());
    let mut fold_stated = Vec::with_capacity(parts.len());

    match folded.as_mut() {
        // Folding, the span is read, mixed and turned into a payload on a
        // thread of its own, one span ahead of the writer.
        //
        // The two halves want different things — the fold is a mix and a
        // clustering over a block, the encoder is a filter search over an
        // interval — and neither waits on the other for anything but the
        // samples. One after the other they alternate, and the fold is a ninth
        // of the wall clock; side by side it hides behind the encoder, which
        // has the machine to spare.
        //
        // Two buffers, handed back to be refilled, so this is two spans of
        // memory for the whole encode rather than one an iteration. The
        // producer owns everything it touches: the source, the fold and its
        // running cost, the live objects, and the count that says when a
        // payload is due — which is why nothing here decides that any more.
        Some(fold) => {
            let (spans, from_fold) = std::sync::mpsc::sync_channel::<Result<Produced>>(1);
            let (free, spare) = std::sync::mpsc::channel::<Vec<i32>>();
            for _ in 0..2 {
                let _ = free.send(vec![0i32; span * elements]);
            }
            let input = &config.input;
            let sample_rate = f64::from(source.sample_rate());
            let parts = &parts;
            let source = &mut source;
            let live = &mut live;
            let pin_beds = fold.pin_beds();
            // The scene one span ahead, built by the same thing the fold's
            // own is: a second `Scene` because a weighted one carries filter
            // state and these two are reading different spans, each of them
            // contiguous.
            let mut ahead_scene = Scene::weighed(sample_rate, config.weighing)
                .map_err(|why| Error::unsupported(input, why.to_string()))?;

            std::thread::scope(|scope| -> Result<()> {
                scope.spawn(move || {
                    let mut block = vec![0i32; span * source_channels];
                    let mut since_payload = REFRESH;
                    let mut at = 0u64;
                    let fail = |why| {
                        let _ = spans.send(Err(why));
                    };
                    // Some spans of latency, in the *energy* only — as many
                    // as the fold looks ahead. A span is read, what its
                    // objects do in it is measured and recorded, and the span
                    // that many behind it is then folded knowing what follows
                    // — so an element that has to move because a sound
                    // appears starts moving before it arrives rather than
                    // reaching it as the block ends. The geometry is still
                    // the folded span's own, so nothing that merely moves
                    // leads the audio. See `hz_cluster::smooth`.
                    let look_ahead = fold.look_ahead();
                    // Whether the elements have to be read once an access unit
                    // rather than once a span. Only an overlay does.
                    let per_unit_wanted = matches!(fold.shape, Shape::Overlaid(_));
                    let mut waiting: VecDeque<Waiting> = VecDeque::with_capacity(look_ahead + 1);
                    // Where every element stood at the end of each unit of the
                    // span just read, handed to the queue and swapped for an
                    // empty one rather than built afresh.
                    let mut per_unit: Vec<Vec<(usize, Keyframe)>> = Vec::new();
                    // And the elements a payload states, reused across units,
                    // beside what the last one said — so that a unit in which
                    // nothing moved is dismissed by comparing states rather
                    // than by serialising a payload to discover it.
                    let mut stated = ObjectAudioMetadata {
                        lfe: false,
                        blocks: Vec::new(),
                    };
                    let mut said: Vec<Keyframe> = Vec::new();
                    let mut ahead: Vec<f64> = Vec::new();

                    // Record what the span just read does, if one was, and
                    // fold and send the span that has waited long enough, if
                    // there is one. Returns false if the writer has gone away.
                    let mut give =
                        |read: Option<&[f64]>,
                         waiting: Option<&Waiting>,
                         since_payload: &mut u64|
                         -> std::result::Result<bool, hz_core::Error> {
                            if let Some(read) = read {
                                fold.record(read);
                            }
                            let Some(waiting) = waiting else {
                                return Ok(true);
                            };
                            let Ok(mut samples) = spare.recv() else {
                                return Ok(false);
                            };
                            samples.clear();
                            samples.resize(span * elements, 0);
                            let payload = fold_span(
                                fold,
                                input,
                                waiting.states(),
                                parts,
                                &waiting.block,
                                source_channels,
                                waiting.frames,
                                lfe_channel,
                                elements,
                                &mut samples,
                            )?;

                            // The same two conditions as a plain encode: it
                            // changed, or it is due.
                            let units = waiting.frames.div_ceil(frame);
                            let mut payloads: Vec<Option<Stated>> = vec![None; units];
                            let mut any = false;
                            match &fold.shape {
                                // A fold decides one answer for the whole
                                // block, and a payload can only ride the unit
                                // its ramp starts on. Due is measured against
                                // the *next* chance to say it.
                                Shape::Clustered(_) => {
                                    if payload != fold.last_payload
                                        || *since_payload + span_units as u64 > REFRESH
                                    {
                                        fold.last_payload.clear();
                                        fold.last_payload.extend_from_slice(&payload);
                                        payloads[0] = Some(Stated {
                                            bytes: payload,
                                            sample_offset: 0,
                                        });
                                        any = true;
                                        // What the writer's own count would
                                        // have reached: nothing on the unit
                                        // that carries it, then one for each
                                        // of the rest.
                                        *since_payload = units as u64 - 1;
                                    } else {
                                        *since_payload += units as u64;
                                    }
                                }
                                // An overlay's elements are the master's, and
                                // the master states them per unit. So each
                                // unit is offered its own, on exactly the two
                                // conditions a plain encode uses — and where
                                // nothing moved, nothing is written, which is
                                // what keeps a static bed from restating
                                // itself forty times a second.
                                Shape::Overlaid(overlaid) => {
                                    let kept = overlaid.elements + overlaid.slots.len();
                                    for (unit, states) in waiting.units.iter().enumerate() {
                                        // Nothing moved, and nothing is due:
                                        // no payload, and no *serialisation*
                                        // to find that out. Comparing the
                                        // states asks the same question the
                                        // plain path asks with `moved`, and it
                                        // is the whole of the work in the
                                        // units where a programme is still —
                                        // which, on a dub, is nearly all of
                                        // them.
                                        if held(&said, states, kept) && *since_payload < REFRESH {
                                            *since_payload += 1;
                                            continue;
                                        }
                                        remember(&mut said, states, kept);
                                        let one = overlay_payload(
                                            overlaid,
                                            input,
                                            parts,
                                            states,
                                            &mut stated,
                                        )?;
                                        if one != fold.last_payload || *since_payload >= REFRESH {
                                            fold.last_payload.clear();
                                            fold.last_payload.extend_from_slice(&one);
                                            payloads[unit] = Some(Stated {
                                                bytes: one,
                                                sample_offset: sample_offset_of(
                                                    states
                                                        .iter()
                                                        .map(|(part, state)| (*part, state)),
                                                    parts,
                                                    waiting.at + (unit * frame) as u64,
                                                    frame,
                                                ),
                                            });
                                            any = true;
                                            *since_payload = 0;
                                        } else {
                                            *since_payload += 1;
                                        }
                                    }
                                }
                            }
                            let due = any.then_some(());

                            // On the same cadence as the payload, and for the
                            // same reason: a payload goes out when the
                            // elements have moved, and a fold of elements that
                            // have moved is a different fold. Between them the
                            // stream keeps the arrangement it has.
                            let mut presentations = None;
                            if due.is_some()
                                && carries_folds
                                && let Some(positions) = fold.positions()
                            {
                                presentations = presentations_of(
                                    positions,
                                    fold.stated(),
                                    lfe_channel.is_some(),
                                );
                            }

                            Ok(spans
                                .send(Ok(Produced {
                                    frames: waiting.frames,
                                    samples,
                                    payloads,
                                    presentations,
                                }))
                                .is_ok())
                        };

                    loop {
                        let done = at >= limit || {
                            let frames = match source.read(&mut block) {
                                Ok(frames) => frames,
                                Err(why) => return fail(why),
                            };
                            if frames == 0 {
                                true
                            } else {
                                // Everything past the real samples is silence,
                                // so a folded element's mix and a copied
                                // channel both come out right.
                                block[frames * source_channels..].fill(0);
                                // The objects are advanced to the end of each
                                // access unit in turn, and where they stood at
                                // each is kept. The *fold* is placed on the
                                // last of those — the payload it writes states
                                // where the elements finish up and gives a
                                // decoder the span to ramp there across, which
                                // is what the mixer does with the weights —
                                // and an overlay, whose element metadata is
                                // the master's rather than its own, writes one
                                // from each.
                                //
                                // A fold wants only the last, so a fold walks
                                // no units and keeps one row: the per-unit
                                // work is the overlay's and the clustering
                                // path does not pay for it.
                                //
                                // The rows are the previous span's, handed
                                // back when its buffers were, so a span of
                                // this costs no allocation once the shapes
                                // have settled.
                                let units = if per_unit_wanted {
                                    frames.div_ceil(frame)
                                } else {
                                    1
                                };
                                if per_unit.len() < units {
                                    per_unit.resize_with(units, Vec::new);
                                }
                                per_unit.truncate(units);
                                for (unit, row) in per_unit.iter_mut().enumerate() {
                                    let ends = if per_unit_wanted {
                                        at + ((unit + 1) * frame).min(frames) as u64 - 1
                                    } else {
                                        at + frames as u64 - 1
                                    };
                                    advance(live, parts, ends);
                                    row.clear();
                                    row.extend(
                                        live.iter().map(|entry| (entry.part_index, entry.state)),
                                    );
                                }
                                at += frames as u64;
                                energies_of(
                                    &mut ahead_scene,
                                    &block,
                                    source_channels,
                                    frames,
                                    live,
                                    parts,
                                    pin_beds,
                                    &mut ahead,
                                );
                                let states = std::mem::take(&mut per_unit);
                                // `per_unit` is refilled from the span that
                                // leaves the queue below, so this is a swap
                                // and not a loss.
                                // The span just read joins the queue; the one
                                // that has waited its turn goes out, placed
                                // knowing what comes next. Their buffers trade
                                // places, so this is one more span of source
                                // than the look-ahead for the whole encode
                                // rather than one an iteration.
                                let kept = std::mem::take(&mut block);
                                let ready = if waiting.len() >= look_ahead {
                                    waiting.pop_front()
                                } else {
                                    None
                                };
                                match give(Some(&ahead), ready.as_ref(), &mut since_payload) {
                                    Ok(true) => {}
                                    Ok(false) => return,
                                    Err(why) => return fail(why),
                                }
                                match ready {
                                    Some(ready) => {
                                        block = ready.block;
                                        per_unit = ready.units;
                                    }
                                    // Nothing sent yet: the queue is still
                                    // filling.
                                    None => block = vec![0i32; span * source_channels],
                                }
                                waiting.push_back(Waiting {
                                    block: kept,
                                    frames,
                                    units: states,
                                    at: at - frames as u64,
                                });
                                false
                            }
                        };
                        if done {
                            // The last spans have less after them to look at,
                            // and are placed for what there is.
                            while let Some(ready) = waiting.pop_front() {
                                match give(None, Some(&ready), &mut since_payload) {
                                    Ok(true) => {}
                                    Ok(false) => return,
                                    Err(why) => return fail(why),
                                }
                            }
                            return;
                        }
                    }
                });

                let mut write = || -> Result<()> {
                    'spans: while let Ok(produced) = from_fold.recv() {
                        let produced = produced?;
                        if let Some(presentations) = &produced.presentations {
                            encoder.set_presentations(presentations);
                            if let Some(loudness) = &mut loudness {
                                loudness.fold(presentations);
                            }
                        }
                        let mut due = produced.payloads.into_iter();
                        let mut done = 0usize;
                        while done < produced.frames {
                            if at >= limit {
                                break 'spans;
                            }
                            let unit_frames = frame.min(produced.frames - done);
                            let tail = unit_frames < frame;
                            unit.fill(0);
                            unit[..unit_frames * elements].copy_from_slice(
                                &produced.samples[done * elements..(done + unit_frames) * elements],
                            );
                            // Each unit takes its own, where it has one. A
                            // fold puts one on the first unit of its block,
                            // which is where the ramp it declares starts; an
                            // overlay puts one wherever the master moved
                            // something, which is the cadence a plain encode
                            // writes at.
                            if let Some(stated) = due.next().flatten() {
                                encoder.set_evolution_at(
                                    OAMD_PAYLOAD,
                                    &stated.bytes,
                                    stated.sample_offset,
                                );
                                payloads += 1;
                            }
                            if let Some(loudness) = &mut loudness {
                                loudness.state(&mut encoder, &unit, elements, unit_frames);
                            }
                            emit(
                                &mut encoder,
                                &mut stream,
                                &config.out,
                                &unit,
                                elements,
                                unit_frames,
                                tail,
                            )?;
                            done += unit_frames;
                            at += unit_frames as u64;
                            units += 1;
                            if let Some(progress) = &mut progress {
                                progress.at(at);
                            }
                            if tail {
                                break 'spans;
                            }
                        }
                        let _ = free.send(produced.samples);
                    }
                    Ok(())
                };
                let written = write();
                // Whichever end stopped first, the other is waiting on a
                // channel: dropping both is what lets the producer finish
                // rather than block on a buffer that will never come back.
                drop(free);
                drop(from_fold);
                written
            })?;
        }

        // No fold: a unit at a time, straight from the source, exactly as
        // before.
        None => 'units: loop {
            if at >= limit {
                break;
            }
            let frames = source.read(&mut block)?;
            if frames == 0 {
                break;
            }
            block[frames * source_channels..].fill(0);
            let tail = frames < frame;
            unit.fill(0);

            // To the end of the unit, so that a keyframe inside it is stated
            // in it — at its own sample, below — and not a unit late.
            let moved = advance(&mut live, &parts, at + frames as u64 - 1);
            folds_due |= carries_folds && moved;
            if folds_due && units.is_multiple_of(BLOCK_UNITS as u64) {
                if let Some(presentations) =
                    plain_presentations(&live, &parts, &mut fold_positions, &mut fold_stated)
                {
                    encoder.set_presentations(&presentations);
                    if let Some(loudness) = &mut loudness {
                        loudness.fold(&presentations);
                    }
                }
                folds_due = false;
            }
            // Spread the master's channels across the elements the stream
            // carries, leaving the padding silent.
            for (index, part) in parts.iter().enumerate() {
                let Some(source_channel) = part.source_channel() else {
                    continue;
                };
                for sample in 0..frame {
                    unit[sample * elements + index] =
                        block[sample * source_channels + source_channel];
                }
            }
            if moved || since_payload >= REFRESH {
                let payload = programme(&live, &parts)
                    .write()
                    .map_err(|why| Error::unsupported(&config.input, format!("metadata: {why}")))?;
                let sample_offset = sample_offset_of(
                    live.iter().map(|entry| (entry.part_index, &entry.state)),
                    &parts,
                    at,
                    frame,
                );
                encoder.set_evolution_at(OAMD_PAYLOAD, &payload, sample_offset);
                since_payload = 0;
                payloads += 1;
            } else {
                since_payload += 1;
            }

            if let Some(loudness) = &mut loudness {
                loudness.state(&mut encoder, &unit, elements, frames);
            }
            emit(
                &mut encoder,
                &mut stream,
                &config.out,
                &unit,
                elements,
                frames,
                tail,
            )?;
            at += frames as u64;
            units += 1;
            if let Some(progress) = &mut progress {
                progress.at(at);
            }
            if tail {
                break 'units;
            }
        },
    }

    stream
        .write_all(&encoder.finish(&[]))
        .map_err(|e| Error::io(&config.out, e))?;
    stream.flush().map_err(|e| Error::io(&config.out, e))?;

    let objects = parts
        .iter()
        .filter(|p| matches!(p, Part::Object { .. }))
        .count();
    println!("{}", config.input.display());
    let padding = parts.iter().filter(|p| matches!(p, Part::Absent)).count();
    println!(
        "  programme    {objects} objects{}, {elements} elements{}",
        if parts.iter().any(|p| matches!(p, Part::Lfe { .. })) {
            " and an LFE"
        } else {
            ""
        },
        if padding > 0 {
            format!(", {padding} of them padding")
        } else {
            String::new()
        }
    );
    println!("  encoded      {units} access units, {at} samples");
    println!(
        "  metadata     {payloads} payloads, one every {:.0} units",
        units as f64 / payloads.max(1) as f64
    );
    match (&config.settings.evolution_key, &config.settings.from) {
        (Some(_), Some(from)) => println!(
            "  protection   each payload's frame signed with the key from {}",
            from.display()
        ),
        (Some(_), None) => println!("  protection   each payload's frame signed"),
        (None, _) => println!(
            "  protection   no key configured, so a constant stands in the field; \
             see docs/encode.md"
        ),
    }
    if let Some(fold) = &folded
        && fold.blocks > 0
    {
        if let Shape::Clustered(_) = &fold.shape {
            println!(
                "  fold         {:.4} mean over the presentations, {:.3} at its worst, \
                 as a fraction of the object's own gains",
                fold.mean / fold.blocks as f64,
                fold.worst
            );
        }
        println!(
            "  weighed by   {}",
            match config.weighing {
                Weighing::Flat => "the plain power, what a meter reads".to_string(),
                Weighing::KWeighted => "the K-weighted power of BS.1770".to_string(),
                Weighing::Perceptual(perception) => format!(
                    "the perceptual importance: what each object adds to the loudness of the \
                     scene over {} ERB-rate bands, {} masker",
                    perception.bands,
                    match perception.masker {
                        hz_cluster::scene::Masker::Global => "a global",
                        hz_cluster::scene::Masker::Local => "a local",
                    }
                ),
            }
        );
        if let Shape::Clustered(clustered) = &fold.shape {
            if clustered.motion.windows() > 0 {
                println!(
                    "  noticed      {:.2} % of the element-windows wobble past the blur, {:.2} % \
                     of what is playing sitting in one; {:.2}°/s of movement the mix never asked \
                     for, against {:.2}°/s it did — see hz_cluster::motion",
                    100.0 * clustered.motion.wobbling(),
                    100.0 * clustered.motion.wobbling_energy(),
                    clustered.motion.invented(),
                    clustered.motion.asked(),
                );
            }
            println!(
                "  held by      {}",
                if config.fold_hold > 0.0 {
                    format!(
                        "a dead band: an element stays where it was unless moving pays {:.0} % of \
                         what staying costs",
                        100.0 * config.fold_hold
                    )
                } else {
                    "nothing: an element moves for any gain the metric sees".to_string()
                }
            );
            let smoothing = clustered.smoother.smoothing();
            println!(
                "  steadied by  a hold over {} block{} behind and {} ahead",
                smoothing.behind,
                if smoothing.behind == 1 { "" } else { "s" },
                smoothing.ahead
            );
        }
        match &fold.shape {
            // `inaudible` is the clustering's count over the whole scene, and
            // an overlay never fills it — it judges sources, not objects — so
            // the line read "0.00 objects a block under it" whatever the
            // programme did. What the floor decides here is which blocks a
            // source's guards are counted over, and that is what it says.
            Shape::Clustered(_) => println!(
                "  floor        {:.1} dBFS at the playback level a dialnorm of {:.0} implies; \
                 {:.2} objects a block under it",
                fold.floor.threshold_dbfs(),
                config.dialnorm,
                fold.inaudible as f64 / fold.blocks as f64
            ),
            Shape::Overlaid(overlaid) => println!(
                "  floor        {:.1} dBFS at the playback level a dialnorm of {:.0} implies; a \
                 source was above it in {:.1} % of the blocks, and is guarded in those",
                fold.floor.threshold_dbfs(),
                config.dialnorm,
                100.0 * overlaid.voiced as f64 / fold.blocks.max(1) as f64
            ),
        }
        if let Shape::Clustered(clustered) = &fold.shape
            && clustered.beds > 0
        {
            println!(
                "  beds         {} channels besides the LFE, {}",
                clustered.beds,
                if clustered.pin_beds {
                    "each pinned to its speaker with an element of its own"
                } else {
                    "free objects of the bed class, sharing the elements"
                }
            );
        }
        if let Shape::Overlaid(overlaid) = &fold.shape {
            overlay_summary(overlaid, fold.blocks, &config);
        }
        println!(
            "  depth        {} rounded to {} bits{}",
            match &fold.shape {
                Shape::Clustered(_) => "elements",
                Shape::Overlaid(_) => "the elements a source reached",
            },
            config.fold_depth,
            if config.fold_depth == 24 {
                ", which is every bit the mix made"
            } else {
                ""
            }
        );

        println!(
            "  headroom     {} blocks where an element's coherent peak was bounded in the fit, {}",
            match &fold.shape {
                Shape::Clustered(clustered) => clustered.bounded,
                Shape::Overlaid(_) => 0,
            },
            match &fold.limiter {
                Some(limiter) => format!(
                    "{} blocks where the limiter brought every element down, the mix peaking at \
                     {:.2} of full scale before it",
                    limiter.limited, limiter.peak
                ),
                None => "no limiter".to_string(),
            }
        );
        if fold.clipped > 0 {
            println!(
                "  clipped      {} samples of the mix went outside the codec's domain \
                 and were clamped",
                fold.clipped
            );
        }
    }
    match config.drc {
        Some(Drc::Measured) => println!(
            "  drc          from the measured curve, per presentation, restated every {} units",
            1u32 << DRC_REFRESH
        ),
        Some(Drc::Constant(db)) => println!(
            "  drc          {db:+.2} dB stated on every substream, restated every {} units",
            1u32 << DRC_REFRESH
        ),
        None => println!("  drc          none stated"),
    }
    if config.presentations {
        presentations_summary(&encoder.stats());
    }
    println!("  wrote        {}", config.out.display());
    // And what the guards make of it. Last, because it is a verdict on
    // everything above it, and because a caller reading the tail of a log
    // should find the answer there.
    if let Some(fold) = &folded
        && let Shape::Overlaid(overlaid) = &fold.shape
    {
        let remarks = overlay_remarks(
            overlaid,
            fold,
            BLOCK_UNITS as f64 * unit_seconds(source.sample_rate()),
        );
        for remark in remarks.iter().filter(|remark| !remark.refused) {
            println!("  note         {}", remark.said);
        }
        let refusals: Vec<&Remark> = remarks.iter().filter(|remark| remark.refused).collect();
        if !refusals.is_empty() {
            // `error:` on standard error, because that is the line a process
            // host greps for and the shape every other refusal here takes.
            // The stream is left where it was written: a refusal is a
            // statement about what is in the file, and the fastest way to
            // check a bound nobody has listened to yet is to listen to what
            // it stopped.
            for refusal in &refusals {
                eprintln!("error: overlay: {}", refusal.said);
            }
            return Err(Error::refused(
                &config.out,
                format!(
                    "the overlay is outside {} of its bounds; the stream was written and left \
                     in place so that it can be heard, and every bound is a flag that turns it \
                     off — see docs/encode.md",
                    refusals.len()
                ),
            ));
        }
    }
    Ok(())
}

impl Part {
    fn source_channel(&self) -> Option<usize> {
        match self {
            Self::Lfe { source_channel } | Self::Object { source_channel, .. } => {
                Some(*source_channel)
            }
            Self::Absent => None,
        }
    }
}

/// What the master set's tracks are, in the order the stream will carry them.
///
/// The low frequency channel goes first because the payload's own flag says it
/// does: when a programme declares one, it is the first element and everything
/// after it is a dynamic object.
fn parts_of(path: &std::path::Path, source: &Source, folding: bool) -> Result<Vec<Part>> {
    let description = source.adm();
    let mut lfe = None;
    let mut objects = Vec::new();

    for track in tracks(path, description, source.channels())? {
        let number = track.number;
        let source_channel = track.source_channel;
        match track.format.type_definition {
            TypeDefinition::DirectSpeakers => {
                let label = track.speaker_label();
                if speakers::is_lfe_label(label) {
                    lfe = Some(Part::Lfe { source_channel });
                } else if folding {
                    // A bed channel is an object that never leaves its
                    // speaker's place, and the fold decides what to do with
                    // it — see [`Beds`]. A label with no place in the cube is
                    // refused rather than guessed at.
                    let Some(position) = hz_render::fold::bed_position(label) else {
                        return Err(Error::unsupported(
                            path,
                            format!(
                                "track {number} is the bed channel `{label}`, which has no \
                                 known place in the cube to fold at"
                            ),
                        ));
                    };
                    objects.push(Part::Object {
                        source_channel,
                        keyframes: vec![Keyframe {
                            position,
                            mode: hz_cluster::class::BED,
                            ..Keyframe::default()
                        }],
                        bed: Some(position),
                    });
                } else {
                    return Err(Error::unsupported(
                        path,
                        format!(
                            "track {number} is the bed channel `{label}`; a bed other than the \
                             LFE is carried only by a folding encode, `--cluster`"
                        ),
                    ));
                }
            }
            TypeDefinition::Objects => objects.push(Part::Object {
                source_channel,
                keyframes: keyframes_of(track.format, description.sample_rate),
                bed: None,
            }),
            other => eprintln!("note: track {number} is `{other}` audio; left out"),
        }
    }

    let mut parts = Vec::with_capacity(MIN_ELEMENTS);
    parts.extend(lfe);
    parts.extend(objects);
    // A master with more objects than a stream has elements is exactly what
    // `--cluster` is for, so the cap only applies when nothing is folding
    // them.
    if !folding && parts.len() > hz_mlp::format::MAX_CHANNELS {
        return Err(Error::unsupported(
            path,
            format!(
                "{} elements; an object programme carries at most {}, so this master needs \
                 `--cluster`",
                parts.len(),
                hz_mlp::format::MAX_CHANNELS
            ),
        ));
    }
    // Below nine the fourth substream, which is what carries elements, would
    // have nothing in it. A folding encode says how many elements it wants and
    // does not need padding to reach them.
    while !folding && parts.len() < MIN_ELEMENTS {
        parts.push(Part::Absent);
    }
    Ok(parts)
}

/// Move every element to the state the master gives it at `at`.
///
/// Returns whether anything changed, which is what decides that a payload is
/// worth writing.
fn advance(live: &mut [Live], parts: &[Part], at: u64) -> bool {
    let mut moved = false;
    for entry in live.iter_mut() {
        let Part::Object { keyframes, .. } = &parts[entry.part_index] else {
            continue;
        };
        while entry.next < keyframes.len() && keyframes[entry.next].sample_pos <= at {
            entry.state = keyframes[entry.next];
            entry.next += 1;
            moved = true;
        }
    }
    moved
}

/// One payload, and the sample of its unit it takes effect at.
#[derive(Clone)]
struct Stated {
    bytes: Vec<u8>,
    /// Where inside the unit the master moved what this states — see
    /// [`sample_offset_of`]. Nought for a fold, whose positions are its own
    /// and decided once a block.
    sample_offset: u32,
}

/// The sample of the unit starting at `unit_start` that a payload stated from
/// these states takes effect at: where inside the unit the master moved what
/// the payload restates.
///
/// One payload states one moment, so the elements that moved inside the unit
/// have to agree on it. On every master seen they do — a reference stream
/// moves everything it moves in a unit at one sample, five, thirteen,
/// twenty-one or twenty-nine into it — and where they do not, the moment most
/// of them moved at is taken, the earliest on a tie, so that what is stated
/// off its sample is the fewest elements by the least. A unit nothing moved
/// in restates a scene already in force, at its start. Only an element with
/// keyframes of its own can have moved; a default state at sample nought is
/// not a move.
fn sample_offset_of<'a>(
    states: impl IntoIterator<Item = (usize, &'a Keyframe)>,
    parts: &[Part],
    unit_start: u64,
    frame: usize,
) -> u32 {
    // The unit at the highest rate the format carries is 160 samples.
    let mut moved = [0u16; 160];
    for (part, state) in states {
        let Some(Part::Object { keyframes, .. }) = parts.get(part) else {
            continue;
        };
        if keyframes.is_empty() || state.sample_pos < unit_start {
            continue;
        }
        let offset = (state.sample_pos - unit_start) as usize;
        if offset < frame.min(moved.len()) {
            moved[offset] += 1;
        }
    }
    let mut chosen = 0usize;
    let mut most = 0u16;
    for (offset, count) in moved.iter().enumerate().take(frame) {
        if *count > most {
            most = *count;
            chosen = offset;
        }
    }
    chosen as u32
}

/// One span of audio, mixed and ready, and what its block of metadata says.
///
/// What crosses from the thread that folds to the thread that writes.
struct Produced {
    frames: usize,
    samples: Vec<i32>,
    /// What each access unit of this span writes, or nothing where it says
    /// what the last one said. Decided by the producer, which is what owns
    /// the count.
    ///
    /// A fold fills only the first: its answer is one for the whole block and
    /// a payload can only ride the unit its ramp starts on. An overlay fills
    /// as many as the master moved something in, because the elements'
    /// metadata is the master's.
    payloads: Vec<Option<Stated>>,
    /// What each presentation of the stream is, over the elements, when this
    /// block moved them enough to be worth restating.
    ///
    /// The folds and not the matrices: the matrices depend on the dead bits,
    /// which only the encoder knows. Computed here because this is where the
    /// elements' positions are — the writer sees samples and has no idea where
    /// they came from.
    presentations: Option<Vec<Presentation>>,
}

/// What each presentation states, and the detector that decides it.
///
/// One detector per presentation, held across the whole stream: the level a
/// measured curve answers to is a leaky integral over about 700 ms, which is
/// twenty-six access units, so it cannot be recomputed per unit from nothing.
///
/// # What level, and which curve
///
/// **The level is the one a decoder stopping at that presentation plays.**
/// Where the stream carries folds, that is the fold — every element through
/// the presentation's rows — and not the leading elements, which is what the
/// narrow presentations were before there were folds to carry. Reading the
/// leading elements then measured two quiet objects for a stereo that sums
/// twelve, and the word boosted where the reference cuts: on the programme
/// slice a constant +1.13 dB, 2.67 dB above what the stereo curve asks for at
/// the level a decoder actually hands back.
///
/// **The curve is the one shipped streams state for that presentation.**
/// [`hz_analysis::drc::STEREO`] for a two-channel fold: much steeper than the
/// wide one, because the fewer the channels the more the sum needs holding
/// back. [`hz_analysis::drc::WIDE`] for everything wider, which is also what
/// shipped streams state for their six-channel presentation, and for a
/// two-channel presentation that is not a fold — a copy of two elements has
/// had no summation to hold back.
///
/// # When the word is stated, and why that is early
///
/// The encoder holds a restart interval before writing it and states the word
/// as it stands when the interval is complete, so a word answers to the level
/// at the end of its interval: up to 107 ms ahead of the unit it is stated
/// on. That is what shipped streams do too — their words match the level 80 to
/// 120 ms ahead best, measured with `cargo xtask drc --against` on two
/// reference streams — so it is kept.
struct Loudness {
    detectors: Vec<hz_analysis::drc::Level>,
    /// How many channels each presentation carries, cumulative.
    widths: Vec<usize>,
    /// The narrow presentations' rows over the elements, the latest the
    /// stream was handed, where it carries folds; empty where it does not.
    /// Refilled in place, so that restating the folds allocates nothing once
    /// the rows exist.
    folds: Vec<Vec<Vec<f64>>>,
    /// One frame of the fold being measured, reused.
    folded: Vec<f64>,
}

impl Loudness {
    fn new(widths: &[usize], rate: f64) -> Self {
        Self {
            detectors: widths
                .iter()
                .map(|_| hz_analysis::drc::Level::new(rate))
                .collect(),
            widths: widths.to_vec(),
            folds: Vec::new(),
            folded: Vec::new(),
        }
    }

    /// The presentations the stream was just handed. The narrow ones are what
    /// their levels are measured through from now on; the widest is the
    /// elements, which are measured as they are.
    fn fold(&mut self, presentations: &[Presentation]) {
        let narrow = presentations.len().saturating_sub(1);
        self.folds.resize_with(narrow, Vec::new);
        for (into, presentation) in self.folds.iter_mut().zip(presentations) {
            into.clone_from(&presentation.rows);
        }
    }

    /// Feed one access unit and state what each presentation now asks for.
    fn state(&mut self, encoder: &mut Encoder, unit: &[i32], elements: usize, frames: usize) {
        let mut gains = [None; hz_mlp::format::MAX_SUBSTREAMS];
        self.measure(unit, elements, frames, &mut gains);
        for (substream, gain) in gains.iter().enumerate() {
            if let Some(word) =
                gain.and_then(|gain| hz_mlp::format::DynamicRange::from_db(gain, DRC_REFRESH))
            {
                encoder.set_dynamic_range(substream, Some(word));
            }
        }
    }

    /// Feed one access unit, and say in decibels what each presentation now
    /// asks for.
    ///
    /// The power is summed over the presentation's channels and averaged over
    /// the unit, which is how the curve was measured and so how it has to be
    /// fed.
    fn measure(
        &mut self,
        unit: &[i32],
        elements: usize,
        frames: usize,
        gains: &mut [Option<f64>; hz_mlp::format::MAX_SUBSTREAMS],
    ) {
        if frames == 0 {
            return;
        }
        for (substream, width) in self.widths.iter().enumerate() {
            let width = (*width).min(elements);
            if width == 0 {
                continue;
            }
            let mut power = 0.0f64;
            let curve = match self.folds.get(substream) {
                Some(rows) if !rows.is_empty() => {
                    self.folded.resize(rows.len(), 0.0);
                    for frame in unit.chunks_exact(elements).take(frames) {
                        self.folded.fill(0.0);
                        for (out, row) in self.folded.iter_mut().zip(rows) {
                            for (gain, sample) in row.iter().zip(frame) {
                                *out += gain * f64::from(*sample);
                            }
                        }
                        for value in &self.folded {
                            let value = value / FULL_SCALE;
                            power += value * value;
                        }
                    }
                    if rows.len() == 2 {
                        hz_analysis::drc::STEREO
                    } else {
                        hz_analysis::drc::WIDE
                    }
                }
                _ => {
                    for frame in 0..frames {
                        for channel in 0..width {
                            let value = f64::from(unit[frame * elements + channel]) / FULL_SCALE;
                            power += value * value;
                        }
                    }
                    hz_analysis::drc::WIDE
                }
            };
            let level = self.detectors[substream].feed(power / frames as f64);
            gains[substream] = Some(curve.gain_db(level));
        }
    }
}

/// Hand one access unit to the encoder and write what comes back.
///
/// The last unit of a stream is short and is *kept*: the format pads it and
/// marks how many of its samples to throw away, which is what `finish` writes.
fn emit(
    encoder: &mut Encoder,
    stream: &mut impl Write,
    out: &std::path::Path,
    unit: &[i32],
    elements: usize,
    frames: usize,
    tail: bool,
) -> Result<()> {
    let bytes = if tail {
        encoder.finish(&unit[..frames * elements])
    } else {
        encoder.push(unit)
    };
    stream.write_all(&bytes).map_err(|e| Error::io(out, e))
}

/// Fold one block of object metadata into the elements, and say where they
/// went.
///
/// The scene is the live objects at the *end* of the block: where the master
/// has put each one, how loud it is over the whole block, and how wide. The
/// clustering decides which elements carry it; [`hz_cluster::mix`] makes their
/// audio, ramping the weights across the block from what the previous one
/// ended at so that a weight changing is a crossfade and not a step — over the
/// same span the payload gives a decoder to ramp its positions across.
///
/// # A block, not an access unit
///
/// This ran once per access unit. Forty samples is a poor estimate of a
/// signal's power, and a payload every forty samples cost +22 % of stream on a
/// master whose fold is the identity. A block is [`BLOCK_UNITS`] of them,
/// which is what the reference's own payloads span.
///
/// The low frequency channel is not clustered. It is a bed: it goes to the
/// first element untouched and the payload says so.
///
/// Returns the payload this block *would* write. Whether it is written is the
/// caller's: an unchanged one is not.
#[allow(clippy::too_many_arguments)]
fn fold_span(
    fold: &mut Fold,
    path: &std::path::Path,
    states: &[(usize, Keyframe)],
    parts: &[Part],
    block: &[i32],
    source_channels: usize,
    frames: usize,
    lfe_channel: Option<usize>,
    elements: usize,
    out: &mut [i32],
) -> Result<Vec<u8>> {
    // The scene, and each object's signal for this block. The buffers are the
    // fold's own and are refilled, not reallocated.
    let pin_beds = matches!(&fold.shape, Shape::Clustered(clustered) if clustered.pin_beds);
    fold.scene.start();
    fold.gains.clear();
    fold.signals.resize_with(states.len(), Vec::new);
    let mut object = 0usize;
    for (part_index, state) in states {
        let Part::Object {
            source_channel,
            bed,
            ..
        } = &parts[*part_index]
        else {
            continue;
        };
        let signal = &mut fold.signals[object];
        signal.clear();
        signal.reserve(frames);
        for sample in 0..frames {
            let value = f64::from(block[sample * source_channels + source_channel]) / FULL_SCALE;
            signal.push(value as f32);
        }
        // The energy is not computed here: it is what the clustering is
        // steered by, and two places computing it is two answers. Averaged
        // over a block rather than over forty samples, which is the difference
        // between an estimate and a sample of noise, and weighted the way a
        // listener weighs it — see `hz_cluster::scene`.
        fold.scene.push(
            &SceneSource {
                position: state.position,
                gain: state.gain,
                size: state.spread,
                importance: state.importance,
                pinned: bed.filter(|_| pin_beds),
                mode: state.mode,
            },
            signal.iter().map(|sample| f64::from(*sample)),
        );
        fold.gains.push(state.gain);
        object += 1;
    }
    fold.signals.truncate(object);
    // Weighed against each other, now that every object is in — which is a
    // step only the perceptual rule has anything to do in.
    fold.scene.finish();

    // Which shape this is decides where the weights come from and what the
    // payload says; everything either side of that is the same work.
    match &fold.shape {
        Shape::Clustered(_) => cluster_span(
            fold,
            path,
            block,
            source_channels,
            frames,
            lfe_channel,
            elements,
            out,
        ),
        Shape::Overlaid(_) => overlay_span(
            fold,
            path,
            states,
            parts,
            block,
            source_channels,
            frames,
            lfe_channel,
            elements,
            out,
        ),
    }
}

/// One mixed sample into the codec's domain.
///
/// An element is a *sum*, and a sum of objects that happen to agree is louder
/// than any of them. Clamped rather than wrapped: the codec's domain is
/// twenty-four bits and a value outside it wraps to the other end of the
/// range, which a decoder reports as a saturated recorrelator and a listener
/// hears as a crack. Counted, because a fold that clips is a fold whose
/// objects wanted more headroom than the mix left, and that is worth knowing
/// rather than swallowing.
///
/// Counted against the domain *before* the step is applied: a sample the mix
/// pushed past full scale is a clip, and one the rounding nudged over is not.
#[inline]
fn quantise(value: f32, step: f64, peak: &mut f64, clipped: &mut u64) -> i32 {
    let raw = (f64::from(value) * FULL_SCALE).round();
    *peak = peak.max(raw.abs() / FULL_SCALE);
    if !(FLOOR..=CEILING).contains(&raw) {
        *clipped += 1;
    }
    let ceiling = (CEILING / step).floor() * step;
    ((raw / step).round() * step).clamp(FLOOR, ceiling) as i32
}

/// Fold the block's objects into fewer elements, and say where they went.
#[allow(clippy::too_many_arguments)]
fn cluster_span(
    fold: &mut Fold,
    path: &std::path::Path,
    block: &[i32],
    source_channels: usize,
    frames: usize,
    lfe_channel: Option<usize>,
    elements: usize,
    out: &mut [i32],
) -> Result<Vec<u8>> {
    let Fold {
        shape,
        scene,
        signals,
        gains,
        mixed,
        from,
        limiter,
        renderers,
        floor,
        ..
    } = fold;
    let Shape::Clustered(clustered) = shape else {
        unreachable!("a clustering span over a clustering fold")
    };

    // The elements are placed for the energies of the window around this
    // block, so that one moving because a sound *appears* is already there
    // when it does — the geometry is this block's, and only the energy looks
    // around it. See `hz_cluster::smooth`. Every span's energies were
    // recorded as it was read, ahead of the fold, so the window is already
    // in; the last blocks of a stream have less after them, and are placed
    // for what there is.
    clustered
        .smoother
        .place(scene.objects(), &mut clustered.placing);
    let clustering = clustered
        .clusterer
        .cluster(&clustered.placing, clustered.previous.as_ref())
        .map_err(|why| Error::unsupported(path, why.to_string()))?;

    // What of the elements' movement a listener would notice, on the
    // positions as the stream will carry them rather than as the fold holds
    // them — see `hz_cluster::motion`.
    clustered.written.clear();
    clustered.written.extend(
        clustering
            .positions
            .iter()
            .map(|at| hz_meta::oamd::Position::from_master(*at).to_master()),
    );
    clustered.carried.clear();
    clustered
        .carried
        .extend((0..clustering.elements()).map(|element| {
            clustering
                .weights
                .iter()
                .zip(scene.objects())
                .map(|(row, object)| {
                    let weight = row[element];
                    object.energy.max(0.0) * weight * weight
                })
                .sum::<f64>()
        }));
    clustered
        .motion
        .record(&clustered.written, &clustered.carried);

    // What the weights ramp *from*: the previous block's, or this block's own
    // at the very start, where there is nothing to ramp from.
    from.clear();
    match &clustered.previous {
        Some(previous) if previous.weights.len() == clustering.weights.len() => {
            from.extend(previous.weights.iter().cloned());
        }
        _ => from.extend(clustering.weights.iter().cloned()),
    }

    hz_cluster::mix::mix(signals, gains, from, &clustering.weights, frames, mixed);
    if clustering.bounded > 0 {
        clustered.bounded += 1;
    }
    // Whatever the bound left goes through a limiter with one gain for
    // every element — see `hz_cluster::headroom`. The limiter counts its
    // own blocks.
    if let Some(limiter) = limiter {
        limiter.apply(mixed, CEILING / FULL_SCALE);
    }

    // The low frequency channel is *copied*, not mixed, so it is not rounded:
    // the fold is the lossy step and a pass-through is not part of it.
    let first = usize::from(lfe_channel.is_some());
    for sample in 0..frames {
        if let Some(channel) = lfe_channel {
            out[sample * elements] = block[sample * source_channels + channel];
        }
        for (element, signal) in mixed.iter().enumerate() {
            out[sample * elements + first + element] =
                quantise(signal[sample], fold.step, &mut fold.peak, &mut fold.clipped);
        }
    }

    // What it cost, over every presentation the stream will be played through.
    if let Ok(report) =
        hz_cluster::metric::error_with(scene.objects(), &clustering, renderers, floor)
    {
        fold.blocks += 1;
        fold.mean += report.mean;
        fold.worst = fold.worst.max(report.worst);
        fold.inaudible += report.inaudible as u64;
    }

    // The whole block to reach the new positions, which is what stops an
    // element's position stepping — the same argument as the weights', and the
    // same span, so a decoder's ramp and the mixer's are one movement.
    let ramp = Ramp::from_samples(frames as u16).unwrap_or(Ramp::Long);
    let payload = clustering
        .to_metadata(lfe_channel.is_some(), 0, ramp)
        .write()
        .map_err(|why| Error::unsupported(path, format!("metadata: {why}")))?;
    clustered.previous = Some(clustering);
    Ok(payload)
}

/// Keep the master's elements and pan its last few objects onto them.
///
/// The elements arrive as they are: element `e` carries its own channel, at
/// its own level, under its own metadata. A source is panned onto the elements
/// that are live this block — see [`hz_cluster::overlay`] — and added to them,
/// pre-divided by each element's *stated* gain so that what a decoder renders
/// is the element as it was plus the source where it asked to be.
///
/// # The fast path is the point
///
/// An element no source reaches this block, and reached by none last block
/// either, is **copied**. Not summed with a row of zeroes and not rounded to
/// `--fold-depth`: the samples the master brought, written out unchanged. On a
/// dub that is most of the programme, and it is the difference between a
/// re-voiced stream and a re-encoded one. The low frequency channel is always
/// copied, as it is in a clustering fold, and so is an element a source took a
/// spare slot in, which carries that source alone.
#[allow(clippy::too_many_arguments)]
fn overlay_span(
    fold: &mut Fold,
    path: &std::path::Path,
    states: &[(usize, Keyframe)],
    parts: &[Part],
    block: &[i32],
    source_channels: usize,
    frames: usize,
    lfe_channel: Option<usize>,
    elements: usize,
    out: &mut [i32],
) -> Result<Vec<u8>> {
    let Fold {
        shape,
        scene,
        signals,
        gains,
        mixed,
        from,
        limiter,
        floor,
        renderers,
        ..
    } = fold;
    let Shape::Overlaid(overlaid) = shape else {
        unreachable!("an overlay span over an overlay fold")
    };
    let first = usize::from(lfe_channel.is_some());
    let sources = overlaid.sources;
    let objects = signals.len();
    // The master's own objects, which are the elements, and the ones appended
    // after them, which are the sources. An object's index among the signals
    // is its part's, less the low frequency channel that has no signal of its
    // own here.
    let carried = objects.saturating_sub(sources);

    // Where each element is, and what a decoder will apply to it. Both are the
    // master's, read off this block's state; the elements are the parts before
    // the sources, and a spare slot's element is the source that took it.
    overlaid.carriers.clear();
    overlaid.positions.clear();
    overlaid.stated.clear();
    let mut stated = [1.0f64; hz_mlp::format::MAX_CHANNELS];
    for (part_index, state) in states.iter().take(overlaid.elements) {
        let element = *part_index;
        let Part::Object { bed, .. } = &parts[element] else {
            // The low frequency channel has no position and is never panned
            // onto: vector-base panning needs a direction and it has none, and
            // the presentations fold it by its own rule.
            continue;
        };
        overlaid.positions.push(state.position);
        // An element a decoder will silence carries nothing a source could be
        // heard through, and dividing by its gain would divide by nought.
        // *Muted*, and not "inactive": the master's keyframes carry no active
        // flag, so a silenced element is one whose stated gain is nought and
        // nothing here can see any other kind.
        let gain = stated_gain(state.gain);
        stated[element] = gain;
        overlaid.stated.push(gain);
        if gain <= 0.0 {
            continue;
        }
        overlaid.carriers.push(hz_cluster::overlay::Carrier {
            element,
            position: state.position,
            bed: overlaid.beds.get(element).copied().unwrap_or(bed.is_some()),
        });
    }
    // A source that took a spare element of its own is an element the
    // presentations fold like any other, and it is never a carrier: the scene
    // a source is panned onto is the master's, not the other sources'. Its
    // audio is the source's own, and the payload states the source's gain
    // for it, so that is the gain it is folded at.
    for index in &overlaid.slots {
        let state = &states[overlaid.elements + index].1;
        overlaid.positions.push(state.position);
        overlaid.stated.push(stated_gain(state.gain));
    }

    // Fit the sources onto them.
    let scene_objects = scene.objects();
    overlaid.fitting.clear();
    for index in 0..sources {
        let object = carried + index;
        overlaid.fitting.push(hz_cluster::Object {
            position: states[overlaid.elements + index].1.position,
            energy: scene_objects.get(object).map_or(0.0, |o| o.energy),
            size: states[overlaid.elements + index].1.spread,
            pinned: None,
            mode: states[overlaid.elements + index].1.mode,
            peak: 0.0,
        });
    }
    overlaid.overlay.fit(
        elements,
        &overlaid.carriers,
        &overlaid.fitting,
        Some(&overlaid.previous),
        &mut overlaid.weights,
    );

    // What the fit did to each source, and whether it is good enough to ship.
    //
    // This runs *before* the mix, because the fallback changes the weights:
    // a source the fit could not place acceptably is routed to the nearest
    // bed outright, which is audible and slightly misplaced rather than
    // diffuse and wandering. See [`Bounds`].
    overlaid.resultants.clear();
    overlaid.energies.clear();
    overlaid.run.resize(overlaid.sources, 0);
    overlaid.resting.resize(overlaid.sources, None);
    overlaid.stranded_run.resize(overlaid.sources, 0);
    std::mem::swap(&mut overlaid.was_audible, &mut overlaid.audible);
    overlaid.audible.clear();
    overlaid.audible.resize(overlaid.sources, false);
    overlaid.was_audible.resize(overlaid.sources, false);
    for index in 0..overlaid.sources {
        let source = overlaid.fitting[index];
        let slotted = overlaid.slotted[index];
        // A source with an element of its own is exactly where it asked to
        // be, and a source nobody can hear is not a fact about the
        // programme. Neither is judged, and neither breaks a run.
        let voiced = !slotted && floor.heard(source.energy.max(0.0));
        overlaid.audible[index] = voiced;
        overlaid
            .energies
            .push(if voiced { source.energy.max(0.0) } else { 0.0 });
        if !voiced {
            // **A source nobody can hear carries nothing** — but its weights
            // are left exactly as they were.
            //
            // Zeroing them was the first answer, and it put a fade at every
            // phrase onset: the next audible block then ramped the voice up
            // from nought over 27 ms, because a weight that changed is a
            // weight the mixer crossfades. The weights were never the problem.
            // A silent source contributes `w · 0` whatever `w` is, so what has
            // to change for an element to be *copied* is not the weight but
            // the question asked of it — which is now whether any source
            // reaching it was **audible**, this block or the last. See where
            // `touched` is decided.
            //
            // Keeping them also keeps the held set: the fit is handed the last
            // real answer rather than a row of noughts, so hysteresis survives
            // a pause instead of restarting after every phrase.
            // Where it went is held at the last place it was heard, not reset
            // to where it asked to be. `Motion` judges a window by the
            // loudest the source got anywhere in it, so a window straddling a
            // phrase boundary is judged — and pushing the asked direction
            // through the silence would show it leaving its carriers and
            // coming back, an out-and-back of twice the drift at every
            // onset and release. That is the guard reporting punctuation.
            let held = overlaid
                .resting
                .get(index)
                .copied()
                .flatten()
                .unwrap_or_else(|| hz_cluster::direction(source.position));
            overlaid.resultants.push(held);
            // And silence ends a run: two phrases either side of a pause are
            // two runs, not one long one.
            overlaid.run[index] = 0;
            overlaid.stranded_run[index] = 0;
            continue;
        }

        // The source's weights over this block's carriers rather than over
        // every element, which is the shape the measures want. One buffer,
        // refilled: this runs once per audible source per block.
        let row = &mut overlaid.row;
        row.clear();
        row.extend(
            overlaid
                .carriers
                .iter()
                .map(|carrier| overlaid.weights[index][carrier.element]),
        );
        let mut drift = hz_cluster::overlay::drift(source.position, &overlaid.carriers, row);
        let mut spread = hz_cluster::overlay::spread(row);

        // The fallback, on the two conditions a fallback can fix: the source
        // went nowhere at all, or it went somewhere far enough off, or it
        // went everywhere at once. Where it went slightly wrong the fit's
        // answer is better than a bed's, and it is left alone.
        let unplaceable = drift.is_none()
            || drift.is_some_and(|at| overlaid.bounds.drift > 0.0 && at > overlaid.bounds.drift)
            || (overlaid.bounds.spread > 0.0 && spread.strongest < overlaid.bounds.spread);
        if unplaceable && overlaid.bounds.fallback {
            if let Some(slot) =
                hz_cluster::overlay::nearest_bed(&overlaid.carriers, source.position)
            {
                row.iter_mut().for_each(|weight| *weight = 0.0);
                row[slot] = 1.0;
                for (carrier, weight) in overlaid.carriers.iter().zip(row.iter()) {
                    overlaid.weights[index][carrier.element] = *weight;
                }
                drift = hz_cluster::overlay::drift(source.position, &overlaid.carriers, row);
                spread = hz_cluster::overlay::spread(row);
                overlaid.fell_back += 1;
            }
        }

        match drift {
            Some(at) => {
                overlaid.drift += at;
                overlaid.drifted += 1;
                overlaid.worst_drift = overlaid.worst_drift.max(at);
                if overlaid.bounds.drift > 0.0 && at > overlaid.bounds.drift / 2.0 {
                    overlaid.over_half_drift += 1;
                }
                if overlaid.bounds.drift > 0.0 && at > overlaid.bounds.drift {
                    overlaid.over_drift += 1;
                    overlaid.run[index] += 1;
                    overlaid.worst_run = overlaid.worst_run.max(overlaid.run[index]);
                } else {
                    overlaid.run[index] = 0;
                }
                overlaid.stranded_run[index] = 0;
                let went = hz_cluster::overlay::resultant(&overlaid.carriers, row)
                    .unwrap_or_else(|| hz_cluster::direction(source.position));
                overlaid.resting[index] = Some(went);
                overlaid.resultants.push(went);
            }
            // Audible, and carried by nothing at all: every element silenced
            // by the gain it states, and no bed to fall back to either.
            // Counted rather than guessed at.
            None => {
                // A source that went nowhere was still *audible*, so it counts
                // towards the blocks the shares are over. Without this the
                // denominator was the blocks that were placed, and a run
                // stranded throughout divided by one.
                overlaid.stranded += 1;
                overlaid.drifted += 1;
                overlaid.run[index] = 0;
                overlaid.stranded_run[index] += 1;
                overlaid.worst_stranded_run = overlaid
                    .worst_stranded_run
                    .max(overlaid.stranded_run[index]);
                let held = overlaid
                    .resting
                    .get(index)
                    .copied()
                    .flatten()
                    .unwrap_or_else(|| hz_cluster::direction(source.position));
                overlaid.resultants.push(held);
            }
        }
        if overlaid.bounds.spread > 0.0 && spread.strongest < overlaid.bounds.spread {
            overlaid.over_spread += 1;
        }
    }

    // What the sources cost and what level they come out at, on every
    // presentation the stream is played through — the elements as the master
    // placed them, the sources' own weights, and `hz_cluster::metric`'s own
    // measures over the two. The elements are not in this: they are not
    // folded, so they cost nothing by construction.
    overlaid.judged.positions.clear();
    overlaid
        .judged
        .positions
        .extend(overlaid.carriers.iter().map(|carrier| carrier.position));
    // Only the sources that were actually panned. One that took a spare
    // element of its own is not folded at all — it *is* an element, carried
    // whole and rendered from its own place — so judging its unused pan lets
    // a source that costs nothing by construction push the guards around.
    overlaid.panned.clear();
    let mut judged = 0usize;
    for index in 0..overlaid.sources {
        if overlaid.slotted[index] {
            continue;
        }
        // Filled in place: the rows are the same shape block after block once
        // the carrier count settles, so this allocates on the first block and
        // never again.
        if overlaid.judged.weights.len() <= judged {
            overlaid.judged.weights.push(Vec::new());
        }
        let row = &mut overlaid.judged.weights[judged];
        row.clear();
        row.extend(
            overlaid
                .carriers
                .iter()
                .map(|carrier| overlaid.weights[index][carrier.element]),
        );
        overlaid.panned.push(overlaid.fitting[index]);
        judged += 1;
    }
    overlaid.judged.weights.truncate(judged);
    if !overlaid.carriers.is_empty()
        && !overlaid.panned.is_empty()
        && let Ok(report) =
            hz_cluster::metric::error_with(&overlaid.panned, &overlaid.judged, renderers, floor)
    {
        fold.mean += report.mean;
        fold.worst = fold.worst.max(report.worst);
        if report.worst > overlaid.worst_cost {
            overlaid.worst_cost = report.worst;
            // Which presentation it was worst on, and what the source asked
            // for there. A cost is a *relative* error — how far the carriers
            // land from the source, over what the source itself radiates on
            // that layout — so a refusal that does not name the layout leaves
            // a caller unable to tell a real geometric miss from a small
            // denominator. Kept only for the worst block, which is the one
            // the refusal quotes.
            overlaid.worst_cost_where.clear();
            for layer in &report.layouts {
                if !overlaid.worst_cost_where.is_empty() {
                    overlaid.worst_cost_where.push_str(", ");
                }
                // The cost *and* its denominator. A relative error cannot be
                // read without the vector it is relative to: a presentation
                // the source barely reaches has a small own norm, and a small
                // absolute error over it is a large relative one. Printing
                // both is what lets a caller tell that case from a real miss
                // without taking anybody's word for it.
                overlaid.worst_cost_where.push_str(&format!(
                    "{} {:.2} of {:.2}",
                    layer.name, layer.worst, layer.own
                ));
            }
        }
        if overlaid.bounds.cost > 0.0 && report.worst > overlaid.bounds.cost {
            overlaid.over_cost += 1;
        }
        // The level on the *worst* presentation and not the mean of them: a
        // voice that survives 7.1.4 and vanishes in stereo has vanished.
        let strayed = report
            .layouts
            .iter()
            .map(|layer| {
                if layer.energy > 1e-6 {
                    (20.0 * layer.energy.log10()).abs()
                } else {
                    0.0
                }
            })
            .fold(0.0f64, f64::max);
        overlaid.worst_level = overlaid.worst_level.max(strayed);
        if overlaid.bounds.level > 0.0 && strayed > overlaid.bounds.level {
            overlaid.over_level += 1;
        }
    }
    // Where the carriers put the sources, block after block. The sources are
    // static, so anything this calls movement is movement nobody asked for.
    overlaid
        .motion
        .record(&overlaid.resultants, &overlaid.energies);
    if overlaid.energies.iter().any(|energy| *energy > 0.0) {
        overlaid.voiced += 1;
    }
    fold.blocks += 1;

    // Which elements are mixed and which are copied. An element is mixed if a
    // source reaches it now *or* reached it last block, because the weights
    // ramp across the boundary and a ramp from something to nothing is still
    // something for most of the block.
    overlaid.copy_from.clear();
    overlaid.copy_from.resize(elements, None);
    overlaid.touched.clear();
    overlaid.touched.resize(elements, false);
    for (index, row) in overlaid.weights.iter().enumerate() {
        if overlaid.slotted[index] {
            continue;
        }
        // A source that nobody could hear in this block or the last reaches
        // nothing, whatever its weights say: it contributed `w · 0` to both,
        // so the elements it names are the master's own samples and are
        // copied. One block of memory because the weights ramp across a
        // boundary — the block after a source falls silent still carries the
        // tail of the block before it.
        if !overlaid.audible[index] && !overlaid.was_audible[index] {
            continue;
        }
        let was = overlaid.previous.get(index);
        for element in 0..elements {
            if row[element] != 0.0 || was.is_some_and(|was| was[element] != 0.0) {
                overlaid.touched[element] = true;
            }
        }
    }
    for (element, part) in parts.iter().enumerate().take(overlaid.elements) {
        if !overlaid.touched[element]
            && let Some(channel) = part.source_channel()
        {
            overlaid.copy_from[element] = Some(channel);
        }
    }
    // A source that took a spare slot is an element of its own carrying
    // nothing else, so it is copied too, and its metadata is its own.
    for (slot, index) in overlaid.slots.iter().enumerate() {
        let element = overlaid.elements + slot;
        if let Some(channel) = parts[overlaid.elements + index].source_channel() {
            overlaid.copy_from[element] = Some(channel);
        }
    }

    // The mix matrix over every object: an element carries itself at unity
    // when it is mixed at all, and a source is spread over the carriers,
    // divided by each one's stated gain so the sum renders where it asked to.
    overlaid.mix.clear();
    overlaid.mix.resize_with(objects, Vec::new);
    for row in overlaid.mix.iter_mut() {
        row.clear();
        row.resize(elements, 0.0);
    }
    for object in 0..carried {
        let element = object + first;
        if overlaid.copy_from[element].is_none() {
            overlaid.mix[object][element] = 1.0;
        }
        // An element's own audio goes through as it is: the level a decoder
        // applies is in the metadata, which is the master's, so applying it
        // here as well would apply it twice.
        gains[object] = 1.0;
    }
    for index in 0..sources {
        if overlaid.slotted[index] {
            continue;
        }
        for element in 0..elements {
            let weight = overlaid.weights[index][element];
            if weight != 0.0 && stated[element] > 0.0 {
                overlaid.mix[carried + index][element] = weight / stated[element];
            }
        }
    }

    // What the weights ramp from: the previous block's matrix, or this one's
    // where there is nothing to ramp from.
    if from.len() != objects || from.first().is_none_or(|row| row.len() != elements) {
        from.clear();
        from.extend(overlaid.mix.iter().cloned());
    }
    // **An element's own signal is never ramped.** Only a source's weight is.
    //
    // A copied element has no row in the matrix at all — that is what makes it
    // free — so the block after a source first reaches it, the previous
    // matrix has a nought on its diagonal and this one has a one. Left alone,
    // `hz_cluster::mix::mix` reads that as a weight to interpolate and fades
    // the element's *own* audio up from silence across the block: 27 ms of
    // the programme nobody touched, arriving at 0.13, 0.38, 0.62 and 0.88 of
    // itself over the four quarters of the block, with a step at each end.
    // Every change of carrier set does it — a held set flipping, a fallback
    // taken, a moving carrier arriving, an element unmuting — so on a dub it
    // is a hole and a click in the M&E at every phrase.
    //
    // The diagonal is therefore forced to what this block asks for before the
    // ramp is applied. A source's weights still ramp, which is what the
    // crossfade is for; the audio that was already there does not.
    for object in 0..carried {
        let element = object + first;
        let carries_itself = overlaid.mix[object][element];
        if carries_itself != 0.0 {
            from[object][element] = carries_itself;
        }
    }
    hz_cluster::mix::mix(signals, gains, from, &overlaid.mix, frames, mixed);
    let mut limited = false;
    if let Some(limiter) = limiter {
        // Only the elements this block actually mixed. The rest are copied
        // straight out of the master and never reach `mixed` at all, so their
        // rows are noughts the limiter would otherwise scan twice a block.
        overlaid.live.clear();
        overlaid
            .live
            .extend(overlaid.copy_from.iter().map(Option::is_none));
        limited = limiter.apply_to(mixed, CEILING / FULL_SCALE, &overlaid.live);
    }

    // Element by element rather than sample by sample: which of the two an
    // element is was decided once for the whole block, so asking again at
    // every sample is a branch a million times a second that always answers
    // the same. Most elements are the copy, which is now a strided run.
    for element in 0..elements {
        match overlaid.copy_from[element] {
            Some(channel) => {
                for sample in 0..frames {
                    out[sample * elements + element] = block[sample * source_channels + channel];
                }
            }
            None => {
                let signal = &mixed[element];
                for sample in 0..frames {
                    out[sample * elements + element] =
                        quantise(signal[sample], fold.step, &mut fold.peak, &mut fold.clipped);
                }
            }
        }
    }
    if overlaid.copy_from.iter().all(Option::is_some) {
        overlaid.whole += 1;
    }
    overlaid.copied += overlaid
        .copy_from
        .iter()
        .filter(|from| from.is_some())
        .count() as u64;
    overlaid.remixed += overlaid
        .copy_from
        .iter()
        .filter(|from| from.is_none())
        .count() as u64;

    // What this block did, for a checker that has to reproduce it. Written
    // before the buffers are swapped, while `overlaid.mix` still holds the
    // weights this block ended at.
    if overlaid.report.is_some() {
        write_overlay_report(
            overlaid, parts, gains, from, carried, frames, elements, limited,
        )
        .map_err(|e| Error::io(path, e))?;
    }
    overlaid.reported_blocks += 1;
    overlaid.reported_at += frames as u64;

    std::mem::swap(from, &mut overlaid.mix);
    std::mem::swap(&mut overlaid.previous, &mut overlaid.weights);

    // No payload from here. An overlay's element metadata is the master's and
    // the master states it once an access unit, so the producer writes one per
    // unit from `Waiting::units`; building a thirty-third one from the block's
    // last state would be the same bytes computed twice.
    let _ = path;
    Ok(Vec::new())
}

/// Whether the elements say what they said at the unit before.
///
/// The cheap half of deciding a payload. Two states that differ still often
/// *write* the same bytes — the wire codes a position to a sixty-second of the
/// room — so this is a fast reject and not the decision: where it says nothing
/// moved, nothing is written and nothing is serialised; where it says
/// something did, the bytes are compared as before and may still turn out
/// equal. The answer is therefore exactly what it was, at a fraction of the
/// work, and on a dub nearly every unit takes the cheap path.
fn held(said: &[Keyframe], states: &[(usize, Keyframe)], kept: usize) -> bool {
    let kept = kept.min(states.len());
    said.len() == kept
        && said
            .iter()
            .zip(states.iter().take(kept))
            .all(|(before, (_, now))| before == now)
}

fn remember(said: &mut Vec<Keyframe>, states: &[(usize, Keyframe)], kept: usize) {
    said.clear();
    said.extend(states.iter().take(kept).map(|(_, state)| *state));
}

/// One block of the account described at [`Overlaid::report`].
///
/// A line-based format on purpose: it is read by one tool, it has to survive
/// being looked at by eye when that tool and this disagree, and a structured
/// one would put a parser between the encoder's answer and the question being
/// asked of it.
///
/// ```text
/// element <index> <source channel>    once, before any block: which master
///                                     channel the element's own audio is —
///                                     a copy names it too, but an element
///                                     mixed on every block is never copied
/// block <index> <first sample> <frames> <limited>
/// copy <element> <source channel>     an element taken from the master whole
/// mix <element>                       an element the block summed
/// w <source> <element> <from> <to>    a weight, already divided by the
///                                     element's stated gain, ramped across
///                                     the block from `from` to `to`
/// g <source> <gain>                   the source's own gain, applied before
///                                     the weight, as `hz_cluster::mix` does
/// ```
#[allow(clippy::too_many_arguments)]
fn write_overlay_report(
    overlaid: &mut Overlaid,
    parts: &[Part],
    gains: &[f64],
    from: &[Vec<f64>],
    carried: usize,
    frames: usize,
    elements: usize,
    limited: bool,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let block = overlaid.reported_blocks;
    let at = overlaid.reported_at;
    // Split the borrow: the writer is one field and everything it reads is
    // another, which is what lets this take `&mut Overlaid` and still read.
    let Overlaid {
        report,
        copy_from,
        mix,
        sources,
        slotted,
        ..
    } = overlaid;
    let Some(out) = report.as_mut() else {
        return Ok(());
    };
    if block == 0 {
        for (element, part) in parts.iter().enumerate().take(elements) {
            if let Some(channel) = part.source_channel() {
                writeln!(out, "element {element} {channel}")?;
            }
        }
    }
    writeln!(out, "block {block} {at} {frames} {}", u8::from(limited))?;
    for (element, from) in copy_from.iter().enumerate().take(elements) {
        match from {
            Some(channel) => writeln!(out, "copy {element} {channel}")?,
            None => writeln!(out, "mix {element}")?,
        }
    }
    for index in 0..*sources {
        if slotted[index] {
            continue;
        }
        let row = carried + index;
        writeln!(out, "g {index} {:.17e}", gains[row])?;
        for element in 0..elements {
            let to = mix[row][element];
            let was = from
                .get(row)
                .and_then(|row| row.get(element))
                .copied()
                .unwrap_or(to);
            if to != 0.0 || was != 0.0 {
                writeln!(out, "w {index} {element} {was:.17e} {to:.17e}")?;
            }
        }
    }
    Ok(())
}

/// What an overlay states for its elements at one instant.
///
/// The elements the master brought, each saying exactly what the master said
/// of it, and then the sources that took a spare element, each saying what it
/// says of itself. Nothing here is derived from the fold: an overlay places no
/// element, so its metadata is a copy and not a result — which is why this can
/// be called once an access unit while the mix is decided once a block.
fn overlay_payload(
    overlaid: &Overlaid,
    path: &std::path::Path,
    parts: &[Part],
    states: &[(usize, Keyframe)],
    stated: &mut ObjectAudioMetadata,
) -> Result<Vec<u8>> {
    // The caller's payload, refilled: the one block it holds and that block's
    // element list are the caller's and are reused unit after unit, never
    // taken. What is allocated here is the bytes the payload serialises to,
    // and those are the payload's own — they travel with it to the writer —
    // so a unit in which nothing moved (which `held` decides before this is
    // reached) allocates nothing at all.
    let ramp = ramp_of_states(states, parts, overlaid.elements);
    stated.lfe = matches!(parts.first(), Some(Part::Lfe { .. }));
    if stated.blocks.is_empty() {
        stated.blocks.push(Block {
            offset: 0,
            ramp,
            objects: Vec::new(),
        });
    }
    let block = &mut stated.blocks[0];
    block.ramp = ramp;
    block.objects.clear();
    for (part_index, state) in states.iter().take(overlaid.elements) {
        block.objects.push(object_of(&parts[*part_index], state));
    }
    for index in &overlaid.slots {
        let part = overlaid.elements + index;
        block.objects.push(object_of(&parts[part], &states[part].1));
    }
    stated
        .write()
        .map_err(|why| Error::unsupported(path, format!("metadata: {why}")))
}

/// The ramp the master asks for over the elements it brought, which is what an
/// overlay states: the positions are the master's, so the time a decoder is
/// given to reach them is the master's too.
fn ramp_of_states(states: &[(usize, Keyframe)], parts: &[Part], elements: usize) -> Ramp {
    let longest = states
        .iter()
        .take(elements)
        .filter(|(part_index, _)| matches!(parts[*part_index], Part::Object { .. }))
        .map(|(_, state)| state.ramp_samples)
        .max()
        .unwrap_or(0);
    if longest == 0 {
        Ramp::None
    } else {
        Ramp::Samples(longest.min(2047) as u16)
    }
}

/// One thing the guards found, and how seriously.
struct Remark {
    refused: bool,
    said: String,
}

/// What the guards say about the stream that was just written.
///
/// Every share is over the blocks a source was **audible** in, and the
/// sources that took a spare element of their own are not judged at all: they
/// are exactly where they asked to be, by construction.
///
/// The stream is **left on disk** when this refuses. A refusal is a statement
/// about what is in the file, and the fastest way to check a bound nobody has
/// listened to yet is to listen to what it stopped.
fn overlay_remarks(overlaid: &Overlaid, fold: &Fold, seconds_a_block: f64) -> Vec<Remark> {
    let mut out = Vec::new();
    let bounds = overlaid.bounds;
    let voiced = overlaid.voiced.max(1) as f64;
    let placed = overlaid.drifted.max(1) as f64;
    let mut say = |refused: bool, said: String| out.push(Remark { refused, said });

    if bounds.drift > 0.0 && overlaid.drifted > 0 {
        let share = overlaid.over_drift as f64 / placed;
        let run = overlaid.worst_run as f64 * seconds_a_block;
        if share > OVERLAY_SHARE {
            say(
                true,
                format!(
                    "{:.1} % of the blocks a source was audible in put it more than {:.0}° from \
                     where it asked to be, which is past the {:.0} % this refuses at",
                    100.0 * share,
                    bounds.drift,
                    100.0 * OVERLAY_SHARE
                ),
            );
        } else if run > OVERLAY_RUN {
            say(
                true,
                format!(
                    "a source stayed more than {:.0}° from where it asked to be for {run:.1} s \
                     without a break, which is past the {OVERLAY_RUN:.0} s this refuses at — a \
                     share says how much of a programme is wrong and not whether it is wrong all \
                     at once",
                    bounds.drift
                ),
            );
        } else if overlaid.worst_drift > bounds.drift / 2.0 {
            say(
                false,
                format!(
                    "a source reached {:.1}° from where it asked to be, past the {:.0}° a rear \
                     blur stops forgiving; {:.1} % of the audible blocks are over that",
                    overlaid.worst_drift,
                    bounds.drift / 2.0,
                    // The share over the *half* bound, which is what this
                    // sentence is about. Printing the share over the refusal
                    // bound here said "0.0 % over 15°" on a run whose mean
                    // drift was 15.4°, which is a number that contradicts the
                    // sentence around it.
                    100.0 * overlaid.over_half_drift as f64 / placed
                ),
            );
        }
    }

    if bounds.spread > 0.0 && overlaid.drifted > 0 {
        let share = overlaid.over_spread as f64 / placed;
        if share > OVERLAY_SHARE {
            say(
                true,
                format!(
                    "{:.1} % of the blocks a source was audible in left no element holding {:.2} \
                     of it, so it is rendered from everywhere the fit reached rather than from \
                     somewhere",
                    100.0 * share,
                    bounds.spread
                ),
            );
        } else if share > 0.0 {
            say(
                false,
                format!(
                    "{:.1} % of the audible blocks spread a source with no element holding {:.2} \
                     of it",
                    100.0 * share,
                    bounds.spread
                ),
            );
        }
    }

    if bounds.wobble > 0.0 && overlaid.motion.windows() > 0 {
        let wobbling = 100.0 * overlaid.motion.wobbling();
        if wobbling > bounds.wobble {
            say(
                true,
                format!(
                    "{wobbling:.2} % of the windows a source was heard in carry movement nobody \
                     asked for — a carrier moved under a source that did not — which is past the \
                     {:.0} % this refuses at; {:.2}°/s of it",
                    bounds.wobble,
                    overlaid.motion.invented()
                ),
            );
        } else if wobbling > bounds.wobble / 5.0 {
            say(
                false,
                format!(
                    "{wobbling:.2} % of the windows a source was heard in carry movement nobody \
                     asked for, {:.2}°/s of it — see hz_cluster::motion",
                    overlaid.motion.invented()
                ),
            );
        }
    }

    if overlaid.stranded > 0 {
        // On the same share and the same run as drift. A source-block with
        // nowhere to go is a hole in the dub, but one of them is 27 ms and a
        // programme is not unshippable for it; what makes it unshippable is
        // that it keeps happening, or that it happens for a whole phrase.
        let share = overlaid.stranded as f64 / placed.max(1.0);
        let run = overlaid.worst_stranded_run as f64 * seconds_a_block;
        let said = format!(
            "{} source-blocks ({:.1} %) were audible with no element to carry them at all — \
             every candidate muted, and no bed to fall back to — the longest stretch {run:.1} s",
            overlaid.stranded,
            100.0 * share
        );
        say(share > OVERLAY_SHARE || run > OVERLAY_RUN, said);
    }

    if bounds.level > 0.0 && overlaid.voiced > 0 {
        let share = overlaid.over_level as f64 / voiced;
        if share > OVERLAY_SHARE {
            say(
                true,
                format!(
                    "{:.1} % of the voiced blocks render a source more than {:.1} dB from the \
                     level it asked for on some presentation, {:.2} dB at its worst — a voice \
                     that survives the wide layout and vanishes in the downmix",
                    100.0 * share,
                    bounds.level,
                    overlaid.worst_level
                ),
            );
        } else if overlaid.worst_level > bounds.level / 2.0 {
            say(
                false,
                format!(
                    "a source's rendered level strayed {:.2} dB on some presentation",
                    overlaid.worst_level
                ),
            );
        }
    }

    if bounds.cost > 0.0 && overlaid.voiced > 0 {
        let share = overlaid.over_cost as f64 / voiced;
        if share > OVERLAY_SHARE {
            say(
                true,
                format!(
                    "{:.1} % of the voiced blocks cost the worst source more than {:.2} of its \
                     own gain vector, {:.3} at its worst ({}). A source at a speaker's place \
                     costs nothing; this much means the elements the master brought cannot \
                     reach where the source asked to be",
                    100.0 * share,
                    bounds.cost,
                    overlaid.worst_cost,
                    overlaid.worst_cost_where
                ),
            );
        } else if fold.blocks > 0 && fold.mean / fold.blocks as f64 > 0.05 {
            say(
                false,
                format!(
                    "the sources cost {:.4} of their own gains on average",
                    fold.mean / fold.blocks as f64
                ),
            );
        }
    }

    if fold.clipped > 0 {
        say(
            true,
            format!(
                "{} samples went outside the codec's domain and were clamped; an overlay that \
                 clips is one whose elements had no headroom left for what was added to them",
                fold.clipped
            ),
        );
    } else if let Some(limiter) = &fold.limiter
        && limiter.limited > 0
    {
        say(
            false,
            format!(
                "{} blocks where the limiter brought every element down, the mix peaking at \
                 {:.2} of full scale before it",
                limiter.limited, limiter.peak
            ),
        );
    }
    out
}

/// What an overlay did, in the encoder's own summary.
///
/// The two numbers that matter are how much of the programme came through
/// untouched and how far the sources ended up from where they asked to be.
/// Everything else an overlay could report is the clustering's and is not
/// stated here, because an overlay does not do it.
fn overlay_summary(overlaid: &Overlaid, blocks: u64, config: &Config) {
    let element_blocks = overlaid.copied + overlaid.remixed;
    println!(
        "  overlay      {} elements kept, {} sources panned onto them{}",
        overlaid.elements,
        overlaid.sources - overlaid.slots.len(),
        if overlaid.slots.is_empty() {
            String::new()
        } else {
            format!(
                ", {} taking a spare element of its own",
                overlaid.slots.len()
            )
        }
    );
    if element_blocks > 0 {
        println!(
            "  untouched    {:.1} % of the element-blocks copied through, neither mixed nor \
             rounded ({} of {}); {:.1} % of the programme's blocks touched nothing at all",
            100.0 * overlaid.copied as f64 / element_blocks as f64,
            overlaid.copied,
            element_blocks,
            // The share of *time*, beside the share of element-blocks. They
            // answer different questions: how much of the scene came through
            // untouched, and how much of the running time nothing was mixed
            // in at all — which on a dub is the silence between the phrases,
            // and is what the fast path is worth.
            100.0 * overlaid.whole as f64 / blocks.max(1) as f64
        );
    }
    if overlaid.drifted > 0 {
        println!(
            "  drift        {:.2}° mean between where a source asked to be and where its \
             carriers put it, {:.2}° at its worst, over {:.2} audible sources a block",
            overlaid.drift / overlaid.drifted as f64,
            overlaid.worst_drift,
            overlaid.drifted as f64 / blocks.max(1) as f64,
        );
    }
    let beds = overlaid.beds.iter().filter(|bed| **bed).count();
    println!(
        "  beds         {} of {} elements are bed channels, {}",
        beds,
        overlaid.elements,
        match (beds, overlaid.declared_beds) {
            (0, _) => "so nothing prefers one and the fallback has nowhere to go".to_string(),
            (found, 0) => format!(
                "all {found} of them worked out from the master — static, and exactly at a \
                 speaker's place on the wire's own grid — since it declares none"
            ),
            (found, declared) if found == declared => format!("all {found} declared by the master"),
            (found, declared) => format!(
                "{declared} declared by the master and {} worked out from being static at a \
                 speaker's place",
                found - declared
            ),
        }
    );
    println!(
        "  carried by   {}",
        match config.overlay_beds {
            Some(reach) => format!(
                "the beds wherever a bed-only fit lands within {reach:.2} of what the source \
                 radiates, and the moving elements only where it does not"
            ),
            None => "whatever fits best, the moving elements included".to_string(),
        }
    );
    if overlaid.fell_back > 0 && overlaid.drifted > 0 {
        println!(
            "  fell back    {:.1} % of the audible source-blocks were put on the nearest bed \
             outright, the fit having placed them too far off or too widely ({} of {})",
            100.0 * overlaid.fell_back as f64 / overlaid.drifted as f64,
            overlaid.fell_back,
            overlaid.drifted
        );
    }
    if overlaid.motion.windows() > 0 {
        println!(
            "  unasked      {:.2} % of the windows a source was heard in carry movement nobody \
             asked for, {:.2}°/s of it — a carrier moved under a source that did not",
            100.0 * overlaid.motion.wobbling(),
            overlaid.motion.invented(),
        );
    }
    if overlaid.stranded > 0 {
        println!(
            "  stranded     {} source-blocks were audible with every element \
             silenced by their stated gain, and went nowhere",
            overlaid.stranded
        );
    }
}

/// Whether the stream carries the folds `--presentations` asked for, and why
/// the intervals that do not could not.
///
/// An interval whose folds are refused is written with the leading elements
/// in its presentations instead, which is a stream that plays and is lossless
/// and is not what was asked for — so it is said here rather than only under
/// `HZ_FOLD`.
fn presentations_summary(stats: &hz_mlp::encoder::Stats) {
    let refused = stats.folds_asked - stats.folds_carried;
    if stats.folds_asked == 0 {
        println!(
            "  presentations none: the programme is no wider than the widest fold, so the \
             leading elements are its presentations"
        );
    } else if refused == 0 {
        println!(
            "  presentations the folds themselves, in every one of {} restart intervals",
            stats.folds_asked
        );
    } else {
        let over = stats.folds_over_the_domain;
        let unwritable = refused - over;
        let mut why = Vec::with_capacity(2);
        if over > 0 {
            why.push(format!(
                "{over} would have left the codec's twenty-four bits"
            ));
        }
        if unwritable > 0 {
            why.push(format!("{unwritable} had rows no cascade could write"));
        }
        println!(
            "  presentations the folds in {} of {} restart intervals; the other {refused} carry \
             the leading elements — {} — see HZ_FOLD=1",
            stats.folds_carried,
            stats.folds_asked,
            why.join(" and ")
        );
    }
}

/// What the stream's presentations are, over the elements it carries.
///
/// A decoder may stop after any substream, and what it gets when it does is a
/// fold of the programme onto the layout that substream's presentation names —
/// 2.0, 5.1, 7.1, and then the elements themselves. The rows are the gains that
/// fold asks for, one row per channel, over the elements as this block's
/// clustering placed them.
///
/// The low frequency element is not folded. It is a bed: it goes to the low
/// frequency channel of any presentation that has one, at unity, and to nothing
/// at all in a presentation that has none — a stereo fold discards it, which is
/// what every stereo fold does.
///
/// `stated` is the gain a decoder gives each positioned element before it
/// renders it, when that is not unity — the master's own, for a stream that
/// carries the master's elements as they are. `None` is unity throughout,
/// which is what an element whose audio a fold has already mixed at its
/// objects' gains states.
fn presentations_of(
    positions: &[[f64; 3]],
    stated: Option<&[f64]>,
    lfe: bool,
) -> Option<Vec<Presentation>> {
    let first = usize::from(lfe);
    let elements = first + positions.len();
    let mut out = Vec::with_capacity(4);
    for layout in [
        Layout::stereo(),
        Layout::surround_5_1(),
        Layout::surround_7_1(),
    ] {
        let width = layout.channels();
        // A presentation as wide as the programme is the programme, and the
        // last one already is.
        if width >= elements {
            return None;
        }
        let fold = ObjectFold::for_layout(&layout).ok()?;
        let low = layout.index_of("LFE1");
        // An element's column at a time: the fold answers for every channel
        // of the layout at once.
        let mut rows = vec![vec![0.0f64; elements]; width];
        let mut gains = vec![0.0f64; width];
        for element in 0..elements {
            if lfe && element == 0 {
                if let Some(low) = low {
                    rows[low][element] = 1.0;
                }
                continue;
            }
            let index = element - first;
            let stated = stated.map_or(1.0, |stated| stated[index]);
            if stated == 0.0 {
                continue;
            }
            fold.gains(positions[index], &mut gains);
            for (row, gain) in rows.iter_mut().zip(&gains) {
                row[element] = gain * stated;
            }
        }
        out.push(Presentation {
            channels: width,
            rows,
        });
    }
    out.push(Presentation {
        channels: elements,
        rows: (0..elements)
            .map(|element| {
                let mut row = vec![0.0; elements];
                row[element] = 1.0;
                row
            })
            .collect(),
    });
    Some(out)
}

/// The presentations of a stream that carries the master's elements as they
/// are: each folded from where the master puts it, at the gain the master
/// states for it, which is what a decoder rendering the elements applies.
///
/// A padding element is silent and folds to nothing, and so does an element
/// the master has switched off. `positions` and `stated` are the caller's, so
/// that restating the folds as the elements move allocates nothing for them.
fn plain_presentations(
    live: &[Live],
    parts: &[Part],
    positions: &mut Vec<[f64; 3]>,
    stated: &mut Vec<f64>,
) -> Option<Vec<Presentation>> {
    let lfe = matches!(parts.first(), Some(Part::Lfe { .. }));
    positions.clear();
    stated.clear();
    for (part, entry) in parts.iter().zip(live).skip(usize::from(lfe)) {
        debug_assert!(std::ptr::eq(part, &parts[entry.part_index]));
        match part {
            Part::Object { .. } => {
                positions.push(entry.state.position);
                stated.push(stated_gain(entry.state.gain));
            }
            Part::Absent => {
                positions.push([0.0; 3]);
                stated.push(0.0);
            }
            // Only ever the first part, which is skipped.
            Part::Lfe { .. } => return None,
        }
    }
    presentations_of(positions, Some(stated), lfe)
}

/// Full scale for the codec's twenty-four bit domain, and its two ends.
const FULL_SCALE: f64 = 8_388_608.0;
const CEILING: f64 = 8_388_607.0;
const FLOOR: f64 = -8_388_608.0;

/// What a folded element's samples are rounded to.
///
/// # Why a fold rounds at all
///
/// Sixteen objects do not go into eleven elements without loss — that is what
/// a fold *is* — and once a step has accepted losing the difference between
/// two directions, keeping the difference between two values 2^-24 apart is
/// not a principle, it is an oversight. A mix of objects fills all
/// twenty-four bits with a product of real weights, and a lossless coder then
/// has to carry every one of them: the low four are noise from the
/// multiplication and cost about a fifth of the stream.
///
/// Measured on a two-hour programme folded 17 → 12: the reference encoder's
/// elements have exactly four dead low bits everywhere, ours had none, and
/// the difference accounts for the whole of a 1.33× size gap — our coder is
/// 0.95× the reference on identical audio. See `docs/encode.md`.
///
/// Twenty bits puts the rounding noise at about −120 dBFS, under the
/// programme's own noise floor and under what any playback chain resolves.
/// `--full-depth` declines the trade.
pub const FOLD_BITS: u32 = 20;

/// What the stream states as its dynamic range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Drc {
    /// A gain per access unit, from the curve measured off shipped streams and
    /// the level of each presentation — see [`Loudness`] and `docs/drc.md`.
    Measured,
    /// One gain, stated once and never moved, for a caller that knows better
    /// or is comparing.
    Constant(f64),
}

/// How long a dynamic range word stands, as a power of two access units.
///
/// Seven is 128 units, which is what the reference streams use and what this
/// encoder already restates its object metadata on. The field holds three
/// bits, so seven is also as long as a word may stand at all.
pub const DRC_REFRESH: u32 = hz_mlp::format::DynamicRange::MAX_REFRESH;

/// The passes `--fold-search` will accept.
///
/// Each pass looks half as far as the last, so eight takes the reach from a
/// tenth of the cube to a two-thousand-five-hundred-and-sixtieth — finer than
/// the wire codes a position to, which means the passes past it cannot move an
/// element anywhere the payload could say. Zero declines the search.
pub const FOLD_SEARCHES: std::ops::RangeInclusive<usize> = 0..=8;

/// The depths `--fold-depth` will accept, and why they end where they do.
///
/// Twenty-four is the codec's own domain: there is nothing finer to keep.
///
/// Seventeen is where the *format* stops being able to say what was rounded
/// away. A block declares its dead low bits in a field capped at
/// [`hz_mlp::format::MAX_OUTPUT_SHIFT`] — seven — so twenty-four less seven is
/// the coarsest grid whose zeroes the stream can still decline to write.
/// Rounding below that throws away signal and buys nothing: the eighth dead
/// bit is coded as a zero like any other.
pub const FOLD_DEPTHS: std::ops::RangeInclusive<u32> =
    (24 - hz_mlp::format::MAX_OUTPUT_SHIFT as u32)..=24;

/// The step those bits leave, and the top of the range that lands on it.
///
/// The ceiling matters: rounding a sample near full scale *up* would put it
/// outside the codec's domain, and clamping it back to `CEILING` would leave
/// one sample that is not on the step — which takes the whole block's dead
/// bits with it, for one sample in a hundred million.
const fn fold_step(bits: u32) -> f64 {
    (1u32 << (24 - bits)) as f64
}

fn programme(live: &[Live], parts: &[Part]) -> ObjectAudioMetadata {
    let lfe = matches!(parts.first(), Some(Part::Lfe { .. }));
    let objects = live
        .iter()
        .map(|entry| object_of(&parts[entry.part_index], &entry.state))
        .collect();

    ObjectAudioMetadata {
        lfe,
        blocks: vec![Block {
            // An access unit is forty samples and the offset counts
            // thirty-twos, so the only offset inside one is the first.
            offset: 0,
            ramp: ramp_of(live, parts),
            objects,
        }],
    }
}

/// What one element states, from the master's own keyframe.
///
/// The whole of an element's metadata for a stream that is not folding: where
/// the master put it, how loud it said it was, and how it asked to be
/// rendered. An overlay writes exactly this for the elements it keeps, which
/// is what makes its payload the master's rather than a regeneration of it —
/// unlike `hz_cluster::Clustering::to_metadata`, which states positions the
/// fold settled on and carries no size, snap or zone at all.
fn object_of(part: &Part, state: &Keyframe) -> Object {
    match part {
        Part::Lfe { .. } => Object {
            active: true,
            gain: Gain::Unity,
            render: None,
        },
        Part::Absent => Object {
            active: false,
            gain: Gain::Silent,
            render: None,
        },
        Part::Object { .. } => Object {
            active: true,
            gain: gain_of(state.gain),
            render: Some(Render {
                position: Position::from_master(state.position),
                size: size_of(state.spread),
                elevation: state.mode.elevation && state.position[2].abs() > f64::EPSILON,
                zones: state.mode.zones,
                screen: state.mode.screen,
                snap: state.mode.snap,
                ..Render::default()
            }),
        },
    }
}

/// What a decoder will actually apply for an element, in linear amplitude.
///
/// **Not the master's own number.** The syntax carries whole decibels, so an
/// element asking for −5.4 dB is written as −5 and a decoder applies −5. An
/// overlay divides a source's contribution by this gain so that the sum comes
/// out where it was asked for — see `hz_cluster::overlay` — and dividing by
/// the master's un-quantised figure would leave up to half a decibel of the
/// error the compensation exists to remove.
fn stated_gain(linear: f64) -> f64 {
    gain_of(linear)
        .decibels()
        .map_or(0.0, |db| 10f64.powf(f64::from(db) / 20.0))
}

/// The gain in the whole decibels the syntax carries.
fn gain_of(linear: f64) -> Gain {
    if linear <= 1e-5 {
        return Gain::Silent;
    }
    let db = 20.0 * linear.log10();
    if db.abs() < 0.5 {
        return Gain::Unity;
    }
    Gain::Decibels(db.round().clamp(-49.0, 15.0) as i8)
}

/// Object size, which the master carries as a width from 0 to 1.
fn size_of(spread: f64) -> Size {
    if spread <= 0.0 {
        return Size::Point;
    }
    Size::Uniform((spread * 31.0).round().clamp(1.0, 31.0) as u8)
}

/// How long this block takes to arrive.
///
/// One ramp covers the whole block, so the longest any element asked for wins:
/// arriving late is a slower move than the master wanted, arriving early is a
/// jump it did not.
fn ramp_of(live: &[Live], parts: &[Part]) -> Ramp {
    let longest = live
        .iter()
        .filter(|entry| matches!(parts[entry.part_index], Part::Object { .. }))
        .map(|entry| entry.state.ramp_samples)
        .max()
        .unwrap_or(0);
    if longest == 0 {
        Ramp::None
    } else {
        Ramp::Samples(longest.min(2047) as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 7.1 bed and one source after it, as a master being re-voiced brings
    /// them: the low frequency channel first because the payload's flag says
    /// it is, the bed elements after it, and the sources last.
    fn overlay_parts(sources: &[[f64; 3]]) -> Vec<Part> {
        let mut parts = vec![Part::Lfe { source_channel: 0 }];
        for (index, label) in ["L", "R", "C", "Lss", "Rss", "Lrs", "Rrs"]
            .iter()
            .enumerate()
        {
            let position = hz_render::fold::bed_position(label).expect("a bed channel");
            parts.push(Part::Object {
                source_channel: index + 1,
                keyframes: vec![Keyframe {
                    position,
                    mode: hz_cluster::class::BED,
                    ..Keyframe::default()
                }],
                bed: Some(position),
            });
        }
        for (index, position) in sources.iter().enumerate() {
            parts.push(Part::Object {
                source_channel: 8 + index,
                keyframes: vec![Keyframe {
                    position: *position,
                    ..Keyframe::default()
                }],
                bed: None,
            });
        }
        parts
    }

    fn overlay_fold(parts: &[Part], sources: usize, width: usize) -> Fold {
        Fold {
            shape: Shape::Overlaid(Box::new(Overlaid {
                overlay: hz_cluster::overlay::Overlay::new().expect("a reference"),
                sources,
                elements: parts.len() - sources,
                row: Vec::new(),
                live: Vec::new(),
                slotted: vec![false; sources],
                panned: Vec::new(),
                slots: Vec::new(),
                beds: beds_among(parts, parts.len() - sources),
                declared_beds: parts[..parts.len() - sources]
                    .iter()
                    .filter(|part| matches!(part, Part::Object { bed: Some(_), .. }))
                    .count(),
                carriers: Vec::new(),
                weights: Vec::new(),
                previous: vec![vec![0.0; width]; sources],
                fitting: Vec::new(),
                mix: Vec::new(),
                positions: Vec::new(),
                stated: Vec::new(),
                touched: Vec::new(),
                copy_from: Vec::new(),
                copied: 0,
                remixed: 0,
                stranded: 0,
                drift: 0.0,
                drifted: 0,
                worst_drift: 0.0,
                bounds: Bounds::default(),
                over_drift: 0,
                over_half_drift: 0,
                over_spread: 0,
                over_level: 0,
                over_cost: 0,
                run: vec![0; sources],
                worst_run: 0,
                stranded_run: Vec::new(),
                worst_stranded_run: 0,
                worst_level: 0.0,
                worst_cost: 0.0,
                worst_cost_where: String::new(),
                motion: hz_cluster::motion::Motion::new(
                    hz_cluster::motion::HEARING,
                    BLOCK_UNITS as f64 * unit_seconds(48_000),
                ),
                resultants: Vec::new(),
                energies: Vec::new(),
                resting: Vec::new(),
                audible: Vec::new(),
                was_audible: Vec::new(),
                whole: 0,
                voiced: 0,
                fell_back: 0,
                judged: blank_clustering(),
                report: None,
                reported_blocks: 0,
                reported_at: 0,
            })),
            renderers: hz_cluster::metric::delivery().expect("the presentations"),
            blocks: 0,
            mean: 0.0,
            worst: 0.0,
            clipped: 0,
            // Every bit the mix made: a test about what the arithmetic does
            // should not be reading the fold's rounding noise.
            step: fold_step(24),
            peak: 0.0,
            last_payload: Vec::new(),
            scene: Scene::weighed(48_000.0, Weighing::Flat).expect("a scene"),
            signals: Vec::new(),
            gains: Vec::new(),
            mixed: vec![Vec::new(); width],
            from: Vec::new(),
            floor: hz_cluster::floor::Floor::relative_only(),
            inaudible: 0,
            limiter: None,
        }
    }

    /// A block of distinct, deliberately low-bit-noisy samples, so that
    /// "unchanged" means unchanged and not merely "close".
    fn noisy_block(channels: usize, frames: usize) -> Vec<i32> {
        (0..channels * frames)
            .map(|n| {
                let n = n as i64;
                (((n * 2_654_435_761) % 4_000_000) - 2_000_000) as i32
            })
            .collect()
    }

    fn states_of(parts: &[Part]) -> Vec<(usize, Keyframe)> {
        parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let state = match part {
                    Part::Object { keyframes, .. } => {
                        keyframes.first().copied().unwrap_or_default()
                    }
                    _ => Keyframe::default(),
                };
                (index, state)
            })
            .collect()
    }

    /// **The point of the whole mode.** A centre source lands on the centre
    /// element with weight one, so every other element — the low frequency
    /// channel included — is copied out of the master's block sample for
    /// sample: not summed with a row of zeroes, and above all not rounded to
    /// the fold's depth.
    #[test]
    fn an_element_no_source_reaches_is_copied_bit_for_bit() {
        let parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 256;
        let block = noisy_block(channels, frames);
        let states = states_of(&parts);
        let mut fold = overlay_fold(&parts, 1, elements);
        let mut out = vec![0i32; frames * elements];

        fold_span(
            &mut fold,
            std::path::Path::new("a master"),
            &states,
            &parts,
            &block,
            channels,
            frames,
            Some(0),
            elements,
            &mut out,
        )
        .expect("the span folds");

        let centre = 3usize; // the LFE, then L, R, C
        for element in 0..elements {
            let same = (0..frames).all(|sample| {
                out[sample * elements + element] == block[sample * channels + element]
            });
            if element == centre {
                assert!(!same, "the element the source landed on was not mixed");
            } else {
                assert!(same, "element {element} was not copied through unchanged");
            }
        }
        assert_eq!(fold.clipped, 0);
    }

    /// An element a decoder will turn down by 6 dB has the source's
    /// contribution pushed up by 6 dB before it is added, so that what comes
    /// out of the decoder is the element as it was plus the source at the
    /// level it asked for. Without the compensation a dub would follow every
    /// gain the original mix wrote for something else.
    #[test]
    fn a_sources_share_is_divided_by_the_gain_the_element_states() {
        let mut quiet = overlay_parts(&[[0.0, 1.0, 0.0]]);
        // The centre element, at half amplitude.
        if let Part::Object { keyframes, .. } = &mut quiet[3] {
            keyframes[0].gain = 0.5;
        }
        let loud = overlay_parts(&[[0.0, 1.0, 0.0]]);

        let elements = loud.len() - 1;
        let channels = loud.len();
        let frames = 256;
        let block = noisy_block(channels, frames);

        let run = |parts: &[Part]| -> Vec<i32> {
            let states = states_of(parts);
            let mut fold = overlay_fold(parts, 1, elements);
            let mut out = vec![0i32; frames * elements];
            fold_span(
                &mut fold,
                std::path::Path::new("a master"),
                &states,
                parts,
                &block,
                channels,
                frames,
                Some(0),
                elements,
                &mut out,
            )
            .expect("the span folds");
            out
        };
        let at_unity = run(&loud);
        let at_half = run(&quiet);

        // The element's own audio is the same in both; what differs is the
        // source added to it, which has to be twice as large where the
        // decoder will halve it.
        let centre = 3usize;
        let mut ratio = 0.0f64;
        let mut counted = 0u32;
        for sample in 0..frames {
            let own = f64::from(block[sample * channels + centre]);
            let unity = f64::from(at_unity[sample * elements + centre]) - own;
            let half = f64::from(at_half[sample * elements + centre]) - own;
            if unity.abs() > 1000.0 {
                ratio += half / unity;
                counted += 1;
            }
        }
        assert!(counted > 100, "the source was audible in {counted} samples");
        let ratio = ratio / f64::from(counted);
        assert!(
            (ratio - 2.0).abs() < 0.01,
            "the source went in at {ratio:.4} of its level, not twice it"
        );
    }

    /// An overlay's presentations fold each element at the gain the payload
    /// states for it, as a plain encode's do: a decoder rendering the
    /// elements turns the centre down by 6 dB, so a 2.0, 5.1 or 7.1 that
    /// folded it at unity would play it 6 dB louder than the full programme.
    /// Every master seen states 0 dB throughout, which is how this went
    /// unwired without anything sounding wrong.
    #[test]
    fn an_overlays_folds_take_the_gain_each_element_states() {
        // A 7.1 bed and one overhead object before the source: nine elements,
        // so that every fold is narrower than the programme and is written.
        let wider = || {
            let mut parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
            if let Some(Part::Object { source_channel, .. }) = parts.last_mut() {
                *source_channel += 1;
            }
            parts.insert(
                8,
                Part::Object {
                    source_channel: 8,
                    keyframes: vec![Keyframe {
                        position: [0.0, 0.0, 1.0],
                        ..Keyframe::default()
                    }],
                    bed: None,
                },
            );
            parts
        };
        let mut quiet = wider();
        if let Part::Object { keyframes, .. } = &mut quiet[3] {
            keyframes[0].gain = 0.5;
        }
        let loud = wider();

        let elements = loud.len() - 1;
        let channels = loud.len();
        let frames = 256;
        let block = noisy_block(channels, frames);

        let folds = |parts: &[Part]| -> Vec<Presentation> {
            let states = states_of(parts);
            let mut fold = overlay_fold(parts, 1, elements);
            let mut out = vec![0i32; frames * elements];
            fold_span(
                &mut fold,
                std::path::Path::new("a master"),
                &states,
                parts,
                &block,
                channels,
                frames,
                Some(0),
                elements,
                &mut out,
            )
            .expect("the span folds");
            let positions = fold.positions().expect("an overlay has positions");
            let stated = fold.stated().expect("and gains");
            assert_eq!(stated.len(), positions.len(), "one gain an element");
            presentations_of(positions, fold.stated(), true).expect("wider than every fold")
        };
        let at_unity = folds(&loud);
        let at_half = folds(&quiet);

        let centre = 3usize; // the LFE, then L, R, C
        let six_down = stated_gain(0.5);
        let mut reached = 0;
        for (unity, half) in at_unity.iter().zip(&at_half).take(3) {
            for (row_unity, row_half) in unity.rows.iter().zip(&half.rows) {
                for element in 0..row_unity.len() {
                    let wanted = if element == centre {
                        row_unity[element] * six_down
                    } else {
                        row_unity[element]
                    };
                    assert!(
                        (row_half[element] - wanted).abs() < 1e-12,
                        "{} channels, element {element}: {} against {wanted}",
                        unity.channels,
                        row_half[element]
                    );
                }
                if row_unity[centre] > 0.0 {
                    reached += 1;
                }
            }
        }
        assert!(reached >= 3, "the centre reached a channel of every fold");
    }

    /// With folds, a narrow presentation's word answers to the fold's level
    /// through the curve measured for it — the stereo curve for two channels,
    /// the wide one above — and not to the leading elements. Here the leading
    /// two elements are silent and the rest loud, so reading them would ask
    /// for the wide curve's boost where the fold is loud enough to cut.
    #[test]
    fn a_folds_word_answers_to_the_fold_through_its_own_curve() {
        const ELEMENTS: usize = 12;
        const FRAMES: usize = 40;
        let rate = 48_000.0 / FRAMES as f64;
        // Every element into both channels of the stereo, and each of six
        // channels taking two elements.
        let stereo = Presentation {
            channels: 2,
            rows: vec![vec![0.3; ELEMENTS]; 2],
        };
        let surround = Presentation {
            channels: 6,
            rows: (0..6)
                .map(|channel| {
                    let mut row = vec![0.0; ELEMENTS];
                    row[2 * channel] = 1.0;
                    row[2 * channel + 1] = 1.0;
                    row
                })
                .collect(),
        };
        let identity = |channels: usize| Presentation {
            channels,
            rows: (0..channels)
                .map(|channel| {
                    let mut row = vec![0.0; ELEMENTS];
                    row[channel] = 1.0;
                    row
                })
                .collect(),
        };
        let presentations = [stereo, surround, identity(8), identity(ELEMENTS)];
        let widths = [2, 6, 8, ELEMENTS];

        // The leading two elements silent, the rest a constant at a tenth of
        // full scale.
        let level = 0.1 * FULL_SCALE;
        let mut unit = vec![0i32; FRAMES * ELEMENTS];
        for frame in unit.chunks_exact_mut(ELEMENTS) {
            for sample in &mut frame[2..] {
                *sample = level as i32;
            }
        }
        let sample = f64::from(level as i32) / FULL_SCALE;

        let settle = |loudness: &mut Loudness| {
            let mut gains = [None; hz_mlp::format::MAX_SUBSTREAMS];
            for _ in 0..2_000 {
                loudness.measure(&unit, ELEMENTS, FRAMES, &mut gains);
            }
            gains
        };
        // What a detector fed a constant power settles at, through a curve.
        let settled = |power: f64, curve: hz_analysis::drc::Measured| {
            let mut detector = hz_analysis::drc::Level::new(rate);
            let mut level = 0.0;
            for _ in 0..2_000 {
                level = detector.feed(power);
            }
            curve.gain_db(level)
        };

        let mut folded = Loudness::new(&widths, rate);
        folded.fold(&presentations);
        let gains = settle(&mut folded);
        // Stereo: each channel is 0.3 of ten loud elements.
        let stereo_power = 2.0 * (0.3 * 10.0 * sample).powi(2);
        let wanted = settled(stereo_power, hz_analysis::drc::STEREO);
        assert!(
            (gains[0].unwrap() - wanted).abs() < 1e-9,
            "{:?} against {wanted}",
            gains[0]
        );
        assert!(wanted < 0.0, "a fold this loud is cut, not boosted");
        // Six channels: the first silent, five carrying two loud elements.
        let surround_power = 5.0 * (2.0 * sample).powi(2);
        let wanted = settled(surround_power, hz_analysis::drc::WIDE);
        assert!((gains[1].unwrap() - wanted).abs() < 1e-9);

        // Without folds, the leading elements and the wide curve, as a stream
        // whose narrow presentations are copies of them plays.
        let mut copied = Loudness::new(&widths, rate);
        let gains = settle(&mut copied);
        assert!((gains[0].unwrap() - settled(0.0, hz_analysis::drc::WIDE)).abs() < 1e-9);
        assert!(gains[0].unwrap() > 0.0, "two silent elements are boosted");
    }

    /// **The other half of the point.** What an overlay writes for the
    /// elements it kept is byte for byte what a plain encode of those elements
    /// alone would have written — the master's own metadata, not a
    /// regeneration of it. A clustering cannot do this: it states the
    /// positions its own fold settled on and carries no size, snap or zone at
    /// all.
    #[test]
    fn the_elements_state_exactly_what_the_master_said_of_them() {
        let parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 256;
        let block = noisy_block(channels, frames);
        let states = states_of(&parts);
        let mut fold = overlay_fold(&parts, 1, elements);
        let mut out = vec![0i32; frames * elements];

        fold_span(
            &mut fold,
            std::path::Path::new("a master"),
            &states,
            &parts,
            &block,
            channels,
            frames,
            Some(0),
            elements,
            &mut out,
        )
        .expect("the span folds");
        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        let mut scratch = ObjectAudioMetadata {
            lfe: false,
            blocks: Vec::new(),
        };
        let written = overlay_payload(
            overlaid,
            std::path::Path::new("a master"),
            &parts,
            &states,
            &mut scratch,
        )
        .expect("a payload");

        // The same elements through the path that does no folding at all,
        // which is the master's metadata by definition.
        let kept = &parts[..elements];
        let live: Vec<Live> = kept
            .iter()
            .enumerate()
            .map(|(part_index, part)| Live {
                part_index,
                next: 0,
                state: match part {
                    Part::Object { keyframes, .. } => {
                        keyframes.first().copied().unwrap_or_default()
                    }
                    _ => Keyframe::default(),
                },
            })
            .collect();
        let plain = programme(&live, kept).write().expect("metadata");
        assert_eq!(written, plain, "the payload is not the master's own");
    }

    /// Run a sequence of blocks through one fold, each with its own source
    /// gain, and give back what each block wrote. Every other test here folds
    /// a single block, which is exactly how the fade below went unseen: it
    /// only exists at the seam between two.
    fn overlay_blocks(
        parts: &[Part],
        sources: usize,
        gains: &[f64],
        frames: usize,
    ) -> (Vec<Vec<i32>>, Vec<i32>, Fold) {
        let elements = parts.len() - sources;
        let channels = parts.len();
        let block = noisy_block(channels, frames);
        let mut fold = overlay_fold(parts, sources, elements);
        let mut written = Vec::new();
        for gain in gains {
            let mut states = states_of(parts);
            for state in states.iter_mut().skip(elements) {
                state.1.gain = *gain;
            }
            let mut out = vec![0i32; frames * elements];
            fold_span(
                &mut fold,
                std::path::Path::new("a master"),
                &states,
                parts,
                &block,
                channels,
                frames,
                Some(0),
                elements,
                &mut out,
            )
            .expect("the span folds");
            written.push(out);
        }
        (written, block, fold)
    }

    /// **An element's own audio is never ramped.** A copied element has no row
    /// in the mix matrix, so the block after a source first reaches it the
    /// previous matrix holds a nought on its diagonal and this one a one.
    /// Read as a weight, that fades the programme's own audio up from silence
    /// across the block — a hole and a click in material nobody touched, at
    /// every change of carrier set.
    ///
    /// Two blocks: the source silent in the first, so the centre is copied,
    /// and audible in the second, so it is mixed. The centre's own samples
    /// must be there at full scale from the very first sample of block two.
    #[test]
    fn a_kept_element_does_not_fade_in_when_it_becomes_a_carrier() {
        let parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 1280;
        let centre = 3usize;
        let (written, block, _) = overlay_blocks(&parts, 1, &[0.0, 1.0], frames);

        // Block one: the source is silent, so every element is copied.
        for element in 0..elements {
            assert!(
                (0..frames).all(|sample| written[0][sample * elements + element]
                    == block[sample * channels + element]),
                "element {element} was not copied while the source was silent"
            );
        }

        // Block two: the centre carries the source. Its own audio is whole
        // from the first sample, and so is the source — the weights did not
        // change, so there is nothing to crossfade. A voice fading up over
        // 27 ms at every phrase onset was what zeroing them cost.
        let source = channels - 1;
        let share = |sample: usize| -> f64 {
            let own = f64::from(block[sample * channels + centre]);
            let got = f64::from(written[1][sample * elements + centre]);
            (got - own) / f64::from(block[sample * channels + source])
        };
        let at_first = share(0);
        assert!(
            at_first.abs() > 0.05,
            "the source arrived at {at_first} of itself in the first sample, which is a fade"
        );
        for sample in [1usize, 2, frames / 4, frames / 2, frames - 1] {
            let now = share(sample);
            assert!(
                (now - at_first).abs() < 1e-3,
                "the weight moved across the block: {at_first} at the first sample and {now} \
                 at {sample}, so the source is being ramped when nothing asked it to be"
            );
        }
    }

    /// **A block costs no allocation once the shapes have settled.** A dub is
    /// forty blocks a second for the length of a feature, so a buffer that
    /// grows per block is a buffer that grows a couple of million times.
    ///
    /// Counted as capacity rather than through a global allocator: what is
    /// being asserted is that the fold's own buffers are *reused*, and a
    /// capacity that never changes after the first block is exactly that
    /// statement, without a shim that every other test in the workspace would
    /// then have to live with.
    #[test]
    fn a_block_grows_no_buffer_after_the_first() {
        let parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 1280;
        let block = noisy_block(channels, frames);
        let mut fold = overlay_fold(&parts, 1, elements);
        let mut out = vec![0i32; frames * elements];

        // Vary the source's level so the block alternates between mixing and
        // copying, which is the path that used to reallocate.
        let mut sizes: Vec<Vec<usize>> = Vec::new();
        for turn in 0..24 {
            let mut states = states_of(&parts);
            for state in states.iter_mut().skip(elements) {
                state.1.gain = if turn % 4 < 2 { 1.0 } else { 0.0 };
            }
            fold_span(
                &mut fold,
                std::path::Path::new("a master"),
                &states,
                &parts,
                &block,
                channels,
                frames,
                Some(0),
                elements,
                &mut out,
            )
            .expect("the span folds");
            let Shape::Overlaid(overlaid) = &fold.shape else {
                panic!("an overlay")
            };
            sizes.push(vec![
                overlaid.row.capacity(),
                overlaid.carriers.capacity(),
                overlaid.panned.capacity(),
                overlaid.judged.weights.capacity(),
                overlaid.copy_from.capacity(),
                overlaid.touched.capacity(),
                overlaid.live.capacity(),
                overlaid.resultants.capacity(),
                overlaid.energies.capacity(),
                fold.from.capacity(),
                fold.signals.capacity(),
                fold.gains.capacity(),
            ]);
        }

        // Whatever the first few blocks had to reach, nothing grows after
        // them: the buffers are refilled, not rebuilt.
        let settled = &sizes[3];
        for (turn, sizes) in sizes.iter().enumerate().skip(4) {
            assert_eq!(
                sizes, settled,
                "a buffer grew on block {turn}: {sizes:?} against {settled:?}"
            );
        }
    }

    /// **Silence must not fade the voice.** A source audible, then silent for
    /// a stretch, then audible again — which is every phrase of a dub.
    ///
    /// Through the silence its carriers are copied out of the master whole,
    /// which is the fast path; and when it comes back it comes back at full
    /// weight in the first sample, because its weights were never touched.
    /// Zeroing them while silent made the fast path work and put a 27 ms fade
    /// on every onset. What decides an element is copied is now whether a
    /// source reaching it was *audible*, not what its weights say.
    #[test]
    fn a_voice_returning_after_a_pause_is_not_faded_in() {
        let parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 1280;
        let centre = 3usize;
        // Ten blocks of voice, ten of silence, ten of voice: a phrase, a
        // pause, a phrase.
        let mut levels = vec![1.0; 10];
        levels.extend(std::iter::repeat_n(0.0, 10));
        levels.extend(std::iter::repeat_n(1.0, 10));
        let (written, block, _) = overlay_blocks(&parts, 1, &levels, frames);

        // The blocks inside the silence — all but the first, which still
        // carries the previous block's mix — are copied, every element of
        // them, sample for sample.
        for silent in 11..=19 {
            for element in 0..elements {
                assert!(
                    (0..frames).all(|sample| written[silent][sample * elements + element]
                        == block[sample * channels + element]),
                    "block {silent} element {element} was not copied through the silence"
                );
            }
        }

        // And the voice comes back whole in the first sample of block twenty,
        // at exactly the weight it had before the pause: the pause changed
        // nothing about where the voice goes, so the fit's answer — and the
        // hysteresis it was holding — are the ones it left with.
        let source = channels - 1;
        let share = |turn: usize, sample: usize| -> f64 {
            let own = f64::from(block[sample * channels + centre]);
            let got = f64::from(written[turn][sample * elements + centre]);
            (got - own) / f64::from(block[sample * channels + source])
        };
        let before = share(9, frames - 1);
        let at_first = share(20, 0);
        let settled = share(20, frames - 1);
        assert!(
            (at_first - before).abs() < 1e-3,
            "the voice came back at {at_first} of itself against {before} before the pause, \
             which is a fade at the onset"
        );
        assert!(
            (at_first - settled).abs() < 1e-3,
            "the weight walked across the block after the pause: {at_first} then {settled}"
        );
    }

    /// **The silence of a dub is not a gain of nought.** Every other silence
    /// test here mutes the source by its gain, and `hz_cluster::mix::mix`
    /// skips a row whose gain is nought, so the block that stops carrying it
    /// comes out bit-exact. A separated voice track is not like that: its
    /// gain is one and its "silence" is a residual a few steps above zero,
    /// under the audibility floor but not nothing. That block is mixed —
    /// `w · residual` is added and the element is rounded to `--fold-depth`
    /// — and only the block after it is copied. So the bound on what a pause
    /// costs is: **one block of `w · residual`, then nothing**, and this pins
    /// both halves at the depth the dub is encoded at.
    #[test]
    fn a_residual_under_the_floor_costs_one_mixed_block_and_then_nothing() {
        let parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 1280;
        let centre = 3usize;
        let source = channels - 1;
        let loud = noisy_block(channels, frames);
        // The same programme with the source down to a residual: a few
        // steps either side of nought, some hundred decibels under the
        // elements around it, which is under the floor.
        let mut faint = loud.clone();
        for sample in 0..frames {
            faint[sample * channels + source] = loud[sample * channels + source].rem_euclid(7) - 3;
        }
        let residual = faint
            .iter()
            .skip(source)
            .step_by(channels)
            .map(|v| v.abs())
            .max()
            .unwrap_or(0);
        assert!(residual > 0 && residual <= 3, "a residual, not silence");

        let mut fold = overlay_fold(&parts, 1, elements);
        // The room's own floor, as an encode has it: the other tests here
        // decline it because they silence by gain, and this one is about a
        // source that is quiet rather than off.
        fold.floor = hz_cluster::floor::Floor::for_dialnorm(-31.0);
        let states = states_of(&parts);
        let mut written = Vec::new();
        for block in [&loud, &faint, &faint] {
            let mut out = vec![0i32; frames * elements];
            fold_span(
                &mut fold,
                std::path::Path::new("a master"),
                &states,
                &parts,
                block,
                channels,
                frames,
                Some(0),
                elements,
                &mut out,
            )
            .expect("the span folds");
            written.push(out);
        }

        // Block one: the source is under the floor but the block before was
        // not, so the centre is mixed — its own samples plus the weight times
        // the residual, which is within a few steps of its own samples and
        // not, in general, exactly them.
        let apart = |turn: usize| -> i64 {
            (0..frames)
                .map(|sample| {
                    i64::from(written[turn][sample * elements + centre])
                        - i64::from(faint[sample * channels + centre])
                })
                .map(i64::abs)
                .max()
                .unwrap_or(0)
        };
        let first = apart(1);
        assert!(
            first <= i64::from(residual) + 1,
            "the first faint block strayed {first} steps, more than the residual it carried"
        );
        // Block two: the source was under the floor in this block and the
        // one before, so the centre is copied — bit-exact, residual and all.
        assert_eq!(apart(2), 0, "the second faint block was not copied whole");
    }

    /// The mirror: an element that stops being a carrier is copied whole from
    /// its first sample, because the weight ramping to nothing belongs to the
    /// last block that mixed it and not to the first that copies it.
    #[test]
    fn a_kept_element_is_whole_again_from_the_first_sample_it_is_copied() {
        let parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 1280;
        let centre = 3usize;
        // Audible, then silent for two blocks: the first silent block still
        // mixes (the weight ramps down across it), the second copies.
        let (written, block, _) = overlay_blocks(&parts, 1, &[1.0, 0.0, 0.0], frames);

        assert!(
            (0..frames).all(|sample| written[2][sample * elements + centre]
                == block[sample * channels + centre]),
            "the centre was not copied whole once the source had gone"
        );
        // And the block between — mixed, because the source was audible in the
        // one before it — is **bit-exact** too, every sample of it. There is
        // no residual to tolerate: a source under the floor contributes
        // `w · 0` whatever its weights are, and `hz_cluster::mix::mix` skips a
        // row whose gain is nought outright. This once allowed four steps of
        // slack; it does not need any.
        assert!(
            (0..frames).all(|sample| written[1][sample * elements + centre]
                == block[sample * channels + centre]),
            "the block that stopped carrying the source is not its own audio exactly"
        );
    }

    /// **A pause is not movement.** A static source, audible then silent then
    /// audible again, over carriers that never move: the ruler must say
    /// nothing wobbled.
    ///
    /// It used to say a great deal. `Motion` judges a window by the loudest
    /// the source got anywhere in it, so a window straddling a phrase boundary
    /// was judged — and the silent blocks pushed the direction the source
    /// *asked* for while the voiced ones pushed where its carriers actually
    /// put it, which is an out-and-back of twice the drift at every onset and
    /// release. The 5 % refusal was reachable with nothing moving at all.
    #[test]
    fn a_pause_is_not_movement() {
        let parts = overlay_parts(&[[-1.0, -0.4, 0.0]]);
        let frames = 1280;
        // Ten blocks voiced, ten silent, ten voiced. The carriers are the bed
        // and never move, so any wobble the ruler finds is invented.
        let mut gains = Vec::new();
        gains.extend(std::iter::repeat_n(1.0, 10));
        gains.extend(std::iter::repeat_n(0.0, 10));
        gains.extend(std::iter::repeat_n(1.0, 10));
        let (_, _, fold) = overlay_blocks(&parts, 1, &gains, frames);

        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        assert!(
            overlaid.motion.windows() > 0,
            "no window was ever judged, so this test proves nothing"
        );
        assert_eq!(
            overlaid.motion.wobbling(),
            0.0,
            "a pause was read as movement over {} windows",
            overlaid.motion.windows()
        );
        assert_eq!(overlaid.motion.invented(), 0.0);
    }

    /// And a pause ends a **run**: two stretches of drift either side of one
    /// are two runs, not a single long one. A run is what refuses on its own,
    /// whatever the share, so joining two across a silence would refuse a
    /// programme for a fault it does not have.
    #[test]
    fn a_pause_ends_a_run_of_drift() {
        // Every element down the left side and a source at hard right: the
        // fit cannot place it, so every voiced block is past the drift bound.
        let mut parts = vec![Part::Lfe { source_channel: 0 }];
        for (index, label) in ["L", "Lss", "Lrs"].iter().enumerate() {
            let position = hz_render::fold::bed_position(label).expect("a bed channel");
            parts.push(Part::Object {
                source_channel: index + 1,
                keyframes: vec![Keyframe {
                    position,
                    mode: hz_cluster::class::BED,
                    ..Keyframe::default()
                }],
                bed: Some(position),
            });
        }
        parts.push(Part::Object {
            source_channel: 4,
            keyframes: vec![Keyframe {
                position: [1.0, 0.0, 0.0],
                ..Keyframe::default()
            }],
            bed: None,
        });

        // Three blocks adrift, one silent, three adrift: the longest run is
        // three, not six. The fallback is off so the drift stays.
        let gains = [1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0];
        let elements = parts.len() - 1;
        let mut fold = overlay_fold(&parts, 1, elements);
        if let Shape::Overlaid(overlaid) = &mut fold.shape {
            overlaid.bounds.fallback = false;
        }
        let frames = 1280;
        let block = noisy_block(parts.len(), frames);
        for gain in gains {
            let mut states = states_of(&parts);
            for state in states.iter_mut().skip(elements) {
                state.1.gain = gain;
            }
            let mut out = vec![0i32; frames * elements];
            fold_span(
                &mut fold,
                std::path::Path::new("a master"),
                &states,
                &parts,
                &block,
                parts.len(),
                frames,
                Some(0),
                elements,
                &mut out,
            )
            .expect("the span folds");
        }

        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        assert!(overlaid.over_drift >= 6, "the source was not adrift at all");
        assert_eq!(
            overlaid.worst_run, 3,
            "the silence did not end the run: it reached {}",
            overlaid.worst_run
        );
    }

    /// **A dub's master declares no bed.** A programme decoded back out of a
    /// delivery stream carries its 7.1 bed as ordinary objects that never
    /// leave their speakers' places, and declares only the low frequency
    /// channel — so every mechanism that asks "is this a bed?" was inert on
    /// the one input this mode exists for. They are worked out instead.
    #[test]
    fn a_bed_that_the_master_never_declared_is_still_a_bed() {
        // The same seven places, as plain objects with one static keyframe
        // each: what `truehdd decode` hands back.
        let mut parts = vec![Part::Lfe { source_channel: 0 }];
        for (index, label) in ["L", "R", "C", "Lss", "Rss", "Lrs", "Rrs"]
            .iter()
            .enumerate()
        {
            parts.push(Part::Object {
                source_channel: index + 1,
                keyframes: vec![Keyframe {
                    position: hz_render::fold::bed_position(label).expect("a place"),
                    ..Keyframe::default()
                }],
                bed: None,
            });
        }
        // An object that is still but nowhere near a speaker: not a bed.
        parts.push(Part::Object {
            source_channel: 8,
            keyframes: vec![Keyframe {
                position: [0.4, 0.3, 0.2],
                ..Keyframe::default()
            }],
            bed: None,
        });
        // And one that visits a speaker's place but moves: not a bed either.
        parts.push(Part::Object {
            source_channel: 9,
            keyframes: vec![
                Keyframe {
                    position: [0.0, 1.0, 0.0],
                    ..Keyframe::default()
                },
                Keyframe {
                    sample_pos: 4800,
                    position: [0.0, -1.0, 0.0],
                    ..Keyframe::default()
                },
            ],
            bed: None,
        });

        let beds = beds_among(&parts, parts.len());
        assert!(!beds[0], "the low frequency channel is not panned onto");
        for element in 1..=7 {
            assert!(
                beds[element],
                "element {element} is at a speaker's place and still"
            );
        }
        assert!(!beds[8], "a still object away from a speaker is not a bed");
        assert!(
            !beds[9],
            "an object that moves is not a bed, wherever it starts"
        );
    }

    /// And the carriers say so, so that the beds-first preference and the
    /// fallback have something to work with on a real dub.
    #[test]
    fn the_carriers_of_an_undeclared_bed_are_marked_as_beds() {
        let mut parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        // Strip every declaration: the positions stay, the claim goes.
        for part in parts.iter_mut() {
            if let Part::Object { bed, .. } = part {
                *bed = None;
            }
        }
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 256;
        let block = noisy_block(channels, frames);
        let states = states_of(&parts);
        let mut fold = overlay_fold(&parts, 1, elements);
        let mut out = vec![0i32; frames * elements];
        fold_span(
            &mut fold,
            std::path::Path::new("a master"),
            &states,
            &parts,
            &block,
            channels,
            frames,
            Some(0),
            elements,
            &mut out,
        )
        .expect("the span folds");

        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        assert_eq!(overlaid.declared_beds, 0, "the master declares none");
        assert_eq!(
            overlaid.carriers.iter().filter(|c| c.bed).count(),
            7,
            "the seven bed channels were not recognised without a declaration"
        );
    }

    /// And they are used as beds, which is the half that matters. A source
    /// between the left side and the left rear, on a master that declares no
    /// bed at all and has a moving object sitting on the source, goes to the
    /// two beds either side of it and to nothing else.
    ///
    /// This is the case the whole of `beds_among` exists for: before it, the
    /// preference had no bed to prefer, and the object took the voice.
    #[test]
    fn an_undeclared_bed_still_wins_a_source_from_a_moving_object() {
        let mut parts = overlay_parts(&[]);
        // The object that would otherwise take it, sitting exactly on the
        // source, and then the source.
        let between = [-1.0, -0.5, 0.0];
        parts.push(Part::Object {
            source_channel: 8,
            keyframes: vec![
                Keyframe {
                    position: between,
                    ..Keyframe::default()
                },
                // It moves, so it is not mistaken for a bed itself.
                Keyframe {
                    sample_pos: 4800,
                    position: [0.9, 0.9, 0.0],
                    ..Keyframe::default()
                },
            ],
            bed: None,
        });
        parts.push(Part::Object {
            source_channel: 9,
            keyframes: vec![Keyframe {
                position: between,
                ..Keyframe::default()
            }],
            bed: None,
        });
        // Every declaration stripped: this is the shape a decoded master has.
        for part in parts.iter_mut() {
            if let Part::Object { bed, .. } = part {
                *bed = None;
            }
        }

        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 256;
        let block = noisy_block(channels, frames);
        let states = states_of(&parts);
        let mut fold = overlay_fold(&parts, 1, elements);
        let mut out = vec![0i32; frames * elements];
        fold_span(
            &mut fold,
            std::path::Path::new("a master"),
            &states,
            &parts,
            &block,
            channels,
            frames,
            Some(0),
            elements,
            &mut out,
        )
        .expect("the span folds");

        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        let moving = elements - 1;
        assert!(
            overlaid.previous[0][moving] < 1e-9,
            "the moving object took {} of a source the beds reach",
            overlaid.previous[0][moving]
        );
        let carried: Vec<usize> = (0..elements)
            .filter(|element| overlaid.previous[0][*element] != 0.0)
            .collect();
        assert!(
            !carried.is_empty() && carried.len() <= 2,
            "the source went to {} elements: {carried:?}",
            carried.len()
        );
        for element in &carried {
            assert!(
                overlaid.beds[*element],
                "element {element} carried the source and is not a bed"
            );
        }
    }

    /// A source that took a spare element of its own is **not folded** — it
    /// *is* an element, carried whole and rendered from its own place — so
    /// judging its unused pan let something that costs nothing by
    /// construction push the guards around. Fourteen elements and three
    /// sources: two take slots, one is panned, and only the panned one is
    /// measured.
    #[test]
    fn a_source_with_an_element_of_its_own_is_not_judged_as_though_it_were_folded() {
        let mut parts = overlay_parts(&[]);
        for index in 0..6 {
            parts.push(Part::Object {
                source_channel: 8 + index,
                keyframes: vec![Keyframe {
                    position: [0.5, -0.5, 0.5],
                    ..Keyframe::default()
                }],
                bed: None,
            });
        }
        let element_parts = parts.len();
        assert_eq!(element_parts, 14);
        // A rear source, then a *high* centre and the left: the two nearest
        // the front take the two spare elements and the rear is panned. The
        // rear is at a place the bed reaches, so it costs nothing; the raised
        // centre — half way up, still nearer the front than the left is, so
        // it keeps its slot — is at a place no floor bed can reach, so if its unused pan
        // were still being judged the cost guard would trip on it. That is
        // what makes this test able to tell "not judged" from "judged and
        // cheap": the slotted source is the one that would be expensive.
        for (index, position) in [[-1.0, -1.0, 0.0], [0.0, 1.0, 0.5], [-0.577, 1.0, 0.0]]
            .into_iter()
            .enumerate()
        {
            parts.push(Part::Object {
                source_channel: 14 + index,
                keyframes: vec![Keyframe {
                    position,
                    ..Keyframe::default()
                }],
                bed: None,
            });
        }
        let slots = spare_slots(&parts, element_parts, 3, true);
        assert_eq!(
            slots,
            vec![1, 2],
            "the two nearest the front take the slots"
        );

        let elements = element_parts + slots.len();
        let channels = parts.len();
        let frames = 256;
        let block = noisy_block(channels, frames);
        let states = states_of(&parts);
        let mut fold = overlay_fold(&parts, 3, elements);
        if let Shape::Overlaid(overlaid) = &mut fold.shape {
            overlaid.elements = element_parts;
            overlaid.slotted = vec![false, true, true];
            overlaid.slots = slots;
            overlaid.previous = vec![vec![0.0; elements]; 3];
        }
        let mut out = vec![0i32; frames * elements];
        fold_span(
            &mut fold,
            std::path::Path::new("a master"),
            &states,
            &parts,
            &block,
            channels,
            frames,
            Some(0),
            elements,
            &mut out,
        )
        .expect("the span folds");

        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        // Only the panned source is judged.
        assert_eq!(overlaid.panned.len(), 1, "a slotted source was judged");
        assert_eq!(overlaid.judged.weights.len(), 1);
        // And the two that took elements move neither guard: a source carried
        // whole and rendered from its own place costs nothing by construction,
        // so letting its unused pan into the metric was letting a certainty
        // vote on a measurement. The panned one is at a speaker's place, and
        // the high one — which a pan over a floor bed would have priced far
        // past the bound — took a slot, so nothing is over either bound.
        assert_eq!(
            overlaid.over_cost, 0,
            "a slotted source pushed the cost guard"
        );
        assert_eq!(
            overlaid.over_level, 0,
            "a slotted source pushed the level guard"
        );

        // And the slotted sources' own elements are bit-exact copies of their
        // channels: an element of one's own is not a fold.
        for (slot, index) in overlaid.slots.iter().enumerate() {
            let element = element_parts + slot;
            let channel = 14 + index;
            assert!(
                (0..frames).all(|s| out[s * elements + element] == block[s * channels + channel]),
                "the element given to source {index} is not its own channel"
            );
        }
    }

    /// **A keyframe lands where the master put it.** An overlay states its
    /// elements once an access unit, not once a block, because their metadata
    /// is the master's and the master states it per unit. Read once a block,
    /// a keyframe at sample 48 000 landed at 47 360 — thirteen milliseconds
    /// early, on the nearest block boundary — and any that shared a block with
    /// another was dropped altogether.
    ///
    /// Two elements moving inside one block, at different samples: both
    /// arrive, and each at its own.
    #[test]
    fn an_element_is_restated_at_the_unit_the_master_moved_it_in() {
        let mut parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        // Two of the bed elements become movers, a unit apart and neither on
        // a block boundary.
        if let Part::Object { keyframes, bed, .. } = &mut parts[1] {
            *bed = None;
            keyframes.push(Keyframe {
                sample_pos: 200,
                position: [-0.5, 0.5, 0.0],
                ..Keyframe::default()
            });
        }
        if let Part::Object { keyframes, bed, .. } = &mut parts[2] {
            *bed = None;
            keyframes.push(Keyframe {
                sample_pos: 600,
                position: [0.5, 0.5, 0.0],
                ..Keyframe::default()
            });
        }

        // Walk the units the way the producer does, and ask what each states.
        let elements = parts.len() - 1;
        let frame = 40usize;
        let mut live: Vec<Live> = parts
            .iter()
            .enumerate()
            .map(|(part_index, part)| Live {
                part_index,
                next: 0,
                state: match part {
                    Part::Object { keyframes, .. } => {
                        keyframes.first().copied().unwrap_or_default()
                    }
                    _ => Keyframe::default(),
                },
            })
            .collect();
        let fold = overlay_fold(&parts, 1, elements);
        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        let mut scratch = ObjectAudioMetadata {
            lfe: false,
            blocks: Vec::new(),
        };
        let mut last: Vec<u8> = Vec::new();
        let mut restated = Vec::new();
        for unit in 0..BLOCK_UNITS {
            advance(&mut live, &parts, ((unit + 1) * frame) as u64 - 1);
            let states: Vec<(usize, Keyframe)> = live
                .iter()
                .map(|entry| (entry.part_index, entry.state))
                .collect();
            let payload = overlay_payload(
                overlaid,
                std::path::Path::new("a master"),
                &parts,
                &states,
                &mut scratch,
            )
            .expect("a payload");
            if payload != last {
                last = payload;
                restated.push(unit);
            }
        }

        // The first unit states the scene, then the two moves in the units
        // that hold them: sample 200 is unit 5, sample 600 is unit 15.
        assert_eq!(
            restated,
            vec![0, 5, 15],
            "the moves were not stated in the units that hold them"
        );
    }

    /// **And at the sample the master moved it at.** A unit is forty samples
    /// and a master moves things between its boundaries, so the payload says
    /// how far into the unit it takes effect: the moment the movers share,
    /// the one most of them share where they do not, the earliest on a tie,
    /// and the start of the unit where nothing moved.
    #[test]
    fn a_payload_takes_effect_at_the_sample_the_master_moved_at() {
        let mut parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let frame = 40usize;
        let at = |sample_pos: u64| Keyframe {
            sample_pos,
            ..Keyframe::default()
        };
        // Element one moves at sample 205, element two at 613, and element
        // three has no keyframes of its own: its default state at nought is
        // not a move.
        if let Part::Object { keyframes, .. } = &mut parts[1] {
            keyframes.push(at(205));
        }
        if let Part::Object { keyframes, .. } = &mut parts[2] {
            keyframes.push(at(613));
        }
        if let Part::Object { keyframes, .. } = &mut parts[3] {
            keyframes.clear();
        }
        let states = |one: u64, two: u64| [(1usize, at(one)), (2usize, at(two)), (3usize, at(0))];

        // Unit five holds sample 205: five samples in. Unit fifteen holds
        // 613: thirteen in. Between them nothing moved, and the unit starts.
        let offset = |unit: usize, states: &[(usize, Keyframe)]| {
            sample_offset_of(
                states.iter().map(|(part, state)| (*part, state)),
                &parts,
                (unit * frame) as u64,
                frame,
            )
        };
        assert_eq!(offset(5, &states(205, 0)), 5);
        assert_eq!(offset(15, &states(205, 613)), 13);
        assert_eq!(offset(10, &states(205, 0)), 0, "nothing moved in unit ten");
        assert_eq!(
            offset(0, &states(0, 0)),
            0,
            "the start, and a default state is not a move"
        );

        // Two movers in one unit at different samples: the one most share
        // wins, and the earliest on a tie.
        let tie = [(1usize, at(207)), (2usize, at(209))];
        assert_eq!(offset(5, &tie), 7);
        let three_to_two = [
            (1usize, at(221)),
            (2usize, at(221)),
            (1usize, at(221)),
            (2usize, at(200)),
            (1usize, at(200)),
        ];
        assert_eq!(offset(5, &three_to_two), 21);
    }

    /// **A programme stranded throughout says so as a hundred per cent.** Every
    /// element muted, so the source has no carrier in any block and no bed to
    /// fall back to; the share the guard refuses on is stranded blocks over
    /// audible blocks, and with the stranded blocks counted in the
    /// denominator — which they once were not — that is one, not
    /// `stranded / 1`.
    #[test]
    fn a_source_stranded_in_every_block_is_a_full_share() {
        let mut parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        let elements = parts.len() - 1;
        for part in parts.iter_mut().take(elements) {
            if let Part::Object { keyframes, bed, .. } = part {
                keyframes[0].gain = 0.0;
                *bed = None;
            }
        }
        let channels = parts.len();
        let frames = 256;
        let block = noisy_block(channels, frames);
        let states = states_of(&parts);
        let mut fold = overlay_fold(&parts, 1, elements);
        let mut out = vec![0i32; frames * elements];
        let blocks = 5;
        for _ in 0..blocks {
            fold_span(
                &mut fold,
                std::path::Path::new("a master"),
                &states,
                &parts,
                &block,
                channels,
                frames,
                Some(0),
                elements,
                &mut out,
            )
            .expect("the span folds");
        }
        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        assert!(
            overlaid.carriers.is_empty(),
            "a muted element was a carrier"
        );
        assert_eq!(overlaid.stranded, blocks, "every block stranded the source");
        assert_eq!(
            overlaid.drifted, blocks,
            "the stranded blocks are not in the denominator, so the share is not a share"
        );
    }

    /// An element a decoder will silence is not a place to put a voice: it
    /// leaves the candidate set for the block, so nothing is routed through
    /// it and nothing is divided by its gain. The source still lands
    /// somewhere, so nothing is stranded.
    #[test]
    fn a_muted_element_carries_nothing_and_strands_nothing() {
        let mut parts = overlay_parts(&[[0.0, 1.0, 0.0]]);
        // The centre, silenced — where a centre source would otherwise go.
        if let Part::Object { keyframes, .. } = &mut parts[3] {
            keyframes[0].gain = 0.0;
        }
        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 256;
        let block = noisy_block(channels, frames);
        let states = states_of(&parts);
        let mut fold = overlay_fold(&parts, 1, elements);
        let mut out = vec![0i32; frames * elements];
        fold_span(
            &mut fold,
            std::path::Path::new("a master"),
            &states,
            &parts,
            &block,
            channels,
            frames,
            Some(0),
            elements,
            &mut out,
        )
        .expect("the span folds");

        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        assert!(
            overlaid.carriers.iter().all(|carrier| carrier.element != 3),
            "the muted element is still a candidate"
        );
        // `previous` holds this block's answer once the span has swapped.
        assert_eq!(overlaid.previous[0][3], 0.0, "it was routed through anyway");
        assert_eq!(overlaid.stranded, 0, "the source went nowhere");
        assert!(
            overlaid.previous[0].iter().any(|weight| *weight != 0.0),
            "the source found no carrier at all"
        );
    }

    /// A source no element can reach is put on the nearest bed outright:
    /// audible and a little misplaced beats diffuse and wandering. The
    /// counter says how often that happened, because a fallback taken often
    /// is a programme the mode is wrong for.
    #[test]
    fn a_source_the_fit_cannot_place_is_put_on_the_nearest_bed() {
        // Every element down the left side, and a source at hard right: no
        // fit over these can put it where it asked to be.
        let mut parts = vec![Part::Lfe { source_channel: 0 }];
        for (index, label) in ["L", "Lss", "Lrs"].iter().enumerate() {
            let position = hz_render::fold::bed_position(label).expect("a bed channel");
            parts.push(Part::Object {
                source_channel: index + 1,
                keyframes: vec![Keyframe {
                    position,
                    mode: hz_cluster::class::BED,
                    ..Keyframe::default()
                }],
                bed: Some(position),
            });
        }
        parts.push(Part::Object {
            source_channel: 4,
            keyframes: vec![Keyframe {
                position: [1.0, 0.0, 0.0],
                ..Keyframe::default()
            }],
            bed: None,
        });

        let elements = parts.len() - 1;
        let channels = parts.len();
        let frames = 256;
        let block = noisy_block(channels, frames);
        let states = states_of(&parts);
        let mut fold = overlay_fold(&parts, 1, elements);
        let mut out = vec![0i32; frames * elements];
        fold_span(
            &mut fold,
            std::path::Path::new("a master"),
            &states,
            &parts,
            &block,
            channels,
            frames,
            Some(0),
            elements,
            &mut out,
        )
        .expect("the span folds");

        let Shape::Overlaid(overlaid) = &fold.shape else {
            panic!("an overlay")
        };
        assert_eq!(overlaid.fell_back, 1, "the fallback did not take it");
        assert_eq!(overlaid.stranded, 0, "and it is not stranded either");
        // One bed, outright: sharp rather than smeared over the three.
        let reached: Vec<f64> = overlaid.previous[0]
            .iter()
            .copied()
            .filter(|weight| *weight != 0.0)
            .collect();
        assert_eq!(reached.len(), 1, "it went to {} elements", reached.len());
        assert!((reached[0] - 1.0).abs() < 1e-9);
    }

    /// The spare elements go to the sources nearest the front centre, which is
    /// where a dub's dialogue is. Stated as an angle and not as a channel
    /// name, so a master that names its channels differently still gets it
    /// right.
    #[test]
    fn a_spare_element_goes_to_the_source_nearest_the_front() {
        // A master of fourteen elements — the bed of eight and six objects of
        // the mix — and three sources after them: a rear, the centre and a
        // left.
        let mut parts = overlay_parts(&[]);
        for index in 0..6 {
            parts.push(Part::Object {
                source_channel: 8 + index,
                keyframes: vec![Keyframe {
                    position: [0.5, -0.5, 0.5],
                    ..Keyframe::default()
                }],
                bed: None,
            });
        }
        let elements = parts.len();
        assert_eq!(elements, 14);
        for (index, position) in [[-1.0, -1.0, 0.0], [0.0, 1.0, 0.0], [-0.577, 1.0, 0.0]]
            .into_iter()
            .enumerate()
        {
            parts.push(Part::Object {
                source_channel: 14 + index,
                keyframes: vec![Keyframe {
                    position,
                    ..Keyframe::default()
                }],
                bed: None,
            });
        }

        // Two elements spare, so the centre takes one and the left the other;
        // the rear has no room and is panned.
        assert_eq!(spare_slots(&parts, elements, 3, true), vec![1, 2]);

        // With no room at all, nothing takes a slot and everything is panned.
        assert!(spare_slots(&parts, hz_mlp::format::MAX_CHANNELS, 3, true).is_empty());
    }

    /// A spare element is taken only when asked for. By default a re-voiced
    /// programme keeps the original's width: a fourteen-element master ships
    /// as fourteen however much room there was, and every source is panned
    /// onto the elements it brought.
    #[test]
    fn a_spare_element_is_taken_only_when_asked_for() {
        let mut parts = overlay_parts(&[]);
        for index in 0..6 {
            parts.push(Part::Object {
                source_channel: 8 + index,
                keyframes: vec![Keyframe {
                    position: [0.5, -0.5, 0.5],
                    ..Keyframe::default()
                }],
                bed: None,
            });
        }
        let elements = parts.len();
        assert_eq!(elements, 14);
        for (index, position) in [[-1.0, -1.0, 0.0], [0.0, 1.0, 0.0], [-0.577, 1.0, 0.0]]
            .into_iter()
            .enumerate()
        {
            parts.push(Part::Object {
                source_channel: 14 + index,
                keyframes: vec![Keyframe {
                    position,
                    ..Keyframe::default()
                }],
                bed: None,
            });
        }

        // The same master and the same room: asked for, two sources take a
        // slot; not asked for, none does.
        assert_eq!(spare_slots(&parts, elements, 3, true), vec![1, 2]);
        assert!(spare_slots(&parts, elements, 3, false).is_empty());
    }

    /// The gain used to compensate is the one a decoder will apply, which the
    /// syntax carries in whole decibels — not the master's own figure.
    #[test]
    fn the_compensating_gain_is_the_one_the_syntax_can_say() {
        assert!((stated_gain(1.0) - 1.0).abs() < 1e-12);
        assert!((stated_gain(0.5) - 0.5011872).abs() < 1e-6, "−6 dB exactly");
        // −5.4 dB is written as −5 and applied as −5, so that is what the
        // compensation has to divide by.
        assert!((stated_gain(0.5370) - stated_gain(0.5623)).abs() < 1e-12);
        assert_eq!(stated_gain(0.0), 0.0);
    }

    #[test]
    fn a_gain_of_one_is_written_as_unchanged_rather_than_as_zero_decibels() {
        // The syntax has a code for "unchanged" and its index cannot express
        // zero, so a unity gain has to take the first and not the second.
        assert_eq!(gain_of(1.0), Gain::Unity);
        assert_eq!(gain_of(0.0), Gain::Silent);
        assert_eq!(gain_of(0.5), Gain::Decibels(-6));
        assert_eq!(gain_of(2.0), Gain::Decibels(6));
    }

    #[test]
    fn a_gain_beyond_what_the_syntax_reaches_is_clamped_not_wrapped() {
        assert_eq!(gain_of(1e-4), Gain::Decibels(-49));
        assert_eq!(gain_of(100.0), Gain::Decibels(15));
    }

    #[test]
    fn object_size_maps_the_masters_width_onto_the_wires_thirty_firsts() {
        assert_eq!(size_of(0.0), Size::Point);
        assert_eq!(size_of(1.0), Size::Uniform(31));
        assert_eq!(size_of(0.5), Size::Uniform(16));
        // Anything audible rounds up rather than down to a point: a width the
        // master stated is a width it wanted.
        assert_eq!(size_of(0.001), Size::Uniform(1));
    }

    #[test]
    fn a_block_takes_the_longest_ramp_any_element_asked_for() {
        let parts = vec![
            Part::Object {
                source_channel: 0,
                keyframes: Vec::new(),
                bed: None,
            },
            Part::Object {
                source_channel: 1,
                keyframes: Vec::new(),
                bed: None,
            },
            Part::Absent,
        ];
        let live = |ramps: [u32; 2]| {
            vec![
                Live {
                    part_index: 0,
                    next: 0,
                    state: Keyframe {
                        ramp_samples: ramps[0],
                        ..Keyframe::default()
                    },
                },
                Live {
                    part_index: 1,
                    next: 0,
                    state: Keyframe {
                        ramp_samples: ramps[1],
                        ..Keyframe::default()
                    },
                },
                Live {
                    part_index: 2,
                    next: 0,
                    state: Keyframe::default(),
                },
            ]
        };
        assert_eq!(ramp_of(&live([0, 0]), &parts), Ramp::None);
        assert_eq!(ramp_of(&live([0, 1536]), &parts), Ramp::Samples(1536));
        assert_eq!(ramp_of(&live([512, 1536]), &parts), Ramp::Samples(1536));
        // The field is eleven bits; a longer ramp is stated as the longest it
        // can say rather than wrapping to a jump.
        assert_eq!(ramp_of(&live([9000, 0]), &parts), Ramp::Samples(2047));
    }

    #[test]
    fn padding_elements_are_marked_inactive_rather_than_parked_somewhere() {
        let parts = vec![
            Part::Lfe { source_channel: 0 },
            Part::Object {
                source_channel: 1,
                keyframes: Vec::new(),
                bed: None,
            },
            Part::Absent,
        ];
        let live: Vec<Live> = (0..3)
            .map(|part_index| Live {
                part_index,
                next: 0,
                state: Keyframe::default(),
            })
            .collect();
        let programme = programme(&live, &parts);
        assert!(programme.lfe, "the first element is the low frequency one");
        assert!(
            programme.blocks[0].objects[0].render.is_none(),
            "the LFE has no position"
        );
        assert!(programme.blocks[0].objects[1].active);
        assert!(
            !programme.blocks[0].objects[2].active,
            "padding is inactive"
        );
        assert_eq!(programme.blocks[0].objects[2].gain, Gain::Silent);
    }

    /// Without a fold the presentations are the master's own elements folded
    /// from where it puts them, at the gain it states for them: padding and an
    /// element it switched off contribute nothing, and the low frequency
    /// element goes to a presentation's own low frequency channel and nowhere
    /// else.
    #[test]
    fn a_plain_programme_folds_each_element_where_the_master_puts_it() {
        let object = |source_channel| Part::Object {
            source_channel,
            keyframes: Vec::new(),
            bed: None,
        };
        let mut parts = vec![
            Part::Lfe { source_channel: 0 },
            object(1),
            object(2),
            object(3),
        ];
        parts.resize_with(MIN_ELEMENTS, || Part::Absent);
        let placed = [
            ([-1.0, 1.0, 0.0], 1.0),
            ([1.0, 1.0, 0.0], 0.5),
            ([0.0, -1.0, 0.0], 0.0),
        ];
        let live: Vec<Live> = (0..parts.len())
            .map(|part_index| Live {
                part_index,
                next: 0,
                state: match placed.get(part_index.wrapping_sub(1)) {
                    Some((position, gain)) => Keyframe {
                        position: *position,
                        gain: *gain,
                        ..Keyframe::default()
                    },
                    None => Keyframe::default(),
                },
            })
            .collect();

        let mut positions = Vec::new();
        let mut stated = Vec::new();
        let presentations = plain_presentations(&live, &parts, &mut positions, &mut stated)
            .expect("nine elements are wider than every fold");
        assert_eq!(
            presentations.iter().map(|p| p.channels).collect::<Vec<_>>(),
            [2, 6, 8, MIN_ELEMENTS]
        );
        for (presentation, layout) in presentations.iter().zip([
            Layout::stereo(),
            Layout::surround_5_1(),
            Layout::surround_7_1(),
        ]) {
            let fold = ObjectFold::for_layout(&layout).expect("a fold");
            let low = layout.index_of("LFE1");
            let mut gains = vec![0.0; layout.channels()];
            for (channel, row) in presentation.rows.iter().enumerate() {
                let lfe = if low == Some(channel) { 1.0 } else { 0.0 };
                assert_eq!(row[0], lfe, "{}: the LFE, channel {channel}", layout.name);
                for (index, (position, gain)) in placed.iter().enumerate() {
                    fold.gains(*position, &mut gains);
                    let wanted = gains[channel] * stated_gain(*gain);
                    assert!(
                        (row[1 + index] - wanted).abs() < 1e-12,
                        "{}: object {index}, channel {channel}: {} against {wanted}",
                        layout.name,
                        row[1 + index]
                    );
                }
                assert!(
                    row[1 + placed.len()..].iter().all(|gain| *gain == 0.0),
                    "{}: padding folds to nothing",
                    layout.name
                );
            }
        }
        // Not vacuously: the half-level object is folded at the six decibels
        // the syntax says of it, and the switched-off one folds to nothing
        // from a place the 7.1 does reach — so its gain is what silenced it.
        assert!((stated_gain(0.5) - 10f64.powf(-6.0 / 20.0)).abs() < 1e-12);
        let seven = &presentations[2];
        assert!(seven.rows.iter().all(|row| row[3] == 0.0));
        let fold = ObjectFold::for_layout(&Layout::surround_7_1()).expect("a fold");
        let mut gains = vec![0.0; 8];
        fold.gains(placed[2].0, &mut gains);
        assert!(gains.iter().any(|gain| *gain > 0.1), "{gains:?}");
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    /// Off, or with nothing to be a fraction of, there is no reporter — a
    /// percentage of an unknown total is a number made up.
    #[test]
    fn there_is_nothing_to_report_without_a_total_or_a_flag() {
        assert!(Progress::new(false, 48_000).is_none());
        assert!(Progress::new(true, 0).is_none());
        assert!(Progress::new(true, 1).is_some());
    }

    /// A whole programme's worth of units yields at most a hundred and one
    /// lines, in order, without repeating — which is the whole reason it is a
    /// whole percent and not a frame count.
    #[test]
    fn a_run_reports_each_whole_percent_once_and_in_order() {
        // Two hours at 48 kHz, in forty-sample access units: 216 000 calls.
        let total = 48_000u64 * 3600 * 2;
        let mut progress = Progress::new(true, total).expect("a reporter");
        let mut seen = Vec::new();
        let mut at = 0u64;
        if let Some(percent) = progress.stepped(at) {
            seen.push(percent);
        }
        while at < total {
            at = (at + 40).min(total);
            if let Some(percent) = progress.stepped(at) {
                seen.push(percent);
            }
        }

        assert_eq!(seen.first(), Some(&0), "it starts at nothing");
        assert_eq!(seen.last(), Some(&100), "and ends at everything");
        assert_eq!(seen.len(), 101, "one line a percent, and no more");
        assert!(
            seen.windows(2).all(|pair| pair[1] > pair[0]),
            "never backwards, never twice"
        );
    }

    /// A short run reports fewer lines rather than the same hundred: eight
    /// frames cannot be twelve and a half per cent done.
    #[test]
    fn a_run_shorter_than_a_hundred_frames_reports_what_it_has() {
        let mut progress = Progress::new(true, 8).expect("a reporter");
        let seen: Vec<u64> = (0..=8).filter_map(|at| progress.stepped(at)).collect();
        assert_eq!(seen, vec![0, 12, 25, 37, 50, 62, 75, 87, 100]);
    }

    /// And a caller that overshoots its own total — a final unit padded past
    /// the end — does not get a hundred and four per cent.
    #[test]
    fn overshooting_the_total_still_ends_at_a_hundred() {
        let mut progress = Progress::new(true, 100).expect("a reporter");
        assert_eq!(progress.stepped(100), Some(100));
        assert_eq!(progress.stepped(140), None);
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    /// Twenty bits is a step of sixteen, twenty-four is a step of one.
    #[test]
    fn the_step_is_what_the_bits_leave() {
        assert_eq!(fold_step(24), 1.0);
        assert_eq!(fold_step(20), 16.0);
        assert_eq!(fold_step(FOLD_BITS), 16.0);
    }

    /// Every value the fold can write lands on the step — including the ones
    /// at the ends.
    ///
    /// This is the whole point: the encoder takes a block's dead low bits by
    /// looking for the bits that are zero in *every* sample of it, so one
    /// sample off the step costs the block all four. Rounding a value near
    /// full scale up would put it past `CEILING`, and clamping it back there
    /// would be exactly that one sample — 8 388 607 is not a multiple of
    /// sixteen.
    #[test]
    fn nothing_the_fold_writes_falls_off_the_step() {
        let step = fold_step(FOLD_BITS);
        let ceiling = (CEILING / step).floor() * step;
        assert_eq!(ceiling, 8_388_592.0, "the last multiple of sixteen inside");

        let written = |raw: f64| ((raw / step).round() * step).clamp(FLOOR, ceiling) as i32;
        for raw in [
            FLOOR,
            FLOOR + 1.0,
            -8.0,
            -7.0,
            0.0,
            7.0,
            8.0,
            9.0,
            CEILING - 16.0,
            CEILING - 1.0,
            CEILING,
            // And past the domain, which the mix can reach before it is
            // clamped.
            CEILING + 1000.0,
            FLOOR - 1000.0,
        ] {
            let value = written(raw);
            assert_eq!(value % 16, 0, "{raw} was written as {value}");
            assert!((FLOOR as i32..=CEILING as i32).contains(&value), "{value}");
            // And it is the nearest step, not a truncation towards zero: the
            // rounding has to be unbiased or the fold gains a DC offset.
            if (FLOOR..=CEILING - 16.0).contains(&raw) {
                assert!(
                    (f64::from(value) - raw).abs() <= step / 2.0,
                    "{raw} -> {value}"
                );
            }
        }
    }

    /// The bounds are the format's, not a taste: twenty-four is the codec's
    /// domain and seventeen is where its dead-bit field runs out.
    #[test]
    fn the_depths_end_where_the_format_does() {
        assert_eq!(*FOLD_DEPTHS.end(), 24, "the codec's own width");
        assert_eq!(
            *FOLD_DEPTHS.start(),
            24 - u32::from(hz_mlp::format::MAX_OUTPUT_SHIFT),
            "the coarsest grid a block can still declare"
        );
        assert!(FOLD_DEPTHS.contains(&FOLD_BITS), "the default is inside");
        // And the step at each end is what it should be.
        assert_eq!(fold_step(*FOLD_DEPTHS.end()), 1.0);
        assert_eq!(fold_step(*FOLD_DEPTHS.start()), 128.0);
    }

    /// The word stands as long as the format lets it, which is the cadence the
    /// reference streams use.
    ///
    /// Not decoration: the deadline is what tells a decoder the schedule is
    /// still live, and a word restated more often than it needs to be is
    /// bitrate spent saying the same thing. The reference writes 282 words
    /// over 36 106 units, which is one every 128.
    #[test]
    fn the_dynamic_range_word_stands_for_a_hundred_and_twenty_eight_units() {
        assert_eq!(DRC_REFRESH, hz_mlp::format::DynamicRange::MAX_REFRESH);
        assert_eq!(1u32 << DRC_REFRESH, 128);
        assert_eq!(u64::from(1u32 << DRC_REFRESH), REFRESH);
    }

    /// Unity is a gain the field can say exactly, and it is what the default
    /// states: a stated profile rather than an absent one.
    #[test]
    fn the_default_gain_is_unity_and_the_field_says_it_exactly() {
        let unity = hz_mlp::format::DynamicRange::from_db(0.0, DRC_REFRESH)
            .expect("unity is inside the field");
        assert_eq!(unity.gain, 0);
        assert_eq!(unity.db(), 0.0);
        // And a reduction is expressible, to a tenth of a decibel or so: the
        // field steps in sixty-fourths of a power of two, about 0.094 dB.
        let cut = hz_mlp::format::DynamicRange::from_db(-3.5, DRC_REFRESH).expect("inside");
        assert!((cut.db() + 3.5).abs() < 0.05, "{} dB", cut.db());
        // What does not fit is refused rather than clamped, so a caller that
        // asks for something the field cannot say hears about it.
        assert!(hz_mlp::format::DynamicRange::from_db(40.0, DRC_REFRESH).is_none());
    }

    /// Full depth writes what it was given, to the sample.
    #[test]
    fn full_depth_rounds_to_nothing_at_all() {
        let step = fold_step(24);
        let ceiling = (CEILING / step).floor() * step;
        assert_eq!(ceiling, CEILING);
        for raw in [FLOOR, -1.0, 0.0, 1.0, 8_388_591.0, CEILING] {
            assert_eq!(((raw / step).round() * step).clamp(FLOOR, ceiling), raw);
        }
    }
}

#[cfg(test)]
mod arrangement_tests {
    use hz_mlp::hierarchy::{self, Presentation};
    use hz_render::{Layout, ObjectFold};

    /// Twelve elements around a room, as a fold of a real scene leaves them:
    /// a ring at ear height and four overhead.
    fn scene() -> Vec<[f64; 3]> {
        (0..12)
            .map(|n| {
                if n < 8 {
                    let angle = n as f64 * std::f64::consts::TAU / 8.0;
                    [angle.sin(), angle.cos(), 0.0]
                } else {
                    let angle =
                        (n - 8) as f64 * std::f64::consts::TAU / 4.0 + std::f64::consts::FRAC_PI_4;
                    [angle.sin(), angle.cos(), 1.0]
                }
            })
            .collect()
    }

    /// What each presentation's channels are, over those elements.
    fn presentations(elements: &[[f64; 3]]) -> Vec<Presentation> {
        let mut out: Vec<Presentation> = [
            Layout::stereo(),
            Layout::surround_5_1(),
            Layout::surround_7_1(),
        ]
        .into_iter()
        .map(|layout| {
            let width = layout.channels();
            let fold = ObjectFold::for_layout(&layout).expect("a fold");
            let mut gains = vec![0.0f64; width];
            let rows = (0..width)
                .map(|channel| {
                    elements
                        .iter()
                        .map(|position| {
                            fold.gains(*position, &mut gains);
                            gains[channel]
                        })
                        .collect()
                })
                .collect();
            Presentation {
                channels: width,
                rows,
            }
        })
        .collect();

        // And the last one, which wants the elements themselves.
        out.push(Presentation {
            channels: elements.len(),
            rows: (0..elements.len())
                .map(|element| {
                    let mut row = vec![0.0; elements.len()];
                    row[element] = 1.0;
                    row
                })
                .collect(),
        });
        out
    }

    /// Arranging the elements cannot carry a 7.1, and the hierarchy can.
    ///
    /// The measurement that decided the design. Both halves are here because
    /// the second only means something against the first: storing the elements
    /// in a clever order gets six of the seven rows in and stops, for a reason
    /// no better order fixes — the two rows the 7.1 adds reach the height
    /// elements, which are stored in channels 8 to 11 and outside the eight the
    /// 7.1 carries. Storing the presentations instead gets all of them.
    #[test]
    fn the_hierarchy_carries_what_an_element_order_cannot() {
        let elements = scene();
        let presentations = presentations(&elements);

        // What arranging elements gives: each channel an element, and the
        // narrow presentations unable to reach past their own.
        let as_elements = hierarchy::Hierarchy {
            rows: (0..elements.len())
                .map(|element| {
                    let mut row = vec![0.0; elements.len()];
                    row[element] = 1.0;
                    row
                })
                .collect(),
            within: presentations
                .iter()
                .flat_map(|p| std::iter::repeat_n(p.channels, p.rows.len()))
                .take(elements.len())
                .collect(),
            filled: vec![false; elements.len()],
        };
        let (which, channel) = hierarchy::unreachable(&as_elements, &presentations)
            .expect("elements in channels cannot carry a fold");
        println!("  elements in channels: presentation {which} cannot reach its channel {channel}");
        assert_eq!(which, 0, "the stereo is the first that cannot");

        // And what the hierarchy gives.
        let built = hierarchy::build(&presentations, elements.len()).expect("a hierarchy");
        assert_eq!(built.rows.len(), elements.len(), "one row per element");
        assert_eq!(
            hierarchy::unreachable(&built, &presentations),
            None,
            "a presentation cannot reach one of its own channels"
        );
        println!("  hierarchy: {:?}", built.within);

        // Which presentation pays for which channel. The stereo takes two, the
        // 5.1 four more, the 7.1 two, and the rest restore the elements.
        let mut paid: Vec<usize> = built.within.clone();
        paid.dedup();
        assert_eq!(paid, vec![2, 6, 8, 12], "each presentation adds its own");

        // And the cascade that builds it exists: every row can be brought into
        // the channel that carries it, which is what an encoder has to write.
        let arrangement = hz_mlp::arrange::arrange(&built.rows, &built.within, elements.len())
            .expect("a cascade for the hierarchy");
        println!(
            "  cascade: {} steps, {} of {} channels brought in",
            arrangement.steps.len(),
            arrangement.hosts.iter().flatten().count(),
            built.rows.len()
        );

        let mut hosts: Vec<usize> = arrangement.hosts.iter().flatten().copied().collect();
        let brought = hosts.len();
        hosts.sort_unstable();
        hosts.dedup();
        assert_eq!(hosts.len(), brought, "two channels shared a host");

        // 🔴 And what the cascade *leaves* still carries every presentation.
        // Six of the twelve rows needed no channel of their own, so six
        // channels hold what they always held; the claim is not that the
        // channels equal the hierarchy's rows but that a presentation reading
        // its prefix can still produce its own. That is the property, and it is
        // checked against what the arithmetic actually left there.
        let stored_as = hierarchy::Hierarchy {
            rows: arrangement.held.clone(),
            within: built.within.clone(),
            filled: built.filled.clone(),
        };
        let through_order: Vec<Presentation> = presentations
            .iter()
            .map(|presentation| Presentation {
                channels: presentation.channels,
                rows: presentation.rows.clone(),
            })
            .collect();
        for (which, presentation) in through_order.iter().enumerate() {
            let one = hierarchy::leak(&stored_as, std::slice::from_ref(presentation));
            println!(
                "    presentation {which} ({} ch): {one:.2e} of its own rows out of reach",
                presentation.channels
            );
        }
        let leak = hierarchy::leak(&stored_as, &through_order);
        println!("  what a presentation cannot reach of its own rows: {leak:.2e}");
        // Measured: 2.9e-5, 1.0e-4, 1.5e-4, 0. Fourteen-bit coefficients put
        // the floor at about 6e-5 and a cascade accumulates a few of them, so
        // this is that floor and nothing else. It is not zero and is not meant
        // to be: a fold is carried to the precision of the field it is written
        // with. What is exact is the programme, and the round trip below is
        // what says so.
        assert!(
            leak < 1e-3,
            "the cascade left a presentation unable to reach its own channels: {leak:.2e}"
        );

        // And it costs the programme nothing, which is the only thing never
        // traded. The encoder undoes the declared steps, last to first; a
        // decoder applies them in order and has the elements back.
        let source: Vec<Vec<i32>> = (0..elements.len())
            .map(|channel| {
                (0..193)
                    .map(|frame| {
                        let n = (frame * 7919 + channel * 104_729) as i32;
                        (n % 16_777_216) - 8_388_608
                    })
                    .collect()
            })
            .collect();
        let mut stored = source.clone();
        for step in arrangement.steps.iter().rev() {
            hz_mlp::arrange::unapply(step, &mut stored);
        }
        assert_ne!(stored, source, "the cascade did nothing at all");
        for step in &arrangement.steps {
            hz_mlp::arrange::apply(step, &mut stored);
        }
        assert_eq!(stored, source, "the round trip lost a sample");
    }
}
