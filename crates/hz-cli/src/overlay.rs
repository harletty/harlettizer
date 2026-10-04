//! `--overlay`: keeping a programme's elements and panning its last few
//! objects onto them, whatever the stream is written as.
//!
//! The mode is the encoder's and not a format's. Its arithmetic — which
//! elements a source may be panned onto, the fit, the fallback, the guards,
//! which elements are copied and which mixed, the ramps, the limiter — is
//! decided here block by block, and a writer is handed the decision: an
//! element to copy from a master channel, or an element's mixed samples.
//! What differs between the formats is only what a decoder applies to an
//! element before rendering it, which a writer states per element per block:
//! a TrueHD stream's element gain is metadata a decoder applies, so a source
//! added to it is divided by it first; an IAMF object carries its gain in its
//! samples, so nothing is divided. See [`Kept::stated`].
//!
//! See [`hz_cluster::overlay`] for what the mode is for, and `docs/encode.md`
//! for what it guarantees and why it refuses.

use hz_cluster::Clustering;
use hz_cluster::mix::Limiter;
use hz_cluster::scene::Scene;
use hz_core::{Error, Result};
use hz_render::Keyframe;
use hz_render::Mode;
use std::path::Path;

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
pub(crate) struct Bounds {
    /// How far a source may be from where it asked to be, in degrees, before
    /// the block counts against it. Refused past [`OVERLAY_SHARE`] of the
    /// voiced blocks, or on any run longer than [`OVERLAY_RUN`].
    ///
    /// Thirty degrees is not a localisation bound — it is far past one. It is
    /// the point at which a voice is in a different part of the room from the
    /// picture, which is what a dub cannot ship with. Half of it is remarked
    /// on, which is about where the rear blur stops forgiving.
    pub(crate) drift: f64,
    /// The least the strongest element carrying a source may hold, as a
    /// fraction of the source's own level. Under it the source is diffuse:
    /// no one element is rendering it, so it arrives from everywhere the fit
    /// reached and moves whenever any of those elements does.
    ///
    /// A half is one element holding three quarters of the power. Refused past
    /// [`OVERLAY_SHARE`] of the voiced blocks.
    pub(crate) spread: f64,
    /// How much of the source's movement may be movement nobody asked for, as
    /// a percentage of the windows it was heard in — `hz_cluster::motion`'s
    /// `wobbling`, measured on where the carriers actually put the source.
    ///
    /// The sources of a dub are **static**, so every out-and-back in that
    /// measurement is the breathing the mode's own documentation warns about:
    /// a still voice carried by moving elements. This is the guard that
    /// catches it, and it is the only one that needs a time axis to see the
    /// defect at all.
    pub(crate) wobble: f64,
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
    pub(crate) level: f64,
    /// The worst a source's fold may cost, as a fraction of its own gain
    /// vector — `hz_cluster::metric`'s own measure, restricted to the sources.
    /// The elements are not in it: they are not folded.
    pub(crate) cost: f64,
    /// Whether a source the fit could not place acceptably is routed to the
    /// nearest bed element outright. See [`Overlaid::fell_back`].
    pub(crate) fallback: bool,
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

/// Samples in one overlay block: thirty-two forty-sample units, 1280 at
/// 48 kHz (26.7 ms) and as many more as the rate is a multiple of its family's
/// base — long enough that a block's power is an estimate rather than noise,
/// short enough that a weight's ramp still tracks what it carries.
///
/// The same block whatever the stream is written as, because the block is
/// where the fit is decided and the weights ramp across: two writers with
/// different blocks would write different overlays of one programme.
pub(crate) fn block_samples(sample_rate: u32) -> usize {
    let base = if sample_rate.is_multiple_of(44_100) && !sample_rate.is_multiple_of(48_000) {
        44_100
    } else {
        48_000
    };
    1280 * (sample_rate / base).max(1) as usize
}

/// One mixed sample into a writer's integers, `full_scale` to the unit and
/// rounded to `step`.
///
/// An element is a *sum*, and a sum of objects that happen to agree is louder
/// than any of them. Clamped rather than wrapped, and counted, because an
/// overlay that clips is one whose elements had no headroom left for what was
/// added to them. Counted against the domain *before* the step is applied: a
/// sample the mix pushed past full scale is a clip, and one the rounding
/// nudged over is not. The ceiling is the top of the domain that lands on the
/// step, so that one sample near full scale does not take a block's wasted
/// low bits with it.
#[inline]
pub(crate) fn quantise(
    value: f32,
    full_scale: f64,
    step: f64,
    peak: &mut f64,
    clipped: &mut u64,
) -> i32 {
    let (floor, ceiling) = (-full_scale, full_scale - 1.0);
    let raw = (f64::from(value) * full_scale).round();
    *peak = peak.max(raw.abs() / full_scale);
    if !(floor..=ceiling).contains(&raw) {
        *clipped += 1;
    }
    let top = (ceiling / step).floor() * step;
    ((raw / step).round() * step).clamp(floor, top) as i32
}

/// One kept element as a block ends: where the master has put it, and what a
/// decoder applies to its own samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Kept {
    pub(crate) position: [f64; 3],
    /// What a decoder multiplies the element's samples by before rendering
    /// it, so what a source's weight onto it is divided by for the sum to
    /// come out where the source asked; nought for an element a decoder
    /// silences, which carries nothing a source could be heard through.
    ///
    /// The writer's to say: a TrueHD element's gain is in its metadata, in
    /// the whole decibels the syntax carries; an IAMF object's is in its
    /// samples already, and this is one while it is heard at all.
    pub(crate) stated: f64,
}

