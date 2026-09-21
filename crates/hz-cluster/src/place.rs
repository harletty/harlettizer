//! Moving an element to where the metric says it should be.
//!
//! The elements are placed by a clustering **of directions** and judged by a
//! difference of **gain vectors**. Those are two different criteria, and
//! nothing in [`crate::partition`] optimises the one that decides.
//!
//! # What was tried before, and why it is not the same thing
//!
//! `docs/clustering.md` records an attempt at closing that gap and rejects it:
//! put each element at the cube position whose gain vector best matches the
//! energy-weighted mean of its members' gain vectors. It was worse everywhere —
//! 0.212 against 0.174 at six elements — and the reason given was that a fold
//! is not linear in position, so averaging in one space and inverting a
//! non-linear map into another does not land on the minimiser.
//!
//! That reason is right and it also says what the attempt was: a **proxy**. The
//! quantity the metric reports is
//!
//! ```text
//! Σ_i  e_i · ‖ Σ_k w_ik·r(q_k) − r(p_i) ‖² / ‖ r(p_i) ‖²
//! ```
//!
//! and no mean of gain vectors appears in it. This minimises that expression
//! directly instead: move one element, keep the rest, evaluate what the metric
//! would say, and keep the move if it says less.
//!
//! # How it is cheap enough to do
//!
//! Moving element `k` changes only the objects whose weight on `k` is not zero,
//! and for each of those it changes one term of a sum. So the sum without `k`
//! is computed once per element and each candidate costs one panning of the
//! candidate position plus a dot product per object it touches — which is why
//! this is a few tens of microseconds a block rather than a few thousand.
//!
//! The residual is taken over [`crate::fit::Reference`], which is the stacked
//! gain vectors of the four presentations: the same geometry the metric
//! measures on and the same one the weights were fitted against, so the search
//! and the score cannot disagree about what they are for.
//!
//! # And what it costs to move
//!
//! An element that moves takes every object folded into it along, including
//! the ones that did not move — which the metric cannot see, because it judges
//! one instant and never a movement. So the score of a candidate carries a
//! term for how far it is from where the element was last block, weighted by
//! what the element carries. Without it the search is free to trade a
//! thousandth of error for an element sweeping across the room.

use crate::fit::Reference;
use crate::{Clustering, Object};

/// How hard to look for a better place for each element, and what moving
/// costs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Search {
    /// Passes over the elements. Each halves the reach of the last.
    pub passes: usize,
    /// How far a candidate sits from the element, in cube units, on the first
    /// pass.
    pub reach: f64,
    /// What moving an element costs, per unit of energy it carries and per
    /// unit of `1 − cos` from where it was last block.
    pub movement: f64,
    /// Whether the position of the element's dominant member is offered as a
    /// candidate beside the six steps — see [`DOMINANT`].
    pub dominant: bool,
    /// How much better the best place has to be than the one the element was
    /// in last block before it goes there — see [`HOLDING`]. Nought lets it
    /// move for any gain at all.
    pub holding: f64,
}

impl Search {
    /// No search at all: the positions the clustering settled on stand.
    pub const OFF: Self = Self {
        passes: 0,
        reach: 0.0,
        movement: 0.0,
        dominant: false,
        holding: 0.0,
    };
}

impl Default for Search {
    fn default() -> Self {
        Self {
            passes: PASSES,
            reach: REACH,
            movement: MOVEMENT,
            dominant: DOMINANT,
            holding: HOLDING,
        }
    }
}

/// Passes of the search, each looking half as far as the last.
///
/// Measured on `crowd40` into twelve elements, mean error over the
/// presentations against what the clustering costs a block:
///
/// | passes | mean | worst | a block |
/// |---|---|---|---|
/// | 0 | 0.044 | 0.377 | 58 µs |
/// | 2 | 0.042 | 0.306 | 185 µs |
/// | 3 | 0.039 | 0.318 | 235 µs |
/// | **4** | **0.036** | **0.325** | **268 µs** |
/// | 5 | 0.036 | 0.325 | 342 µs |
/// | 6 | 0.036 | 0.318 | 385 µs |
///
/// Four is where the mean stops moving. The worst object is not the quantity
/// being minimised and does not fall monotonically with it — this is a greedy
/// descent on the energy-weighted sum, and which object ends up furthest out
/// is not something a sum decides.
pub const PASSES: usize = 4;

