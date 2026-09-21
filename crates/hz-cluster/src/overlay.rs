//! Panning sources onto a scene whose elements are already placed.
//!
//! # The problem this is for
//!
//! A programme is re-voiced: the original's elements are kept exactly as they
//! are and a handful of new sources — a dubbed dialogue, one per channel of
//! the track it was taken from — have to join them. Handing the whole thing to
//! [`crate::Clusterer`] works and costs more than it should. The clusterer is
//! asked for the same number of elements the original had, the scene now has
//! more objects than that, so **every** element is re-placed and every one of
//! them is re-mixed and re-quantised: the untouched three quarters of the
//! programme comes out different because something was added to the other
//! quarter. Worse, a source at a place no element occupies pulls an element
//! towards it, so the original scene is *degraded* to make room for the
//! addition.
//!
//! # The answer: clustering with frozen centres
//!
//! Keep the elements where the master put them and pan the sources onto them,
//! treating the element positions as a time-varying loudspeaker set. That is
//! the per-object half of what the clusterer already does — an object is
//! panned onto the cloud of element positions and the weights are then fitted
//! by least squares against the delivery presentations, [`crate::Weighting`]'s
//! `Fitted` — with the recentring loop and the element renumbering removed,
//! because there is nothing to recentre and the numbering is the master's.
//!
//! What that buys, and all of it is checkable rather than argued:
//!
//! - the elements' own metadata is the master's, element for element;
//! - an element no source reaches carries its own audio untouched — not mixed,
//!   not rounded, the same samples it arrived as;
//! - the presentations fold the same metadata by the same functions as they
//!   would have for the original;
//! - the error is confined to the sources and is measurable per source, which
//!   is what [`drift`] is for.
//!
//! The common case costs nothing at all: a programme with a centre bed element
//! at `[0, 1, 0]` and a centre source asking for `[0, 1, 0]` puts weight one on
//! that element and zero everywhere else.
//!
//! # What it costs, and what is not claimed
//!
//! A source whose direction no element covers at some block is spread over the
//! nearest ones, and those may be moving. A static source carried by moving
//! elements **breathes** — it is rendered somewhere slightly different every
//! block although it never asked to move. That is the one real cost, it is
//! bounded by how far the elements are from where the source asked to be, and
//! it is why [`drift`] exists and why beds are preferred when they will do.
//!
//! The other cost is the **cadence**, and it is the encoder's rather than
//! this module's: a fold decided per block cannot state its elements oftener
//! than it decides them, so the master's trajectories are sampled once a block
//! where a plain encode samples them once an access unit. The content of what
//! is stated is the master's; the rate is not. On a bed, which is what most of
//! a dubbed programme's kept elements are, that is no difference at all.
//!
//! **Nothing here is measured on a programme yet.** The clusterer's numbers do
//! not transfer: they are about where elements are *put*, and this puts none.
//! What is claimed is the arithmetic above and the invariants the tests pin.

use crate::fit::{self, Held, Reference};
use crate::{Object, direction, dot};
use hz_core::Result;

/// One element a source may be panned onto.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Carrier {
    /// Which element this is, in the stream's own numbering. The weights come
    /// back over every element, so the caller never maps anything.
    pub element: usize,
    /// Where it is for this block, in the master's cube frame.
    ///
    /// Read as a direction, exactly as [`crate::Weighting::Spread`] reads one
    /// — see the note on the cube in `hz_render::panner`. A bed's is its
    /// speaker's place.
    pub position: [f64; 3],
    /// Whether it is a bed channel, which is what [`BEDS_FIRST`] prefers.
    pub bed: bool,
}

/// How far a fit confined to the bed elements may land from the source, as a
/// fraction of what the source radiates, before the moving elements are let
/// in.
///
/// # Why beds are preferred at all
///
/// A bed element is *still*. A dynamic one is wherever the mix put it this
/// block and somewhere else the next, so a static source carried by one is
/// rendered at a direction that wanders although the source never moved —
/// the breathing the module's note describes. A bed carrying it is a place
/// that holds. So where the beds can reach the source at all, they should have
/// it, even when a moving object happens to sit closer this block.
///
/// # Why an absolute reach and not a comparison
///
/// The obvious rule compares the two fits — take the beds when they are within
/// some fraction of the free answer, which is the shape [`fit::HELD_TOLERANCE`]
/// has, or make the beds an incumbent the free answer has to outbid, which is
/// [`crate::place::HOLDING`]'s. **Both fail at exactly the case this is for.**
/// A moving object that happens to sit on the source fits it *exactly*, so the
/// free residual is nought: no multiple of nought admits the beds, and nothing
/// outbids nought. The comparison stops working precisely where the source is
/// about to be handed to something that will have moved by the next block.
///
/// The question is not which fit is better, it is whether the beds can reach
/// the source **at all** — the note's "is the direction inside the bed hull",
/// asked in the fit's own currency. So it is an absolute reach, and it is
/// per source and per block.
///
/// # The value
///
/// What a bed-only fit costs is not a matter of opinion, and it separates by
/// about an order of magnitude. Over a 7.1 bed, as a fraction of what the
/// source radiates — this is `the_beds_reach_the_floor_and_not_the_ceiling`,
/// which is the test that keeps these numbers true:
///
/// | source | cost |
/// |---|---|
/// | on a bed: centre, the left corner, hard left, rear centre | 0.000 |
/// | between the left side and left rear | 0.061 |
/// | between left and the left side | 0.077 |
/// | half height at the left | 0.562 |
/// | overhead, front high, left high | 0.837–0.844 |
///
/// Everything the floor ring covers is under a tenth; anything with height is
/// over a half. **A quarter** is between them with room on both sides, which
/// is what a constant wants when the gap is this wide: one step back from
/// where it would start deciding anything. It is a flag on the encoder
/// nonetheless, because the gap is a property of a 7.1 bed and a programme
/// whose bed is something else has not been measured.
pub const BEDS_FIRST: f64 = 0.25;