/// One source as a block ends: where it asks to be, how wide, how it asks to
/// be rendered, and — for a source that took a spare element of its own —
/// what a decoder applies to that element.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Voice {
    pub(crate) position: [f64; 3],
    pub(crate) spread: f64,
    pub(crate) mode: Mode,
    pub(crate) stated: f64,
}

impl Voice {
    pub(crate) fn of(state: &Keyframe, stated: f64) -> Self {
        Self {
            position: state.position,
            spread: state.spread,
            mode: state.mode,
            stated,
        }
    }
}

/// What an overlay is set up with, once for the programme.
pub(crate) struct Setup<'a> {
    /// How many trailing objects of the master are sources.
    pub(crate) sources: usize,
    /// Elements the master brought, the low frequency channel included.
    pub(crate) elements: usize,
    /// Which sources take a spare element of their own — see
    /// [`spare_slots`].
    pub(crate) slots: Vec<usize>,
    /// Which elements are bed channels — see [`beds_among`].
    pub(crate) beds: Vec<bool>,
    /// How many of those the master declared.
    pub(crate) declared_beds: usize,
    /// The master channel of each element, slots included: see
    /// [`Overlaid::channels`].
    pub(crate) channels: Vec<Option<usize>>,
    pub(crate) bounds: Bounds,
    /// How far a bed-only fit may land before the moving elements are let in;
    /// `None` declines the preference. See `hz_cluster::overlay::BEDS_FIRST`.
    pub(crate) beds_first: Option<f64>,
    /// Where the block-by-block account goes, if anywhere.
    pub(crate) report: Option<&'a Path>,
    /// How long one block lasts, for the ruler that measures movement.
    pub(crate) seconds_a_block: f64,
}

/// One block, as the writer hands it over.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Block {
    /// One when element 0 is the low frequency channel, which has no signal
    /// among the objects; nought otherwise. The other kept elements are the
    /// objects in order, element `first + o` carrying object `o`.
    pub(crate) first: usize,
    /// Elements the stream carries: the kept ones, then the spare slots.
    pub(crate) elements: usize,
    pub(crate) frames: usize,
}

/// What a block mixes through: the writer's buffers, kept across blocks.
pub(crate) struct Buffers<'a> {
    /// The block's scene, every object in it — the kept elements' and the
    /// sources' — pushed and finished: where the energies the fit and the
    /// guards weigh by come from.
    pub(crate) scene: &'a Scene,
    /// Every object's samples for the block, as the mix takes them: the kept
    /// elements', then the sources'.
    pub(crate) signals: &'a [Vec<f32>],
    /// Each object's gain, applied before its weights: the sources' are the
    /// caller's, the elements' are set to one here.
    pub(crate) gains: &'a mut [f64],
    pub(crate) mixed: &'a mut Vec<Vec<f32>>,
    /// The previous block's mix matrix, which this one ramps from.
    pub(crate) from: &'a mut Vec<Vec<f64>>,
    pub(crate) limiter: Option<&'a mut Limiter>,
    /// The top of the writer's domain, as a fraction of full scale, for the
    /// limiter.
    pub(crate) ceiling: f64,
    pub(crate) floor: &'a hz_cluster::floor::Floor,
    pub(crate) renderers: &'a [Box<dyn hz_render::Renderer>],
}

/// What a block cost, for the writer's running totals.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Spent {
    /// The sources' fold cost over the presentations, mean and worst, when
    /// any source was judged.
    pub(crate) cost: Option<(f64, f64)>,
}

/// The writer's running totals, which the guards read beside their own.
pub(crate) struct Totals<'a> {
    pub(crate) blocks: u64,
    /// The sources' fold cost, summed over the blocks.
    pub(crate) mean: f64,
    pub(crate) clipped: u64,
    pub(crate) limiter: Option<&'a Limiter>,
}