/// How far the first pass looks, in cube units.
///
/// | reach | mean | worst | flips a block |
/// |---|---|---|---|
/// | 0.05 | 0.036 | 0.318 | 2.47 |
/// | **0.1** | **0.036** | **0.325** | **2.78** |
/// | 0.2 | 0.037 | 0.328 | 2.80 |
/// | 0.3 | 0.038 | 0.332 | 4.71 |
/// | 0.5 | 0.039 | 0.330 | 5.31 |
///
/// A long first step is not a wider search, it is a worse one: it lands the
/// element in a basin the descent then has to climb back out of, and the flips
/// say so. A tenth of the cube costs the same as a fifth and beats it.
pub const REACH: f64 = 0.1;

/// What moving an element costs.
///
/// In the units of the score, which is energy times a squared relative error:
/// "how much error is worth one unit of `1 − cos` on everything the element
/// carries".
///
/// # It is zero, and it is written anyway
///
/// An element that moves takes every object folded into it along, including
/// the ones that did not move, and the metric cannot see that — it judges one
/// instant and never a movement. So a term for it belongs in the score on the
/// argument, and the argument is not a measurement. Measured, on `crowd40`
/// into twelve elements:
///
/// | cost of moving | mean | worst | travel a block | flips a block |
/// |---|---|---|---|---|
/// | **0** | **0.037** | **0.328** | **2.52°** | 2.80 |
/// | 0.005 | 0.037 | 0.328 | 2.52° | 2.86 |
/// | 0.02 | 0.037 | 0.328 | 2.52° | 2.86 |
/// | 0.1 | 0.037 | 0.328 | 2.53° | 3.12 |
/// | 0.5 | 0.037 | 0.338 | 2.57° | 4.30 |
///
/// It does nothing, and then it does harm. The reason is that the travel this
/// was meant to bound is **the scene turning**, not the search wandering: two
/// and a half degrees a block is where the objects went, and the search moves
/// an element by a fraction of that. On the master whose fold is the identity
/// it is the same story — 0.02° a block with the term at zero or at 0.02, and
/// 0.04° with a worst of 0.021 at 0.1, which is the term pulling elements away
/// from where the scene wants them.
///
/// Zero, then, and the term kept: the case it guards against — a search
/// wandering while the scene is still — is one no scene in the tree contains,
/// and "there is no fixture for it" is a reason to leave the mechanism where a
/// measurement can find it, not a reason to have none.
pub const MOVEMENT: f64 = 0.0;

/// Whether the element's dominant member is offered as a candidate.
///
/// An element sits where the metric is smallest, never exactly on its
/// loudest member; a dominant voice can be a degree or two from its place.
/// This is not a rule that snaps it there — it is one more candidate in the
/// descent, the position of the member carrying the most of the element's
/// energy, judged by the metric like the six steps.
///
/// Off, because the metric says it buys nothing: the same fold to a
/// thousandth on six scenes of eight, a fifth on the mean of `crowd40` at
/// sixteen for a sixth off its worst object, and seven per cent of the
/// clustering's time — see `docs/clustering.md`, which also records how far
/// a dominant member sits from its element either way: two to seven
/// degrees on average, which the search already puts it at. Kept compiled
/// and measurable from the harness.
pub const DOMINANT: bool = false;