/// Fits sources onto elements that are already placed.
///
/// Built once and reused for every block: the reference's triangulation is the
/// expensive part and none of it depends on the scene. Every buffer it needs
/// is its own and is refilled rather than reallocated, so a block costs no
/// allocation once the shapes have settled.
pub struct Overlay {
    reference: Reference,
    beds_first: Option<f64>,
    /// What each carrier radiates over the reference, one vector per carrier,
    /// and the position each was computed for so that a carrier which has not
    /// moved is not panned again.
    radiated: Vec<Vec<f64>>,
    panned_at: Vec<Option<[f64; 3]>>,
    /// What the source being fitted radiates.
    target: Vec<f64>,
    /// The previous block's row for this source, over this block's carriers.
    anchor: Vec<f64>,
    /// Which carriers are beds, over the carriers rather than the elements.
    beds: Vec<bool>,
    /// The fit confined to the beds, tried first, and the free one, solved
    /// only when the beds could not reach the source.
    onbeds: Vec<f64>,
    free: Vec<f64>,
    /// What a set of weights reaches, for the residual that judges it.
    got: Vec<f64>,
}

impl Overlay {
    /// Fitted against the presentations a delivery stream is played through,
    /// which is what [`crate::metric`] measures and so what the fit should
    /// optimise. See [`Reference::new`].
    pub fn new() -> Result<Self> {
        Ok(Self::over(Reference::new()?))
    }

    pub fn over(reference: Reference) -> Self {
        Self {
            reference,
            beds_first: Some(BEDS_FIRST),
            radiated: Vec::new(),
            panned_at: Vec::new(),
            target: Vec::new(),
            anchor: Vec::new(),
            beds: Vec::new(),
            free: Vec::new(),
            onbeds: Vec::new(),
            got: Vec::new(),
        }
    }

    /// How much better a free fit has to be before the moving elements are let
    /// in; `None` declines the preference and fits over everything. See
    /// [`BEDS_FIRST`].
    pub fn preferring(mut self, beds_first: Option<f64>) -> Self {
        self.beds_first = beds_first;
        self
    }