/// Keeping the master's elements and panning its last few objects onto them
/// — `--overlay`. See [`hz_cluster::overlay`] for what this is for.
pub(crate) struct Overlaid {
    pub(crate) overlay: hz_cluster::overlay::Overlay,
    /// How many trailing objects of the master are sources rather than
    /// elements.
    pub(crate) sources: usize,
    /// Elements the master brought, the low frequency channel included. The
    /// sources that took a spare slot are written after these.
    pub(crate) elements: usize,
    /// One source's weights over this block's carriers, and the elements'
    /// metadata for the payload. Both are refilled rather than rebuilt: a
    /// block is 27 ms, and an allocation a block is an allocation forty times
    /// a second for the length of a feature.
    pub(crate) row: Vec<f64>,
    /// Which elements this block mixed, for the limiter.
    pub(crate) live: Vec<bool>,
    /// The sources that were panned rather than given an element, and their
    /// rows, rebuilt each block — what the metric judges.
    pub(crate) panned: Vec<hz_cluster::Object>,
    /// Whether each source took a spare element, by index. The same fact as
    /// [`Overlaid::slots`], in the shape three hot loops a block want it in.
    pub(crate) slotted: Vec<bool>,
    /// Which source took which spare element of its own, in the order the
    /// slots are written. See [`spare_slots`] and `docs/encode.md`.
    pub(crate) slots: Vec<usize>,
    /// The elements a source may be panned onto this block: active, not
    /// muted, and not the low frequency channel. Refilled every block.
    pub(crate) carriers: Vec<hz_cluster::overlay::Carrier>,
    /// Which elements are bed channels — see [`beds_among`], which works it
    /// out rather than trusting the master to declare it. Decided once, since
    /// it is a property of the whole programme and not of a block.
    pub(crate) beds: Vec<bool>,
    /// How many of them the master declared, so the summary can say which
    /// regime a run was in.
    pub(crate) declared_beds: usize,
    /// `weights[source][element]` as the fit gave them: what the held set is
    /// judged on and what the mix is built from.
    ///
    /// Two buffers that **swap** at the end of every block, so that a block
    /// costs no allocation. Between blocks `previous` therefore holds the
    /// latest answer and `weights` is the spare the next fit will fill —
    /// which is the right way round for the next block and the wrong way
    /// round for anything reading them afterwards.
    pub(crate) weights: Vec<Vec<f64>>,
    pub(crate) previous: Vec<Vec<f64>>,
    /// The sources as the fit sees them, rebuilt each block from the master's
    /// state and the block's own energies.
    pub(crate) fitting: Vec<hz_cluster::Object>,
    /// The mix matrix over every object of the master: an element carrying
    /// itself, a source spread over the carriers and divided by their gains.
    pub(crate) mix: Vec<Vec<f64>>,
    /// Where every element is this block, for the presentations.
    pub(crate) positions: Vec<[f64; 3]>,
    /// And the gain the payload states for each, in the same order: what a
    /// decoder rendering the elements applies, so what the presentations
    /// have to fold them at to sound like it.
    pub(crate) stated: Vec<f64>,
    /// Which elements a source reaches this block or reached last — the rest
    /// are copied through rather than mixed, and so are not rounded either.
    pub(crate) touched: Vec<bool>,
    /// Per element, the source channel it is copied from, or `None` when it is
    /// one of the few that this block actually mixes.
    pub(crate) copy_from: Vec<Option<usize>>,
    /// Element-blocks copied through untouched, and element-blocks mixed.
    pub(crate) copied: u64,
    pub(crate) remixed: u64,
    /// Blocks where an audible source had no carrier at all.
    pub(crate) stranded: u64,
    /// How far the carriers put the audible sources from where they asked to
    /// be, summed and at its worst — see `hz_cluster::overlay::drift`.
    pub(crate) drift: f64,
    pub(crate) drifted: u64,
    pub(crate) worst_drift: f64,
    /// What the encode refuses to write, and what it only remarks on.
    pub(crate) bounds: Bounds,
    /// Source-blocks in which each bound was exceeded, over the voiced ones.
    /// The denominator is [`Overlaid::drifted`]: a source is counted once a
    /// block, in the blocks it was audible in.
    pub(crate) over_drift: u64,
    /// And over half of it, which is what the remark is about — a remark that
    /// quotes the refusal's share beside the half bound contradicts itself.
    pub(crate) over_half_drift: u64,
    pub(crate) over_spread: u64,
    pub(crate) over_level: u64,
    pub(crate) over_cost: u64,
    /// The longest unbroken run of blocks a source was past the drift bound
    /// for, and the run each source is in now. In blocks; the summary turns
    /// them into seconds.
    pub(crate) run: Vec<u64>,
    pub(crate) worst_run: u64,
    /// The same for a source with nowhere at all to go, and the run it is in.
    pub(crate) stranded_run: Vec<u64>,
    pub(crate) worst_stranded_run: u64,
    /// How far a source's rendered level strayed on any presentation, at its
    /// worst, in decibels; and the worst a source's fold cost, with what each
    /// presentation cost in the block that was worst.
    pub(crate) worst_level: f64,
    pub(crate) worst_cost: f64,
    pub(crate) worst_cost_where: String,
    /// Where the carriers actually put each source, block after block, and
    /// what of that movement nobody asked for — `hz_cluster::motion`. The
    /// sources are static, so all of it is invented.
    pub(crate) motion: hz_cluster::motion::Motion,
    pub(crate) resultants: Vec<[f64; 3]>,
    pub(crate) energies: Vec<f64>,
    /// Where each source was last heard, held through the silence after it so
    /// that a phrase boundary is not read as movement. `None` before a source
    /// has ever been audible.
    pub(crate) resting: Vec<Option<[f64; 3]>>,
    /// Whether each source was above the floor this block and the last, which
    /// is what decides an element is copied rather than what its weights say.
    pub(crate) audible: Vec<bool>,
    pub(crate) was_audible: Vec<bool>,
    /// Blocks in which nothing at all was mixed — every element copied — which
    /// is the share of the *running time* the fast path took, as against the
    /// share of element-blocks it took.
    pub(crate) whole: u64,
    /// Blocks in which at least one source was above the audibility floor,
    /// which is what every share below is a share *of*. A dub is silent most
    /// of the time, and a guard counting silent blocks would measure how much
    /// of the programme nobody is speaking in.
    pub(crate) voiced: u64,
    /// Source-blocks routed to the nearest bed because the fit could not place
    /// them acceptably — see `--overlay-fallback`.
    pub(crate) fell_back: u64,
    /// The clustering the sources are judged as, rebuilt each block: the
    /// elements where the master put them, and the sources' own weights.
    pub(crate) judged: Clustering,
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
    pub(crate) report: Option<std::io::BufWriter<std::fs::File>>,
    /// Blocks and samples written so far, so the account can say where it is
    /// without the span loop having to tell it.
    pub(crate) reported_blocks: u64,
    pub(crate) reported_at: u64,
    /// The master channel each element's own audio is — the kept elements',
    /// then the spare slots' sources' — or `None` for an element that has
    /// none. What a copied element is copied from, and what the account names.
    pub(crate) channels: Vec<Option<usize>>,
    /// What a decoder applies to each element this block, by element: what a
    /// source's weight onto it is divided by, nought for one it silences.
    pub(crate) by_element: Vec<f64>,
    /// The block's kept elements and sources, as the caller states them —
    /// see [`Overlaid::span`]. Filled by the caller and refilled every block.
    pub(crate) kept: Vec<Option<Kept>>,
    pub(crate) voices: Vec<Voice>,
}