/// How much better somewhere else has to be before an element goes there.
///
/// # The defect this is for
///
/// The metric judges one instant, so a fold that is right at every instant and
/// *different* at every instant scores perfectly. That is the fold this
/// encoder wrote: measured by [`crate::motion`] on a programme folded one
/// element short of its sources, an element left its place and came back
/// inside the ear's own integration window on 3.63 % of the windows it was
/// heard in, and more of the movement the stream carried was movement the mix
/// never asked for than movement it did. None of it costs anything the metric
/// can see, because at every instant the element is where the metric wants it.
///
/// # Why a dead band and not a cap
///
/// A cap on how far an element may move a block was built and measured and is
/// worse at the thing it is for — see `docs/clustering.md`. Capping an element
/// short of where it should be means the next block caps it again: one arrival
/// becomes a slide that never settles, so the travel goes *up*.
///
/// A dead band has no half-move to compound. The element either stays exactly
/// where it was or goes the whole way to the best place there is, and the
/// thing that decides is the metric's own number: staying is kept unless
/// moving pays by [`HOLDING`] of what staying costs *and* by
/// [`HOLDING_AT_LEAST`] per unit of what the element carries. That is the
/// shape [`crate::restart::WORTH`] and [`crate::earn::WORTH`] already have,
/// one stage further in, and the absolute floor is there for the same reason
/// it is there: on a fold that costs about nothing, one about-nothing is a
/// few per cent less than another on rounding alone.
/// # What it measured
///
/// The wobble of [`crate::motion`] — an element leaving its place and coming
/// back inside the ear's own integration window — on a programme folded one
/// element short of its sources, and on the three synthesised scenes at
/// thirteen elements. Mean error over the presentations and worst object
/// beside it, so that what the band costs is next to what it buys.
///
/// | holding | programme: wobble / error | `burst40`: wobble / error |
/// |---|---|---|
/// | **0**, as it was | 3.63 % / 0.000, 1.412 | 24.59 % / 0.050, 0.916 |
/// | 0.10 | 1.87 % / 0.000, 1.412 | 15.49 % / 0.052, 0.916 |
/// | 0.15 | 1.41 % / 0.000, 1.412 | 12.97 % / 0.051, 0.916 |
/// | 0.20 | 1.01 % / 0.000, 1.412 | 11.91 % / 0.051, 0.916 |
/// | **0.25** | **0.84 % / 0.000, 1.412** | **12.27 % / 0.051, 0.916** |
/// | 0.30 | 0.51 % / 0.000, 1.412 | 11.33 % / 0.051, 0.916 |
/// | 0.35 | 0.45 % / 0.000, **1.479** | — |
///
/// A quarter is not where the curve stops paying — three tenths takes the
/// programme's wobble to 0.51 % for the same error — it is one step back from
/// where the error moves, and a constant chosen on the last value before a
/// cliff is a constant the next programme walks off. What it buys at a
/// quarter: three quarters of the wobble, the share of what is playing that
/// sits in a wobbling element from 0.12 % to 0.03 %, and the movement the mix
/// never asked for from 1.44°/s to 1.20°/s. On `crowd40` it halves that last
/// figure for a thousandth of the mean, and on `broad40` the worst object
/// *improves*, 0.644 to 0.611.
///
/// # What it does not do, which the old counters say it does
///
/// The travel a block halves, 0.37° to 0.21°, and the jumps over fifteen
/// degrees nearly double, 47 to 81. Both are true and neither is the
/// quantity: an element held in place accumulates its discrepancy and then
/// goes the whole way at once, which is one jump where there were several
/// steps. On a ruler with a time axis it is not a jump at all — the windows
/// where an element genuinely went somewhere are 3.04 % without the band and
/// 2.64 % with it, so the same displacement arrives in fewer moves and no
/// more of it is noticed. That the two counters disagree is the argument
/// `docs/clustering-next.md` makes against them.
///
/// # What it costs, which is not error
///
/// Flips. An element held where it was is an element the fit has to route
/// objects through from a slightly stale place, so its active set changes
/// oftener: on the programme 102 flips become 166, and on the ring of
/// `an_object_between_elements_stops_trading_channels` 22 become 72. The band
/// moves instability out of the positions and some of it into the weights,
/// and the metric cannot say which of those a listener would rather have.
///
/// What it can say is that the two are not the same trade everywhere: on
/// `burst40` the flips *fall*, 806 to 752. And that the reach behind of
/// [`crate::smooth`] pays the cost back, because the two are fighting the same
/// block-to-block tremor from opposite ends — on the programme:
///
/// | | wobble | invented | flips |
/// |---|---|---|---|
/// | neither | 3.63 % | 1.44°/s | 102 |
/// | the band alone | 0.84 % | 1.20°/s | 166 |
/// | two blocks behind alone | 2.20 % | 1.32°/s | 96 |
/// | the band and two behind | 0.32 % | 1.18°/s | 132 |
/// | the band and four behind | **0.16 %** | 1.17°/s | **106** |
///
/// The band with four blocks behind is a twenty-third of the wobble for four
/// more flips than shipping neither, and the programme's worst object
/// improves, 1.412 to 1.333. It is not the default here because the window
/// costs `burst40` fourteen per cent of its mean and the band alone costs it
/// one; `harlettizer encode --smooth-behind` is the knob, and which of these
/// a programme wants is what lever 9's listening is for.
///
/// # What it costs to run
///
/// One more score an element a block, which is a seventh of a pass: the
/// clustering goes from 136 to 141 µs a block on the programme, four per
/// cent of a figure that is under a per cent of the encoder. A master whose
/// fold is the identity is untouched — 0.000 error, no wobble and the same
/// movement either way, because an element carrying one object is where that
/// object is and staying and moving are the same place.
pub const HOLDING: f64 = 0.25;