    /// Fit `sources` onto `carriers` for one block.
    ///
    /// `out[source][element]` comes back in linear amplitude over all
    /// `elements`, zero for every element that is not a carrier — the same
    /// shape as `previous`, so a caller holds two of these and swaps them.
    ///
    /// `previous` is the last block's answer, over the same elements. It is
    /// used the way the clusterer uses it: to keep a source in the elements it
    /// was already in when they are nearly as good, so that its active set
    /// does not change over a difference too small to hear. See [`fit::Held`].
    ///
    /// With no carriers every row comes back zero, which is a source with
    /// nowhere to go. That is a condition for the caller to notice and act on,
    /// not one to fail on: it is decided per block and a block is 27 ms.
    pub fn fit(
        &mut self,
        elements: usize,
        carriers: &[Carrier],
        sources: &[Object],
        previous: Option<&[Vec<f64>]>,
        out: &mut Vec<Vec<f64>>,
    ) {
        out.resize_with(sources.len(), Vec::new);
        for row in out.iter_mut() {
            row.clear();
            row.resize(elements, 0.0);
        }
        if carriers.is_empty() || sources.is_empty() {
            return;
        }

        // What each carrier radiates, once: the same for every source, and
        // the expensive half — a panning onto every channel of every
        // presentation the stream is played through.
        //
        // **And most carriers never move.** A dub's elements are mostly a bed,
        // which is at its speaker's place in the first block and in the last;
        // re-panning it forty times a second for the length of a feature
        // computes the same vector over and over. So the position each entry
        // was computed for is kept beside it, and one that has not moved is
        // left alone. An element that does move is panned again, as it must
        // be; nothing is approximated here, only skipped.
        self.radiated.resize_with(carriers.len(), Vec::new);
        self.panned_at.resize(carriers.len(), None);
        for ((carrier, into), was) in carriers
            .iter()
            .zip(self.radiated.iter_mut())
            .zip(self.panned_at.iter_mut())
        {
            if *was == Some(carrier.position) && !into.is_empty() {
                continue;
            }
            self.reference.radiated(carrier.position, 0.0, into);
            *was = Some(carrier.position);
        }
        self.beds.clear();
        self.beds.extend(carriers.iter().map(|carrier| carrier.bed));
        // The preference is only a choice where there is something to choose
        // between: all beds or no beds, and the free fit is the only fit.
        let prefer = self
            .beds_first
            .filter(|_| self.beds.iter().any(|bed| *bed) && self.beds.iter().any(|bed| !*bed));

        for (index, (source, row)) in sources.iter().zip(out.iter_mut()).enumerate() {
            self.reference
                .radiated(source.position, source.size, &mut self.target);

            // The elements this source was in last block, over this block's
            // carriers. Which of them are non-zero is the set; how large they
            // were is not used. The rows are the sources in the order they
            // were given, and they are given in that order every block —
            // they are channels of the input — so the index is the identity.
            let anchored = previous
                .and_then(|previous| previous.get(index))
                .filter(|row| row.len() == elements);
            let held = match anchored {
                Some(previous) => {
                    self.anchor.clear();
                    self.anchor
                        .extend(carriers.iter().map(|carrier| previous[carrier.element]));
                    Some(Held {
                        previous: &self.anchor,
                        tolerance: fit::HELD_TOLERANCE,
                    })
                }
                None => None,
            };

            // The beds are tried first and the moving elements are only let in
            // when the beds cannot reach the source. Where they can — which is
            // the common case, since a dub's voices sit at speaker places —
            // the free fit is never solved at all. See [`BEDS_FIRST`].
            let taken = match prefer {
                Some(reach) => {
                    self.onbeds =
                        fit::fitted_near(&self.radiated, &self.target, held, Some(&self.beds));
                    let level = self.target.iter().map(|v| v * v).sum::<f64>().sqrt();
                    let cost = residual(&self.radiated, &self.target, &self.onbeds, &mut self.got);
                    if cost <= reach * level {
                        &self.onbeds
                    } else {
                        self.free = fit::fitted_near(&self.radiated, &self.target, held, None);
                        &self.free
                    }
                }
                None => {
                    self.free = fit::fitted_near(&self.radiated, &self.target, held, None);
                    &self.free
                }
            };
            for (carrier, weight) in carriers.iter().zip(taken) {
                row[carrier.element] = *weight;
            }
        }
    }
}

/// How wide a source came out: how many elements carry it, and how much the
/// strongest of them has.
///
/// Two numbers because they answer different questions. A source on **one**
/// element is rendered from one place and is as sharp as the format allows; a
/// source shared between three is rendered from three, and paths a wide layout
/// keeps apart arrive together on a narrow one. But the count alone says
/// nothing — two elements at 0.99 and 0.01 is a count of two and a sharp
/// image — so what matters beside it is how much the strongest one holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spread {
    /// Elements carrying more than [`REACHED`] of the source.
    pub paths: usize,
    /// The largest weight, as a fraction of the source's own level.
    pub strongest: f64,
}

/// What counts as an element carrying a source at all.
///
/// A tenth of the amplitude is a fortieth of the power and about twenty
/// decibels down: below that an element is a path the fit happened to leave
/// open rather than one a listener is hearing the source through.
pub const REACHED: f64 = 0.1;

/// How wide a source came out — see [`Spread`].
///
/// Measured against the source's own level rather than against unity, because
/// the fit puts the level back after it chooses the set: a quiet source spread
/// over one element has a weight well under one and is not spread at all.
pub fn spread(weights: &[f64]) -> Spread {
    let level = weights.iter().map(|w| w * w).sum::<f64>().sqrt();
    let scale = if level > 1e-12 { 1.0 / level } else { 0.0 };
    let mut paths = 0;
    let mut strongest = 0.0f64;
    for weight in weights {
        let share = weight.abs() * scale;
        if share > REACHED {
            paths += 1;
        }
        strongest = strongest.max(share);
    }
    Spread { paths, strongest }
}