impl Overlaid {
    pub(crate) fn new(input: &Path, setup: Setup<'_>) -> Result<Self> {
        let Setup {
            sources,
            elements,
            slots,
            beds,
            declared_beds,
            channels,
            bounds,
            beds_first,
            report,
            seconds_a_block,
        } = setup;
        let width = elements + slots.len();
        debug_assert_eq!(channels.len(), width);
        Ok(Self {
            overlay: hz_cluster::overlay::Overlay::new()
                .map_err(|why| Error::unsupported(input, why.to_string()))?
                .preferring(beds_first),
            sources,
            elements,
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
            beds,
            declared_beds,
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
            motion: hz_cluster::motion::Motion::new(hz_cluster::motion::HEARING, seconds_a_block),
            resultants: Vec::new(),
            energies: Vec::new(),
            resting: Vec::new(),
            audible: Vec::new(),
            was_audible: Vec::new(),
            whole: 0,
            voiced: 0,
            fell_back: 0,
            judged: Clustering {
                positions: Vec::new(),
                directions: Vec::new(),
                weights: Vec::new(),
                owners: Vec::new(),
                modes: Vec::new(),
                bounded: 0,
            },
            report: match report {
                Some(path) => Some(std::io::BufWriter::new(
                    std::fs::File::create(path).map_err(|e| Error::io(path, e))?,
                )),
                None => None,
            },
            reported_blocks: 0,
            reported_at: 0,
            channels,
            by_element: Vec::new(),
            kept: Vec::new(),
            voices: Vec::new(),
        })
    }