/// And how much in absolute terms, per unit of energy the element carries.
///
/// The difference is `Σ e_i · relative²` over the objects the element carries,
/// so dividing by what it carries gives a mean squared relative error. Below
/// this, moving every object an element holds is not worth what it does to
/// them.
pub const HOLDING_AT_LEAST: f64 = 1e-4;

/// Move each element to the best place the metric can find near it.
///
/// `targets[i]` is what object `i` radiates over the reference and
/// `radiated[k]` what element `k` at its current position does; both are the
/// caller's, and `radiated` is left holding the elements' final positions so
/// that the weights can be re-solved without panning them again.
///
/// Pinned elements do not move: a bed channel is a speaker feed and belongs at
/// that speaker, whatever the score says.
#[allow(clippy::too_many_arguments)]
pub fn refine(
    clustering: &mut Clustering,
    objects: &[Object],
    live: &[usize],
    reference: &Reference,
    targets: &[Vec<f64>],
    radiated: &mut [Vec<f64>],
    previous: Option<&Clustering>,
    search: Search,
    scratch: &mut Scratch,
) {
    if search.passes == 0 || live.is_empty() {
        return;
    }
    let rows = reference.rows();
    let pinned: Vec<bool> = (0..clustering.elements())
        .map(|element| {
            objects.iter().enumerate().any(|(object, source)| {
                source.pinned.is_some() && clustering.owners[object] == element
            })
        })
        .collect();

    let mut reach = search.reach;
    for pass in 0..search.passes {
        let last = pass + 1 == search.passes;
        for (slot, element) in live.iter().enumerate() {
            if pinned[*element] {
                continue;
            }
            // Which objects this element carries, and how much of each. An
            // element nothing reaches has nothing to be placed for.
            scratch.touched.clear();
            for (object, row) in clustering.weights.iter().enumerate() {
                if row[*element] != 0.0 {
                    scratch.touched.push((object, row[*element]));
                }
            }
            if scratch.touched.is_empty() {
                continue;
            }

            // What every *other* element already puts on the reference for
            // each of those objects, and how far off the target that leaves
            // them. Computed once; each candidate then adds one term.
            scratch.rest.clear();
            scratch.rest.resize(scratch.touched.len() * rows, 0.0);
            for (index, (object, _)) in scratch.touched.iter().enumerate() {
                let row = &clustering.weights[*object];
                let rest = &mut scratch.rest[index * rows..(index + 1) * rows];
                for (other, radiated) in live.iter().zip(radiated.iter()) {
                    let weight = row[*other];
                    if weight == 0.0 || other == element {
                        continue;
                    }
                    for (slot, value) in rest.iter_mut().zip(radiated) {
                        *slot += weight * value;
                    }
                }
            }

            let carried: f64 = scratch
                .touched
                .iter()
                .map(|(object, weight)| objects[*object].energy.max(0.0) * weight * weight)
                .sum();
            let was = previous
                .and_then(|previous| previous.positions.get(*element).copied())
                .unwrap_or(clustering.positions[*element]);

            let mut best = clustering.positions[*element];
            // Where it is now is already panned, so it is scored without
            // panning it again.
            let mut best_score = scored(
                &radiated[slot],
                &scratch.touched,
                &scratch.rest,
                targets,
                objects,
                rows,
            ) + search.movement * carried * apart(best, was);

            // One more candidate: where the member carrying the most of this
            // element sits. Judged like the others, not taken on trust.
            if search.dominant
                && let Some((dominant, _)) = scratch
                    .touched
                    .iter()
                    .map(|(object, weight)| {
                        (*object, objects[*object].energy.max(0.0) * weight * weight)
                    })
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            {
                let candidate = objects[dominant].position;
                if candidate != clustering.positions[*element] {
                    let cost = score(
                        candidate,
                        &scratch.touched,
                        &scratch.rest,
                        targets,
                        objects,
                        reference,
                        rows,
                        &mut scratch.candidate,
                    ) + search.movement * carried * apart(candidate, was);
                    if cost < best_score {
                        best_score = cost;
                        best = candidate;
                    }
                }
            }

            // Six candidates: a step along each axis of the cube, both ways.
            // The cube is the frame a fold reads, so it is the frame to search
            // in — a step along the sphere would be a step off the grid every
            // presentation is a row of.
            for axis in 0..3 {
                for direction in [-1.0f64, 1.0] {
                    let mut candidate = clustering.positions[*element];
                    candidate[axis] += direction * reach;
                    // The cube has no below and no outside.
                    candidate[axis] =
                        candidate[axis].clamp(if axis == 2 { 0.0 } else { -1.0 }, 1.0);
                    if candidate == clustering.positions[*element] {
                        continue;
                    }
                    let cost = score(
                        candidate,
                        &scratch.touched,
                        &scratch.rest,
                        targets,
                        objects,
                        reference,
                        rows,
                        &mut scratch.candidate,
                    ) + search.movement * carried * apart(candidate, was);
                    if cost < best_score {
                        best_score = cost;
                        best = candidate;
                    }
                }
            }

            // The dead band, once the search has finished looking. Staying
            // is not a candidate among the others — it is the answer unless
            // moving pays, which is not the same thing and is why it is
            // weighed here rather than scored above.
            if last
                && search.holding > 0.0
                && previous.is_some()
                && best != was
                && !scratch.touched.is_empty()
            {
                let staying = score(
                    was,
                    &scratch.touched,
                    &scratch.rest,
                    targets,
                    objects,
                    reference,
                    rows,
                    &mut scratch.candidate,
                );
                let pays = best_score < staying * (1.0 - search.holding)
                    && staying - best_score > HOLDING_AT_LEAST * carried;
                if !pays {
                    best = was;
                }
            }

            if best != clustering.positions[*element] {
                clustering.positions[*element] = best;
                reference.radiated(best, 0.0, &mut radiated[slot]);
            }
        }
        reach *= 0.5;
    }
}