/// The bed element nearest a direction, for a source the fit could not place
/// acceptably — see the encoder's `--overlay-fallback`.
///
/// Nearest by angle and not by the fit, because the whole point of a fallback
/// is that the fit is what went wrong. A source put on one bed outright is
/// slightly misplaced and perfectly sharp, which is the trade a fallback is
/// for: audible and a little off beats diffuse and wandering.
pub fn nearest_bed(carriers: &[Carrier], position: [f64; 3]) -> Option<usize> {
    let here = direction(position);
    carriers
        .iter()
        .enumerate()
        .filter(|(_, carrier)| carrier.bed)
        .max_by(|(_, a), (_, b)| {
            dot(direction(a.position), here)
                .partial_cmp(&dot(direction(b.position), here))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(slot, _)| slot)
}

/// How far a set of weights puts a source from where it asked to be, in
/// degrees, or `None` when they put it nowhere at all.
///
/// The direction the carriers actually produce is taken as the **energy**
/// vector — the carriers' directions weighted by the square of their weights —
/// because preserving the energy-weighted direction is the property this whole
/// approach rests on (see the note at the top of [`crate`]), and the honest
/// guard on a claim is a measurement of the quantity claimed.
///
/// Both directions are read from the cube the same way the panner reads one,
/// so a corner is a corner in both and the angle is between two things in the
/// same frame.
pub fn drift(asked: [f64; 3], carriers: &[Carrier], weights: &[f64]) -> Option<f64> {
    let got = resultant(carriers, weights)?;
    let cosine = dot(direction(asked), got).clamp(-1.0, 1.0);
    Some(cosine.acos().to_degrees())
}

/// The direction a set of weights puts a source at: the energy vector of the
/// carriers it reaches, as a unit vector. `None` when every weight is zero, or
/// when the carriers cancel to nothing.
pub fn resultant(carriers: &[Carrier], weights: &[f64]) -> Option<[f64; 3]> {
    let mut sum = [0.0f64; 3];
    let mut power = 0.0;
    for (carrier, weight) in carriers.iter().zip(weights) {
        if *weight == 0.0 {
            continue;
        }
        let energy = weight * weight;
        let at = direction(carrier.position);
        for axis in 0..3 {
            sum[axis] += energy * at[axis];
        }
        power += energy;
    }
    if power <= 0.0 {
        return None;
    }
    let length = dot(sum, sum).sqrt();
    if length < 1e-12 {
        return None;
    }
    Some([sum[0] / length, sum[1] / length, sum[2] / length])
}

/// How far a set of weights lands from the target, as a length in the
/// reference's own units. `got` is scratch the caller owns.
fn residual(radiated: &[Vec<f64>], target: &[f64], weights: &[f64], got: &mut Vec<f64>) -> f64 {
    got.clear();
    got.resize(target.len(), 0.0);
    for (column, weight) in radiated.iter().zip(weights) {
        if *weight == 0.0 {
            continue;
        }
        for (slot, value) in got.iter_mut().zip(column) {
            *slot += weight * value;
        }
    }
    got.iter()
        .zip(target)
        .map(|(got, want)| (got - want) * (got - want))
        .sum::<f64>()
        .sqrt()
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use hz_render::fold::bed_position;

    /// A 7.1.2 bed as a master carries one, the LFE left out.
    pub(crate) fn bed_712() -> Vec<Carrier> {
        ["L", "R", "C", "Lss", "Rss", "Lrs", "Rrs", "Lts", "Rts"]
            .iter()
            .enumerate()
            .map(|(element, label)| Carrier {
                element,
                position: bed_position(label).expect("a bed channel"),
                bed: true,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::bed_712;
    use super::*;
    use hz_render::{Mode, fold::bed_position};

    /// A 7.1 bed as a master carries one: seven elements at their speakers'
    /// places in the cube, the low frequency channel left out because nothing
    /// is ever panned onto it.
    fn bed_carriers() -> Vec<Carrier> {
        ["L", "R", "C", "Lss", "Rss", "Lrs", "Rrs"]
            .iter()
            .enumerate()
            .map(|(element, label)| Carrier {
                element,
                position: bed_position(label)
                    .unwrap_or_else(|| panic!("{label} has a place in the cube")),
                bed: true,
            })
            .collect()
    }

    fn source_at(position: [f64; 3]) -> Object {
        Object {
            position,
            energy: 1.0,
            size: 0.0,
            pinned: None,
            peak: 0.0,
            mode: Mode::default(),
        }
    }

    /// The common case, and the one the whole approach is for: the programme
    /// has a centre bed element, the dub's centre asks for the centre, and the
    /// answer is one weight of one. Nothing else is touched, so nothing else
    /// is mixed or rounded, and the source is rendered exactly where it asked.
    #[test]
    fn a_centre_source_lands_on_the_centre_element_and_touches_nothing_else() {
        let carriers = bed_carriers();
        let centre = carriers
            .iter()
            .position(|carrier| carrier.position == bed_position("C").expect("C"))
            .expect("the bed has a centre");
        let sources = [source_at([0.0, 1.0, 0.0])];
        let mut overlay = Overlay::new().expect("a reference over the presentations");
        let mut weights = Vec::new();
        overlay.fit(carriers.len(), &carriers, &sources, None, &mut weights);

        let row = &weights[0];
        assert!(
            (row[carriers[centre].element] - 1.0).abs() < 1e-6,
            "the centre took {}, not one",
            row[carriers[centre].element]
        );
        for (element, weight) in row.iter().enumerate() {
            if element != carriers[centre].element {
                assert!(
                    *weight < 1e-6,
                    "element {element} took {weight} of a centre"
                );
            }
        }
        let drift = drift(sources[0].position, &carriers, row).expect("it went somewhere");
        assert!(drift < 1e-6, "the centre drifted {drift}°");
    }

    /// With no carriers at all — every element muted or inactive — the answer
    /// is zero everywhere rather than a failure. It is the caller's to notice:
    /// what to do about a source with nowhere to go is a decision about a
    /// programme and not about arithmetic.
    #[test]
    fn a_source_with_no_carrier_is_given_no_weight_rather_than_refused() {
        let sources = [source_at([0.0, 1.0, 0.0])];
        let mut overlay = Overlay::new().expect("a reference");
        let mut weights = Vec::new();
        overlay.fit(7, &[], &sources, None, &mut weights);
        assert_eq!(weights.len(), 1);
        assert_eq!(weights[0].len(), 7);
        assert!(weights[0].iter().all(|weight| *weight == 0.0));
    }

    /// A source the beds reach goes to the beds even when a moving object sits
    /// *exactly* on it, because a bed is a place that holds still and a moving
    /// object is where the mix put it this block and somewhere else the next.
    ///
    /// The source is put between two beds and the object on top of it, which
    /// is the case a tolerance cannot decide: the object's fit is exact, so
    /// any multiple of its residual is still nought. See [`BEDS_FIRST`].
    #[test]
    fn a_source_the_beds_reach_is_not_given_to_a_moving_object_sitting_on_it() {
        let mut carriers = bed_carriers();
        // Between the left side surround at `[-1, 0, 0]` and the left rear at
        // `[-1, -1, 0]`, which is a direction the bed covers.
        let between = [-1.0, -0.5, 0.0];
        carriers.push(Carrier {
            element: carriers.len(),
            position: between,
            bed: false,
        });
        let elements = carriers.len();
        let moving = carriers.last().expect("the object").element;
        let sources = [source_at(between)];

        let mut preferring = Overlay::new().expect("a reference");
        let mut kept = Vec::new();
        preferring.fit(elements, &carriers, &sources, None, &mut kept);
        assert!(
            kept[0][moving] < 1e-6,
            "the moving object took {} of a source the beds reach",
            kept[0][moving]
        );
        let carried: f64 = kept[0].iter().map(|weight| weight * weight).sum();
        assert!(
            carried > 0.5,
            "and the beds carry it, at a power of {carried}"
        );

        // With the preference declined it is a plain fit, which does take the
        // object, since it sits exactly on the source. So the preference is
        // what makes the difference and not something about the geometry.
        let mut plain = Overlay::new().expect("a reference").preferring(None);
        let mut free = Vec::new();
        plain.fit(elements, &carriers, &sources, None, &mut free);
        assert!(
            free[0][moving] > 0.5,
            "without the preference the element on the source should win, not take {}",
            free[0][moving]
        );
    }

    /// A source the beds cannot reach is not forced onto them. Overhead is the
    /// case: no floor bed radiates anything like a source at the ceiling, so
    /// the bed-only fit is many times worse and loses on its own — which is
    /// the property that does not depend on what [`BEDS_FIRST`] is set to.
    #[test]
    fn a_source_the_beds_cannot_reach_takes_the_element_that_can() {
        let mut carriers = bed_carriers();
        carriers.push(Carrier {
            element: carriers.len(),
            position: [0.0, 0.0, 1.0],
            bed: false,
        });
        let elements = carriers.len();
        let overhead = carriers.last().expect("the object").element;
        let sources = [source_at([0.0, 0.0, 1.0])];

        let mut overlay = Overlay::new().expect("a reference");
        let mut weights = Vec::new();
        overlay.fit(elements, &carriers, &sources, None, &mut weights);
        assert!(
            weights[0][overhead] > 0.5,
            "the overhead element took only {} of an overhead source",
            weights[0][overhead]
        );
    }

    /// Every source keeps the level it asked for: the fit puts the level back
    /// after the set is chosen, so a source is neither turned down for having
    /// been panned nor added to.
    #[test]
    fn a_source_arrives_at_the_level_it_asked_for() {
        let carriers = bed_carriers();
        let sources = [
            source_at([0.0, 1.0, 0.0]),
            source_at([-0.577, 1.0, 0.0]),
            source_at([-1.0, -1.0, 0.0]),
        ];
        let mut overlay = Overlay::new().expect("a reference");
        let mut weights = Vec::new();
        overlay.fit(carriers.len(), &carriers, &sources, None, &mut weights);
        for (index, row) in weights.iter().enumerate() {
            let power: f64 = row.iter().map(|weight| weight * weight).sum();
            assert!(
                power > 0.5 && power < 2.0,
                "source {index} came to a power of {power}"
            );
        }
    }

    /// The energy vector of a single carrier is that carrier's own direction,
    /// and of two equal ones the direction between them. The guard that
    /// measures where a source ended up has to agree with the geometry before
    /// it can be believed about a programme.
    #[test]
    fn the_resultant_of_one_carrier_is_that_carrier() {
        let carriers = bed_carriers();
        let mut weights = vec![0.0; carriers.len()];
        weights[2] = 1.0;
        let got = resultant(&carriers, &weights).expect("one carrier");
        let want = direction(carriers[2].position);
        assert!(dot(got, want) > 1.0 - 1e-9, "{got:?} against {want:?}");
        assert_eq!(resultant(&carriers, &vec![0.0; carriers.len()]), None);
    }
    /// What a bed-only fit costs at a spread of directions, which is where
    /// [`BEDS_FIRST`] gets its value. The floor ring is reached for under a
    /// tenth of what the source radiates and anything with height costs over a
    /// half, so the constant has an order of magnitude to sit in — and this is
    /// the test that says so if that ever stops being true.
    #[test]
    fn the_beds_reach_the_floor_and_not_the_ceiling() {
        let carriers = bed_carriers();
        let on_the_floor: [[f64; 3]; 8] = [
            [0.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            [-1.0, -0.5, 0.0],
            [-1.0, 0.5, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 0.5, 0.0],
        ];
        let off_it: [[f64; 3]; 4] = [
            [-1.0, 0.0, 0.5],
            [0.0, 0.0, 1.0],
            [0.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ];
        let mut overlay = Overlay::new().expect("a reference");
        let mut radiated: Vec<Vec<f64>> = vec![Vec::new(); carriers.len()];
        for (carrier, into) in carriers.iter().zip(radiated.iter_mut()) {
            overlay.reference.radiated(carrier.position, 0.0, into);
        }
        let beds: Vec<bool> = carriers.iter().map(|carrier| carrier.bed).collect();
        let cost = |position: [f64; 3], overlay: &mut Overlay| -> f64 {
            let mut target = Vec::new();
            overlay.reference.radiated(position, 0.0, &mut target);
            let weights = fit::fitted_near(&radiated, &target, None, Some(&beds));
            let mut got = Vec::new();
            let level: f64 = target.iter().map(|v| v * v).sum::<f64>().sqrt();
            residual(&radiated, &target, &weights, &mut got) / level
        };

        for position in on_the_floor {
            let cost = cost(position, &mut overlay);
            assert!(
                cost < BEDS_FIRST,
                "the beds should reach {position:?}, and it cost {cost:.4}"
            );
        }
        for position in off_it {
            let cost = cost(position, &mut overlay);
            assert!(
                cost > BEDS_FIRST,
                "the beds should not reach {position:?}, and it cost {cost:.4}"
            );
        }
    }

    /// A source keeps the elements it was already in when they are nearly as
    /// good, so that its active set does not change over a difference too
    /// small to hear. An element going from something to nothing is a
    /// crossfade in the mix; doing it every block is the flipping
    /// [`fit::HELD_TOLERANCE`] exists to damp, and overlay gets it for free by
    /// handing the last block's answer back.
    #[test]
    fn a_source_keeps_the_elements_it_had_when_they_are_nearly_as_good() {
        let mut carriers = bed_carriers();
        // Two objects either side of a source that sits between them, so that
        // which pair carries it is a genuine choice and a small move settles
        // it either way.
        carriers.push(Carrier {
            element: carriers.len(),
            position: [-0.2, 1.0, 0.6],
            bed: false,
        });
        carriers.push(Carrier {
            element: carriers.len(),
            position: [0.2, 1.0, 0.6],
            bed: false,
        });
        let elements = carriers.len();
        let sources = [source_at([0.0, 1.0, 0.6])];

        let mut overlay = Overlay::new().expect("a reference").preferring(None);
        let mut first = Vec::new();
        overlay.fit(elements, &carriers, &sources, None, &mut first);
        let was: Vec<usize> = (0..elements).filter(|e| first[0][*e] != 0.0).collect();
        assert!(!was.is_empty(), "the source went somewhere");

        // The same scene again, handed its own answer: the set does not move.
        let mut again = Vec::new();
        overlay.fit(elements, &carriers, &sources, Some(&first), &mut again);
        let now: Vec<usize> = (0..elements).filter(|e| again[0][*e] != 0.0).collect();
        assert_eq!(was, now, "the set changed with nothing to change it");
    }
    /// What the spread guard counts: elements carrying a real share of the
    /// source, and how much the strongest of them holds. A count alone is not
    /// the question — two elements at 0.99 and 0.01 is a sharp image with a
    /// count of two — which is why both come back together.
    #[test]
    fn how_wide_a_source_came_out_is_two_numbers_and_not_one() {
        let alone = spread(&[0.0, 1.0, 0.0]);
        assert_eq!(alone.paths, 1);
        assert!((alone.strongest - 1.0).abs() < 1e-9);

        // Nearly all on one: a count of two, and sharp.
        let nearly = spread(&[0.99, 0.01, 0.0]);
        assert_eq!(nearly.paths, 1, "a hundredth is not a path");
        assert!(nearly.strongest > 0.99);

        // Genuinely shared over three: diffuse, and the strongest holds well
        // under the half the encoder refuses at.
        let shared = spread(&[0.577, 0.577, 0.577]);
        assert_eq!(shared.paths, 3);
        assert!(
            shared.strongest < 0.6,
            "the strongest held {}",
            shared.strongest
        );

        // Measured against the source's own level, not against unity, since
        // the fit puts the level back after choosing the set: a quiet source
        // on one element is not spread.
        let quiet = spread(&[0.0, 0.05, 0.0]);
        assert_eq!(quiet.paths, 1);
        assert!((quiet.strongest - 1.0).abs() < 1e-9);
        assert_eq!(spread(&[0.0, 0.0]).paths, 0);
    }

    /// The fallback takes the bed nearest the source by angle, and never a
    /// moving element however close it is this block — the whole point being
    /// that the fit is what went wrong and a bed is what holds still.
    #[test]
    fn the_fallback_takes_the_nearest_bed_and_never_a_moving_element() {
        let mut carriers = bed_carriers();
        let rear_left = bed_position("Lrs").expect("Lrs");
        carriers.push(Carrier {
            element: carriers.len(),
            position: rear_left,
            bed: false,
        });
        let moving = carriers.len() - 1;

        let slot = nearest_bed(&carriers, [-1.0, -0.9, 0.0]).expect("a bed");
        assert_ne!(slot, moving, "a moving element is not a fallback");
        assert_eq!(carriers[slot].position, rear_left);

        // Straight ahead goes to the centre.
        let slot = nearest_bed(&carriers, [0.0, 1.0, 0.0]).expect("a bed");
        assert_eq!(carriers[slot].position, bed_position("C").expect("C"));

        // And with no beds at all there is nowhere to fall back to, which the
        // encoder counts as a source that went nowhere.
        let moving_only: Vec<Carrier> = carriers
            .iter()
            .map(|carrier| Carrier {
                bed: false,
                ..*carrier
            })
            .collect();
        assert_eq!(nearest_bed(&moving_only, [0.0, 1.0, 0.0]), None);
    }
    /// **The guards have to agree.** A source the bed can reach must cost
    /// nothing on any of them; one it cannot must fail all of them together,
    /// so that a refusal is never a quarrel between two rulers.
    ///
    /// Measured over both beds a programme brings and twelve positions, the
    /// eight of the floor ring and four with height:
    ///
    /// | bed | source | drift | cost | level |
    /// |---|---|---|---|---|
    /// | 7.1 and 7.1.2 | any of the eight floor places | 0.0° | 0.000 | 0.00 dB |
    /// | 7.1.2 | side at half height | 8.2° | 0.208 | −1.90 dB |
    /// | 7.1.2 | overhead | 0.6° | 0.100 | 0.12 dB |
    /// | 7.1.2 | front high | 31.4° | 1.044 | 2.35 dB |
    /// | 7.1 | overhead | 90.0° | 1.322 | −1.26 dB |
    ///
    /// A source **at a speaker's place costs exactly nothing** — it lands on
    /// that element with a weight of one — which is the case a dub is, since
    /// its voices are the channels of a track. What a bed cannot reach costs
    /// four times the `0.5` the encoder refuses at, and drift says so at the
    /// same time. There is no gap in between on anything measured here, which
    /// is why the encoder's bound sits where it does.
    #[test]
    fn what_the_bed_reaches_costs_nothing_and_what_it_cannot_fails_every_guard() {
        let renderers = crate::metric::delivery().expect("the presentations");
        let floor: [[f64; 3]; 8] = [
            [0.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [0.0, -1.0, 0.0],
        ];
        for carriers in [bed_carriers(), bed_712()] {
            let mut overlay = Overlay::new().expect("a reference");
            for position in floor {
                let (drifted, cost, level) = judge(&mut overlay, &carriers, position, &renderers);
                assert!(
                    drifted < 1e-6 && cost < 1e-6 && level.abs() < 1e-6,
                    "a source at the speaker place {position:?} should cost nothing, and cost \
                     {cost:.3} at {drifted:.1}° and {level:.2} dB"
                );
            }
            // And wherever the **cost** passes, so does the drift. That is
            // the implication that holds, and it is the one worth having: a
            // refusal is never the cost being lenient where the drift is
            // strict. The converse is false, deliberately — see
            // `the_drift_alone_cannot_see_a_source_spread_over_the_wrong_pair`.
            for position in [
                [0.0, 0.0, 1.0],
                [0.0, 1.0, 1.0],
                [-1.0, 1.0, 1.0],
                [-1.0, 0.0, 0.5],
                [-0.365, -0.931, 0.833],
                [-0.623, 0.782, 0.5],
            ] {
                let (drifted, cost, _) = judge(&mut overlay, &carriers, position, &renderers);
                if cost < 0.5 {
                    assert!(
                        drifted < 15.0,
                        "{position:?} costs {cost:.3} but drifted {drifted:.1}°"
                    );
                }
            }
        }
    }

    /// What the three guards make of one static source: its drift in degrees,
    /// the worst its fold costs over the presentations, and the furthest its
    /// rendered level strays on any of them, in decibels.
    fn judge(
        overlay: &mut Overlay,
        carriers: &[Carrier],
        position: [f64; 3],
        renderers: &[Box<dyn hz_render::Renderer>],
    ) -> (f64, f64, f64) {
        let sources = [source_at(position)];
        let mut weights = Vec::new();
        overlay.fit(carriers.len(), carriers, &sources, None, &mut weights);
        let row: Vec<f64> = carriers
            .iter()
            .map(|carrier| weights[0][carrier.element])
            .collect();
        let judged = crate::Clustering {
            positions: carriers.iter().map(|carrier| carrier.position).collect(),
            directions: Vec::new(),
            weights: vec![row.clone()],
            owners: Vec::new(),
            modes: Vec::new(),
            bounded: 0,
        };
        let report = crate::metric::error_with(
            &sources,
            &judged,
            renderers,
            &crate::floor::Floor::relative_only(),
        )
        .expect("a report");
        let strayed = report
            .layouts
            .iter()
            .map(|layer| {
                if layer.energy > 1e-6 {
                    20.0 * layer.energy.log10()
                } else {
                    0.0
                }
            })
            .fold(0.0f64, |m, db| if db.abs() > m.abs() { db } else { m });
        (
            drift(position, carriers, &row).unwrap_or(f64::NAN),
            report.worst,
            strayed,
        )
    }
    /// A carrier that has not moved is not panned again, and one that has
    /// **is**. The cache is the one place in here where a wrong answer would
    /// be silent — a stale vector fits the source to where the element used
    /// to be — so what it is checked against is the same fit with no history
    /// at all.
    #[test]
    fn a_carrier_that_moved_is_panned_again_and_one_that_did_not_is_not() {
        let mut carriers = bed_carriers();
        carriers.push(Carrier {
            element: carriers.len(),
            position: [0.2, 1.0, 0.4],
            bed: false,
        });
        let elements = carriers.len();
        let sources = [source_at([0.1, 1.0, 0.3])];

        let mut held = Overlay::new().expect("a reference").preferring(None);
        let mut first = Vec::new();
        held.fit(elements, &carriers, &sources, None, &mut first);

        // Move the one dynamic carrier a long way and fit again, with the
        // cache warm from the block before.
        carriers.last_mut().expect("the object").position = [-0.9, -0.9, 0.0];
        let mut second = Vec::new();
        held.fit(elements, &carriers, &sources, Some(&first), &mut second);

        // The same block from cold: no cache, nothing to go stale.
        let mut cold = Overlay::new().expect("a reference").preferring(None);
        let mut fresh = Vec::new();
        cold.fit(elements, &carriers, &sources, Some(&first), &mut fresh);

        for element in 0..elements {
            assert!(
                (second[0][element] - fresh[0][element]).abs() < 1e-12,
                "element {element}: the cache gave {} where a cold fit gives {}",
                second[0][element],
                fresh[0][element]
            );
        }
        assert!(
            second[0].iter().any(|w| *w != 0.0),
            "the source went nowhere"
        );
    }
    /// **The drift alone is not enough, and these are the positions that
    /// prove it.** Pinned by value, because the next person to read the cost
    /// guard will see it refuse something the drift is happy with and take it
    /// for a bug.
    ///
    /// A source front-left and half-way up, over a 7.1.2 bed whose only height
    /// is at the **sides**. The fit builds it from the front floor speaker and
    /// the side height one; those straddle the source, so their energy vector
    /// — which is all [`drift`] measures — averages to very nearly the right
    /// direction, while what they *radiate* is nothing like what the source
    /// radiates, which only a layout with front height can show.
    ///
    /// | | drift | level | 2.0 | 5.1 | 7.1 | 7.1.4 |
    /// |---|---|---|---|---|---|---|
    /// | `[-0.623, 0.782, 0.5]` | 12.2° | +0.30 dB | 0.11 | 0.29 | 0.29 | **1.04** |
    /// | `[-0.365, -0.931, 0.833]` | 22.8° | −0.99 dB | 0.22 | 0.22 | 0.55 | **0.98** |
    ///
    /// And **not** because of a small denominator, which is the first thing
    /// anyone suspects of a relative error. The own norms on 7.1.4 are 1.0000
    /// for both — the largest of the four presentations — and on 7.1 the
    /// second position's is 0.8419. Both cost most where they radiate most.
    #[test]
    fn the_drift_alone_cannot_see_a_source_spread_over_the_wrong_pair() {
        let renderers = crate::metric::delivery().expect("the presentations");
        let carriers = bed_712();
        let mut overlay = Overlay::new().expect("a reference");
        for position in [[-0.623, 0.782, 0.5], [-0.365, -0.931, 0.833]] {
            let (_, cost, level) = judge(&mut overlay, &carriers, position, &renderers);
            assert!(
                cost > 0.5,
                "{position:?} is a counterexample and no longer costs anything: {cost:.3}"
            );
            assert!(
                level.abs() < 1.0,
                "{position:?} should arrive at about the right level, and strayed {level:.2} dB"
            );
        }
        // The first is the sharp case: the drift passes its own bound outright
        // while the cost refuses.
        let (drifted, cost, level) =
            judge(&mut overlay, &carriers, [-0.623, 0.782, 0.5], &renderers);
        assert!(
            drifted < 15.0 && level.abs() < 1.0 && cost > 0.5,
            "the counterexample has moved: drift {drifted:.1}°, level {level:.2} dB, \
             cost {cost:.3}"
        );
    }
}