    /// Keep the master's elements and pan its last few objects onto them, for
    /// one block.
    ///
    /// The elements arrive as they are: element `e` carries its own channel,
    /// under its own metadata. A source is panned onto the elements that are
    /// live this block — see [`hz_cluster::overlay`] — and added to them,
    /// divided by what a decoder applies to each ([`Kept::stated`]) so that
    /// what a decoder renders is the element as it was plus the source where
    /// it asked to be.
    ///
    /// The caller fills [`Overlaid::kept`] — every kept element as the block
    /// ends, `None` for the low frequency channel, which is never panned onto
    /// — and [`Overlaid::voices`], every source, and is handed back, per
    /// element, either [`Overlaid::copy_from`] — the master channel to copy —
    /// or its mixed samples in `buffers.mixed`.
    ///
    /// # The fast path is the point
    ///
    /// An element no source reaches this block, and reached by none last block
    /// either, is **copied**. Not summed with a row of zeroes and not rounded to
    /// `--fold-depth`: the samples the master brought, written out unchanged. On a
    /// dub that is most of the programme, and it is the difference between a
    /// re-voiced stream and a re-encoded one. The low frequency channel is always
    /// copied, and so is an element a source took a spare slot in, which
    /// carries that source alone.
    pub(crate) fn span(&mut self, block: Block, buffers: Buffers<'_>) -> std::io::Result<Spent> {
        let Block {
            first,
            elements,
            frames,
        } = block;
        let Buffers {
            scene,
            signals,
            gains,
            mixed,
            from,
            limiter,
            ceiling,
            floor,
            renderers,
        } = buffers;
        let kept = std::mem::take(&mut self.kept);
        let voices = std::mem::take(&mut self.voices);
        let sources = self.sources;
        let objects = signals.len();
        // The master's own objects, which are the elements, and the ones
        // appended after them, which are the sources. An object's index among
        // the signals is its element's, less the low frequency channel that
        // has no signal of its own here.
        let carried = objects.saturating_sub(sources);
        let mut cost = None;

        // Where each element is, and what a decoder will apply to it. Both are
        // the master's, read off this block's state; the elements are the
        // ones before the sources, and a spare slot's element is the source
        // that took it.
        self.carriers.clear();
        self.positions.clear();
        self.stated.clear();
        self.by_element.clear();
        self.by_element.resize(elements, 1.0);
        for (element, state) in kept.iter().enumerate().take(self.elements) {
            // The low frequency channel has no position and is never panned
            // onto: vector-base panning needs a direction and it has none, and
            // the presentations fold it by its own rule.
            let Some(state) = state else {
                continue;
            };
            self.positions.push(state.position);
            // An element a decoder will silence carries nothing a source could
            // be heard through, and dividing by its gain would divide by
            // nought. *Muted*, and not "inactive": the master's keyframes carry
            // no active flag, so a silenced element is one whose stated gain is
            // nought and nothing here can see any other kind.
            self.by_element[element] = state.stated;
            self.stated.push(state.stated);
            if state.stated <= 0.0 {
                continue;
            }
            self.carriers.push(hz_cluster::overlay::Carrier {
                element,
                position: state.position,
                bed: self.beds.get(element).copied().unwrap_or(false),
            });
        }
        // A source that took a spare element of its own is an element the
        // presentations fold like any other, and it is never a carrier: the
        // scene a source is panned onto is the master's, not the other
        // sources'. Its audio is the source's own, and what a decoder applies
        // to it is the source's.
        for index in &self.slots {
            let voice = &voices[*index];
            self.positions.push(voice.position);
            self.stated.push(voice.stated);
        }

        // Fit the sources onto them.
        let scene_objects = scene.objects();
        self.fitting.clear();
        for (index, voice) in voices.iter().enumerate().take(sources) {
            let object = carried + index;
            self.fitting.push(hz_cluster::Object {
                position: voice.position,
                energy: scene_objects.get(object).map_or(0.0, |o| o.energy),
                size: voice.spread,
                pinned: None,
                mode: voice.mode,
                peak: 0.0,
            });
        }
        self.overlay.fit(
            elements,
            &self.carriers,
            &self.fitting,
            Some(&self.previous),
            &mut self.weights,
        );

        // What the fit did to each source, and whether it is good enough to ship.
        //
        // This runs *before* the mix, because the fallback changes the weights:
        // a source the fit could not place acceptably is routed to the nearest
        // bed outright, which is audible and slightly misplaced rather than
        // diffuse and wandering. See [`Bounds`].
        self.resultants.clear();
        self.energies.clear();
        self.run.resize(self.sources, 0);
        self.resting.resize(self.sources, None);
        self.stranded_run.resize(self.sources, 0);
        std::mem::swap(&mut self.was_audible, &mut self.audible);
        self.audible.clear();
        self.audible.resize(self.sources, false);
        self.was_audible.resize(self.sources, false);
        for index in 0..self.sources {
            let source = self.fitting[index];
            let slotted = self.slotted[index];
            // A source with an element of its own is exactly where it asked to
            // be, and a source nobody can hear is not a fact about the
            // programme. Neither is judged, and neither breaks a run.
            let voiced = !slotted && floor.heard(source.energy.max(0.0));
            self.audible[index] = voiced;
            self.energies
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
                let held = self
                    .resting
                    .get(index)
                    .copied()
                    .flatten()
                    .unwrap_or_else(|| hz_cluster::direction(source.position));
                self.resultants.push(held);
                // And silence ends a run: two phrases either side of a pause are
                // two runs, not one long one.
                self.run[index] = 0;
                self.stranded_run[index] = 0;
                continue;
            }

            // The source's weights over this block's carriers rather than over
            // every element, which is the shape the measures want. One buffer,
            // refilled: this runs once per audible source per block.
            let row = &mut self.row;
            row.clear();
            row.extend(
                self.carriers
                    .iter()
                    .map(|carrier| self.weights[index][carrier.element]),
            );
            let mut drift = hz_cluster::overlay::drift(source.position, &self.carriers, row);
            let mut spread = hz_cluster::overlay::spread(row);

            // The fallback, on the two conditions a fallback can fix: the source
            // went nowhere at all, or it went somewhere far enough off, or it
            // went everywhere at once. Where it went slightly wrong the fit's
            // answer is better than a bed's, and it is left alone.
            let unplaceable = drift.is_none()
                || drift.is_some_and(|at| self.bounds.drift > 0.0 && at > self.bounds.drift)
                || (self.bounds.spread > 0.0 && spread.strongest < self.bounds.spread);
            if unplaceable && self.bounds.fallback {
                if let Some(slot) =
                    hz_cluster::overlay::nearest_bed(&self.carriers, source.position)
                {
                    row.iter_mut().for_each(|weight| *weight = 0.0);
                    row[slot] = 1.0;
                    for (carrier, weight) in self.carriers.iter().zip(row.iter()) {
                        self.weights[index][carrier.element] = *weight;
                    }
                    drift = hz_cluster::overlay::drift(source.position, &self.carriers, row);
                    spread = hz_cluster::overlay::spread(row);
                    self.fell_back += 1;
                }
            }

            match drift {
                Some(at) => {
                    self.drift += at;
                    self.drifted += 1;
                    self.worst_drift = self.worst_drift.max(at);
                    if self.bounds.drift > 0.0 && at > self.bounds.drift / 2.0 {
                        self.over_half_drift += 1;
                    }
                    if self.bounds.drift > 0.0 && at > self.bounds.drift {
                        self.over_drift += 1;
                        self.run[index] += 1;
                        self.worst_run = self.worst_run.max(self.run[index]);
                    } else {
                        self.run[index] = 0;
                    }
                    self.stranded_run[index] = 0;
                    let went = hz_cluster::overlay::resultant(&self.carriers, row)
                        .unwrap_or_else(|| hz_cluster::direction(source.position));
                    self.resting[index] = Some(went);
                    self.resultants.push(went);
                }
                // Audible, and carried by nothing at all: every element silenced
                // by the gain it states, and no bed to fall back to either.
                // Counted rather than guessed at.
                None => {
                    // A source that went nowhere was still *audible*, so it counts
                    // towards the blocks the shares are over. Without this the
                    // denominator was the blocks that were placed, and a run
                    // stranded throughout divided by one.
                    self.stranded += 1;
                    self.drifted += 1;
                    self.run[index] = 0;
                    self.stranded_run[index] += 1;
                    self.worst_stranded_run = self.worst_stranded_run.max(self.stranded_run[index]);
                    let held = self
                        .resting
                        .get(index)
                        .copied()
                        .flatten()
                        .unwrap_or_else(|| hz_cluster::direction(source.position));
                    self.resultants.push(held);
                }
            }
            if self.bounds.spread > 0.0 && spread.strongest < self.bounds.spread {
                self.over_spread += 1;
            }
        }