/// What the metric would say about the objects this element carries, with it
/// at `position`.
#[allow(clippy::too_many_arguments)]
fn score(
    position: [f64; 3],
    touched: &[(usize, f64)],
    rest: &[f64],
    targets: &[Vec<f64>],
    objects: &[Object],
    reference: &Reference,
    rows: usize,
    candidate: &mut Vec<f64>,
) -> f64 {
    reference.radiated(position, 0.0, candidate);
    scored(candidate, touched, rest, targets, objects, rows)
}

/// The same, for a position that has already been panned.
fn scored(
    candidate: &[f64],
    touched: &[(usize, f64)],
    rest: &[f64],
    targets: &[Vec<f64>],
    objects: &[Object],
    rows: usize,
) -> f64 {
    let mut total = 0.0;
    for (index, (object, weight)) in touched.iter().enumerate() {
        let rest = &rest[index * rows..(index + 1) * rows];
        let target = &targets[*object];
        let mut difference = 0.0;
        let mut reference_length = 0.0;
        for row in 0..rows {
            let got = rest[row] + weight * candidate[row];
            let want = target[row];
            difference += (got - want) * (got - want);
            reference_length += want * want;
        }
        if reference_length > 1e-12 {
            total += objects[*object].energy.max(0.0) * difference / reference_length;
        }
    }
    total
}

/// `1 − cos` between two positions read as directions, which is how far an
/// element has moved as far as a listener is concerned.
fn apart(a: [f64; 3], b: [f64; 3]) -> f64 {
    let unit = |v: [f64; 3]| {
        let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if length < 1e-12 {
            [0.0, 1.0, 0.0]
        } else {
            [v[0] / length, v[1] / length, v[2] / length]
        }
    };
    let (a, b) = (unit(a), unit(b));
    1.0 - (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]).clamp(-1.0, 1.0)
}