        // What the sources cost and what level they come out at, on every
        // presentation the stream is played through — the elements as the master
        // placed them, the sources' own weights, and `hz_cluster::metric`'s own
        // measures over the two. The elements are not in this: they are not
        // folded, so they cost nothing by construction.
        self.judged.positions.clear();
        self.judged
            .positions
            .extend(self.carriers.iter().map(|carrier| carrier.position));
        // Only the sources that were actually panned. One that took a spare
        // element of its own is not folded at all — it *is* an element, carried
        // whole and rendered from its own place — so judging its unused pan lets
        // a source that costs nothing by construction push the guards around.
        self.panned.clear();
        let mut judged = 0usize;
        for index in 0..self.sources {
            if self.slotted[index] {
                continue;
            }
            // Filled in place: the rows are the same shape block after block once
            // the carrier count settles, so this allocates on the first block and
            // never again.
            if self.judged.weights.len() <= judged {
                self.judged.weights.push(Vec::new());
            }
            let row = &mut self.judged.weights[judged];
            row.clear();
            row.extend(
                self.carriers
                    .iter()
                    .map(|carrier| self.weights[index][carrier.element]),
            );
            self.panned.push(self.fitting[index]);
            judged += 1;
        }
        self.judged.weights.truncate(judged);
        if !self.carriers.is_empty()
            && !self.panned.is_empty()
            && let Ok(report) =
                hz_cluster::metric::error_with(&self.panned, &self.judged, renderers, floor)
        {
            cost = Some((report.mean, report.worst));
            if report.worst > self.worst_cost {
                self.worst_cost = report.worst;
                // Which presentation it was worst on, and what the source asked
                // for there. A cost is a *relative* error — how far the carriers
                // land from the source, over what the source itself radiates on
                // that layout — so a refusal that does not name the layout leaves
                // a caller unable to tell a real geometric miss from a small
                // denominator. Kept only for the worst block, which is the one
                // the refusal quotes.
                self.worst_cost_where.clear();
                for layer in &report.layouts {
                    if !self.worst_cost_where.is_empty() {
                        self.worst_cost_where.push_str(", ");
                    }
                    // The cost *and* its denominator. A relative error cannot be
                    // read without the vector it is relative to: a presentation
                    // the source barely reaches has a small own norm, and a small
                    // absolute error over it is a large relative one. Printing
                    // both is what lets a caller tell that case from a real miss
                    // without taking anybody's word for it.
                    self.worst_cost_where.push_str(&format!(
                        "{} {:.2} of {:.2}",
                        layer.name, layer.worst, layer.own
                    ));
                }
            }
            if self.bounds.cost > 0.0 && report.worst > self.bounds.cost {
                self.over_cost += 1;
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
            self.worst_level = self.worst_level.max(strayed);
            if self.bounds.level > 0.0 && strayed > self.bounds.level {
                self.over_level += 1;
            }
        }
        // Where the carriers put the sources, block after block. The sources are
        // static, so anything this calls movement is movement nobody asked for.
        self.motion.record(&self.resultants, &self.energies);
        if self.energies.iter().any(|energy| *energy > 0.0) {
            self.voiced += 1;
        }

        // Which elements are mixed and which are copied. An element is mixed if a
        // source reaches it now *or* reached it last block, because the weights
        // ramp across the boundary and a ramp from something to nothing is still
        // something for most of the block.
        self.copy_from.clear();
        self.copy_from.resize(elements, None);
        self.touched.clear();
        self.touched.resize(elements, false);
        for (index, row) in self.weights.iter().enumerate() {
            if self.slotted[index] {
                continue;
            }
            // A source that nobody could hear in this block or the last reaches
            // nothing, whatever its weights say: it contributed `w · 0` to both,
            // so the elements it names are the master's own samples and are
            // copied. One block of memory because the weights ramp across a
            // boundary — the block after a source falls silent still carries the
            // tail of the block before it.
            if !self.audible[index] && !self.was_audible[index] {
                continue;
            }
            let was = self.previous.get(index);
            for element in 0..elements {
                if row[element] != 0.0 || was.is_some_and(|was| was[element] != 0.0) {
                    self.touched[element] = true;
                }
            }
        }
        for element in 0..self.elements {
            if !self.touched[element]
                && let Some(channel) = self.channels[element]
            {
                self.copy_from[element] = Some(channel);
            }
        }
        // A source that took a spare slot is an element of its own carrying
        // nothing else, so it is copied too, and its metadata is its own.
        for slot in 0..self.slots.len() {
            let element = self.elements + slot;
            if let Some(channel) = self.channels[element] {
                self.copy_from[element] = Some(channel);
            }
        }

        // The mix matrix over every object: an element carries itself at unity
        // when it is mixed at all, and a source is spread over the carriers,
        // divided by each one's stated gain so the sum renders where it asked to.
        self.mix.clear();
        self.mix.resize_with(objects, Vec::new);
        for row in self.mix.iter_mut() {
            row.clear();
            row.resize(elements, 0.0);
        }
        for (object, gain) in gains.iter_mut().enumerate().take(carried) {
            let element = object + first;
            if self.copy_from[element].is_none() {
                self.mix[object][element] = 1.0;
            }
            // An element's own audio goes through as it is: the level a decoder
            // applies is the writer's to state — in a TrueHD stream's metadata,
            // in an IAMF object's samples — so applying it here as well would
            // apply it twice.
            *gain = 1.0;
        }
        for index in 0..sources {
            if self.slotted[index] {
                continue;
            }
            for element in 0..elements {
                let weight = self.weights[index][element];
                let stated = self.by_element[element];
                if weight != 0.0 && stated > 0.0 {
                    self.mix[carried + index][element] = weight / stated;
                }
            }
        }

        // What the weights ramp from: the previous block's matrix, or this one's
        // where there is nothing to ramp from.
        if from.len() != objects || from.first().is_none_or(|row| row.len() != elements) {
            from.clear();
            from.extend(self.mix.iter().cloned());
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
            let carries_itself = self.mix[object][element];
            if carries_itself != 0.0 {
                from[object][element] = carries_itself;
            }
        }
        hz_cluster::mix::mix(signals, gains, from, &self.mix, frames, mixed);
        let mut limited = false;
        if let Some(limiter) = limiter {
            // Only the elements this block actually mixed. The rest are copied
            // straight out of the master and never reach `mixed` at all, so their
            // rows are noughts the limiter would otherwise scan twice a block.
            self.live.clear();
            self.live.extend(self.copy_from.iter().map(Option::is_none));
            limited = limiter.apply_to(mixed, ceiling, &self.live);
        }

        if self.copy_from.iter().all(Option::is_some) {
            self.whole += 1;
        }
        self.copied += self.copy_from.iter().filter(|from| from.is_some()).count() as u64;
        self.remixed += self.copy_from.iter().filter(|from| from.is_none()).count() as u64;

        // What this block did, for a checker that has to reproduce it. Written
        // before the buffers are swapped, while `self.mix` still holds the
        // weights this block ended at.
        if self.report.is_some() {
            self.write_report(gains, from, carried, frames, elements, limited)?;
        }
        self.reported_blocks += 1;
        self.reported_at += frames as u64;

        std::mem::swap(from, &mut self.mix);
        std::mem::swap(&mut self.previous, &mut self.weights);

        self.kept = kept;
        self.voices = voices;
        Ok(Spent { cost })
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
    fn write_report(
        &mut self,
        gains: &[f64],
        from: &[Vec<f64>],
        carried: usize,
        frames: usize,
        elements: usize,
        limited: bool,
    ) -> std::io::Result<()> {
        use std::io::Write as _;
        let block = self.reported_blocks;
        let at = self.reported_at;
        // Split the borrow: the writer is one field and everything it reads is
        // another, which is what lets this take `&mut Overlaid` and still read.
        let Overlaid {
            report,
            copy_from,
            mix,
            sources,
            slotted,
            channels,
            ..
        } = self;
        let Some(out) = report.as_mut() else {
            return Ok(());
        };
        if block == 0 {
            for (element, channel) in channels.iter().enumerate().take(elements) {
                if let Some(channel) = channel {
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
}

/// One thing the guards found, and how seriously.
pub(crate) struct Remark {
    pub(crate) refused: bool,
    pub(crate) said: String,
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
pub(crate) fn remarks(overlaid: &Overlaid, totals: &Totals, seconds_a_block: f64) -> Vec<Remark> {
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
        } else if totals.blocks > 0 && totals.mean / totals.blocks as f64 > 0.05 {
            say(
                false,
                format!(
                    "the sources cost {:.4} of their own gains on average",
                    totals.mean / totals.blocks as f64
                ),
            );
        }
    }

    if totals.clipped > 0 {
        say(
            true,
            format!(
                "{} samples went outside the codec's domain and were clamped; an overlay that \
                 clips is one whose elements had no headroom left for what was added to them",
                totals.clipped
            ),
        );
    } else if let Some(limiter) = totals.limiter
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
pub(crate) fn summary(overlaid: &Overlaid, blocks: u64, beds_reach: Option<f64>) {
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
        match beds_reach {
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

/// What the guards make of a finished stream, said: the remarks as `note`
/// lines, and the refusals on standard error as `error: overlay: …` with an
/// error that says how many — the shape every other refusal here takes, and
/// the line a process host greps for.
///
/// The stream is left where it was written: a refusal is a statement about
/// what is in the file, and the fastest way to check a bound nobody has
/// listened to yet is to listen to what it stopped.
pub(crate) fn verdict(remarks: &[Remark], out: &Path) -> Result<()> {
    for remark in remarks.iter().filter(|remark| !remark.refused) {
        println!("  note         {}", remark.said);
    }
    let refusals: Vec<&Remark> = remarks.iter().filter(|remark| remark.refused).collect();
    if refusals.is_empty() {
        return Ok(());
    }
    for refusal in &refusals {
        eprintln!("error: overlay: {}", refusal.said);
    }
    Err(Error::refused(
        out,
        format!(
            "the overlay is outside {} of its bounds; the stream was written and left \
             in place so that it can be heard, and every bound is a flag that turns it \
             off — see docs/encode.md",
            refusals.len()
        ),
    ))
}

/// Which of a master's elements are bed channels, whether or not the master
/// says so.
///
/// `elements` holds, per element, its keyframes and whether the master
/// declared it a bed channel, or `None` for an element with no position — the
/// low frequency channel. `grid` is the writer's own coding of a position:
/// two positions that code to the same value are the same position as far as
/// any decoder of that stream is concerned, so that is the resolution to ask
/// "exactly at a speaker's place" at, and it needs no tolerance invented here.
///
/// # Why this cannot be read off the declaration
///
/// **A dub's master declares no bed.** A programme decoded back out of a
/// delivery stream carries its 7.1 bed as ordinary objects that never move
/// from their speakers' places, and declares only the low frequency channel
/// as a bed; that is what a decoder can reconstruct, and it is what arrives
/// here. Read off the declaration, every bed-related mechanism of the overlay
/// was inert on the one input it exists for: the beds-first preference, the
/// fallback, a source with every carrier muted rescued to a bed.
///
/// # What counts as a bed
///
/// An element that **never moves** and sits **exactly at a speaker's place**,
/// both judged on the writer's grid. A static object somewhere that is *not*
/// a speaker's place is not a bed: it is an object that happens to be still,
/// and pinning a source to it would be choosing a place the mix never called a
/// speaker.
pub(crate) fn beds_among<Q: PartialEq>(
    elements: &[Option<(&[Keyframe], bool)>],
    grid: impl Fn([f64; 3]) -> Q,
) -> Vec<bool> {
    let places: Vec<Q> = hz_core::speakers::SPEAKERS
        .iter()
        .filter(|speaker| !speaker.is_lfe())
        .filter_map(|speaker| hz_render::fold::bed_position(speaker.master))
        .map(&grid)
        .collect();
    elements
        .iter()
        .map(|element| match element {
            Some((_, true)) => true,
            Some((keyframes, false)) => {
                let Some(first) = keyframes.first() else {
                    return false;
                };
                let at = grid(first.position);
                keyframes.iter().all(|frame| grid(frame.position) == at) && places.contains(&at)
            }
            None => false,
        })
        .collect()
}

/// Which sources take a spare element of their own, in the order those
/// elements are written: `origins` is where each source starts, `spare` how
/// many more elements the stream has room for than the master brings.
///
/// # Only when asked for
///
/// A spare element makes the stream wider than the master. By default a
/// re-voiced programme keeps the original's width — `allowed` is false, this
/// is empty however much room there was, and every source is panned onto the
/// elements the master brought. `--overlay-spare` is what lets the sources
/// take the room.
///
/// # Which sources get them
///
/// The ones nearest the front centre, which is where a dub's dialogue is and
/// so where an error is least forgiven. Stated as an angle rather than as a
/// channel name on purpose: the sources are objects at speaker places and
/// what makes one the centre is where it is, not what a master called it.
pub(crate) fn spare_slots(origins: &[Option<[f64; 3]>], spare: usize, allowed: bool) -> Vec<usize> {
    let sources = origins.len();
    if !allowed || spare == 0 || sources == 0 {
        return Vec::new();
    }
    let ahead = |index: usize| -> f64 {
        let Some(at) = origins[index] else {
            return f64::MIN;
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The block is the TrueHD encode's thirty-two units of forty samples,
    /// on either family of rates.
    #[test]
    fn a_block_is_thirty_two_units_of_forty_samples() {
        assert_eq!(block_samples(48_000), 1280);
        assert_eq!(block_samples(96_000), 2560);
        assert_eq!(block_samples(44_100), 1280);
        assert_eq!(block_samples(88_200), 2560);
    }

    /// Rounded to the step, clamped to the top of the domain that lands on
    /// it, and a clip counted only where the mix itself went past full scale.
    #[test]
    fn a_mixed_sample_lands_on_the_step_inside_the_domain() {
        let (mut peak, mut clipped) = (0.0, 0u64);
        let full = 8_388_608.0;
        assert_eq!(
            quantise(0.5, full, 16.0, &mut peak, &mut clipped),
            4_194_304
        );
        assert_eq!(
            quantise(
                1.0 / full as f32 * 23.0,
                full,
                16.0,
                &mut peak,
                &mut clipped
            ),
            16
        );
        assert_eq!(clipped, 0);
        let top = quantise(1.0, full, 16.0, &mut peak, &mut clipped);
        assert_eq!(top, 8_388_592);
        assert_eq!(clipped, 1);
        assert_eq!(
            quantise(-1.0, full, 16.0, &mut peak, &mut clipped),
            -8_388_608
        );
        assert_eq!(clipped, 1);
    }
}