/// The search's working buffers, held across blocks so a block costs none.
#[derive(Debug, Default)]
pub struct Scratch {
    touched: Vec<(usize, f64)>,
    rest: Vec<f64>,
    candidate: Vec<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clusterer, Weighting};

    /// A ring of objects at fixed places, each displaced by a small angle a
    /// block. Nothing moves, so anything the elements do is the fold
    /// answering noise.
    fn jittered(block: usize, jitter: f64) -> Vec<Object> {
        (0..24)
            .map(|index| {
                let home = index as f64 / 24.0 * std::f64::consts::TAU;
                // A displacement that is different every block and every
                // object, and always inside `jitter`, without a random
                // number generator in a test.
                let noise = ((index * 977 + block * 31) as f64).sin() * jitter.to_radians();
                let angle = home + noise;
                Object {
                    position: [angle.sin(), angle.cos(), 0.0],
                    energy: 1.0 + (index as f64) * 0.01,
                    size: 0.0,
                    pinned: None,
                    peak: 0.0,
                    mode: crate::Mode::default(),
                }
            })
            .collect()
    }

    /// How far the elements travel over a hundred blocks of a scene that only
    /// jitters, with the dead band and without.
    ///
    /// On the **positions**, which is what the search settles and what the
    /// stream carries. The directions are the clustering's own answer and
    /// [`refine`] does not touch them — see [`crate::Clustering::directions`]
    /// for why they are carried apart.
    fn travel(holding: f64) -> f64 {
        let mut clusterer = Clusterer::new(12, Weighting::Fitted)
            .expect("a clusterer")
            .searching(Search {
                holding,
                ..Search::default()
            });
        let mut previous: Option<crate::Clustering> = None;
        let mut travelled = 0.0;
        for block in 0..100 {
            let objects = jittered(block, 2.0);
            let clustering = clusterer
                .cluster(&objects, previous.as_ref())
                .expect("a fold");
            if let Some(before) = &previous {
                for (was, now) in before.positions.iter().zip(&clustering.positions) {
                    travelled += apart(*was, *now);
                }
            }
            previous = Some(clustering);
        }
        travelled
    }

    /// The scene the dead band is for: the objects are never anywhere else,
    /// so an element that moves is an element following the block's noise.
    #[test]
    fn a_scene_that_only_jitters_moves_the_elements_far_less() {
        let free = travel(0.0);
        let held = travel(HOLDING);
        assert!(
            held < free * 0.5,
            "the dead band left {held:.4} of travel against {free:.4} without it"
        );
    }

    /// And it is a dead band and not a freeze: the shipped constant lets an
    /// element go the whole way when going pays.
    ///
    /// What this used to open with — that the literal is between nought and
    /// one — was a test of the line above it and of nothing else. What is
    /// worth pinning is the behaviour at the other end: a band of one *is* a
    /// freeze, which is why the encoder refuses it.
    #[test]
    fn it_is_a_band_and_not_a_freeze() {
        // A scene that turns is followed, band or no band: every object moves
        // the same way at the same time, so staying costs what turning saves.
        let turn = |holding: f64| {
            let mut clusterer = Clusterer::new(12, Weighting::Fitted)
                .expect("a clusterer")
                .searching(Search {
                    holding,
                    ..Search::default()
                });
            let mut previous: Option<crate::Clustering> = None;
            let mut travelled = 0.0;
            for block in 0..40 {
                let objects: Vec<Object> = jittered(0, 0.0)
                    .into_iter()
                    .map(|object| {
                        let turn = (block as f64 * 3.0).to_radians();
                        let (x, y) = (object.position[0], object.position[1]);
                        Object {
                            position: [
                                x * turn.cos() + y * turn.sin(),
                                y * turn.cos() - x * turn.sin(),
                                object.position[2],
                            ],
                            ..object
                        }
                    })
                    .collect();
                let clustering = clusterer
                    .cluster(&objects, previous.as_ref())
                    .expect("a fold");
                if let Some(before) = &previous {
                    for (was, now) in before.positions.iter().zip(&clustering.positions) {
                        travelled += apart(*was, *now);
                    }
                }
                previous = Some(clustering);
            }
            travelled
        };
        let free = turn(0.0);
        let held = turn(HOLDING);
        assert!(
            held > free * 0.5,
            "a turning scene was followed {held:.4} against {free:.4}, which is a freeze"
        );
    }
}
