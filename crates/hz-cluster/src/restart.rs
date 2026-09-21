//! Starting the clustering over when the scene has left its basins behind.
//!
//! The first block of a stream decides which parts of the scene the elements
//! sit in, and every block after it is warm-started from the one before. That
//! is what keeps an element still when the scene has not moved, and it is also
//! a decision that is never revisited: Lloyd's method finds the nearest local
//! answer to where it started, so a programme that opens on a line of
//! dialogue at the centre and closes on a battle is folded, two hours later,
//! by basins the dialogue chose.
//!
//! # When starting over is free
//!
//! Not often, which is the point. A cold partition is a *different* set of
//! element positions, and moving every element at once is exactly what the
//! warm start exists to prevent — a listener hears the jump rather than the
//! mix. So the restart is taken only when it is both **worth** something and
//! **cheap**:
//!
//! - worth: the cold answer costs at least [`WORTH`] less than the warm one,
//!   in the metric's own terms.
//!
//! - cheap: what it would cost to get there — how far each element moves,
//!   weighted by what it carries — is smaller than what it saves. On a cut
//!   there is nothing to carry, because the objects that were loud have gone
//!   and the ones arriving were not there before, so this term is near zero
//!   exactly when a scene change has happened. It is a way of noticing a cut
//!   without looking for one.
//!
//! Both are measured on the same quantity, so the decision is one comparison:
//! `cold + [`MOVING`]·transition < warm`, with a floor under the gain so that
//! two answers of equal merit do not trade places every time they are asked.
//!
//! # And it is not asked every block
//!
//! Settling the scene twice costs what settling it once does. It is asked
//! every [`RECONSIDER`] blocks, which is a fifth of a second — fast enough
//! that a cut is picked up within a frame of video, slow enough that the fold
//! costs an eighth more rather than twice as much.
//!
//! The renumbering runs against the *warm* answer before the comparison, so a
//! restart that is taken has already been matched element for element to what
//! it replaces: whatever has to move moves, and nothing moves merely for being
//! numbered differently.

use crate::{Clustering, Object};

/// How many blocks between one look at a cold start and the next.
///
/// Eight blocks is 214 ms at forty-eight kilohertz. A cut is picked up within
/// five frames of video, and the fold pays an eighth more rather than twice as
/// much.
pub const RECONSIDER: u64 = 8;

/// How much better a cold answer has to be before it is worth having, as a
/// fraction.
///
/// A floor under the gain, not a threshold on the decision. Two answers of
/// equal merit would otherwise trade places every time they were compared, and
/// what a listener would hear from that is the elements moving for nothing.
pub const WORTH: f64 = 0.05;

/// And how much it has to be better in absolute terms, per unit of energy in
/// the scene.
///
/// A fraction on its own is not enough. On a master whose fold is the identity
/// both answers cost about nothing, and one about-nothing is five per cent
/// less than another about-nothing on rounding alone: one real master
/// restarted in 27 of its 753 blocks for savings in the thirtieth decimal. The
/// restarts were harmless there — the answer is the same answer — but a rule
/// that fires on noise is a rule that will fire on noise somewhere it matters.
///
/// The cost is `Σ e_i · relative²`, so dividing by `Σ e_i` gives a mean squared
/// relative error; a ten-thousandth of that is one per cent of one object, and
/// nothing below it is worth moving twelve elements for.
pub const WORTH_AT_LEAST: f64 = 1e-4;

/// What moving the elements to a cold answer costs, per unit of energy carried
/// and per unit of `1 − cos` moved.
///
/// # Why it is one over the interval and not a number
///
/// The two sides of the comparison are not paid at the same rate. A better
/// answer saves its difference **every block** for as long as it is kept, and
/// it is kept at least until this is asked again; the move is paid **once**.
/// So the move is discounted by the number of blocks the saving is guaranteed
/// for, which is [`RECONSIDER`], and the comparison is between two quantities
/// per block rather than between a rate and a total.
///
/// It is a floor on the discount and not an estimate of it: an answer that is
/// better now is usually better for far longer than an eighth of a second, so
/// this errs towards staying where it is.
///
/// This is also the term that makes the rule notice a cut without looking for
/// one: after a cut the elements carry nothing that was there before, so
/// moving them costs nothing and the restart is free.
pub const MOVING: f64 = 1.0 / RECONSIDER as f64;

/// Whether to abandon the warm answer for the cold one.
///
/// `cold` has already been renumbered against `warm`, so element `k` of one is
/// element `k` of the other and the transition is a sum over slots.
pub fn worth_starting_over(
    warm: &Clustering,
    warm_cost: f64,
    cold: &Clustering,
    cold_cost: f64,
    objects: &[Object],
) -> bool {
    if cold_cost >= warm_cost * (1.0 - WORTH) {
        return false;
    }
    let energy: f64 = objects.iter().map(|object| object.energy.max(0.0)).sum();
    if warm_cost - cold_cost <= WORTH_AT_LEAST * energy {
        return false;
    }
    let transition = moving(warm, cold, objects);
    cold_cost + MOVING * transition < warm_cost
}

/// What it would cost to move from one answer to the other: how far each
/// element goes, weighted by what it is carrying while it goes.
///
/// Weighted by what the **warm** answer carries, which is what is playing:
/// where the cold answer would put the energy is not what a listener hears on
/// the way there.
fn moving(warm: &Clustering, cold: &Clustering, objects: &[Object]) -> f64 {
    let elements = warm.elements();
    if cold.elements() != elements {
        return f64::INFINITY;
    }
    let mut total = 0.0;
    for element in 0..elements {
        let carried: f64 = objects
            .iter()
            .zip(&warm.weights)
            .map(|(object, row)| object.energy.max(0.0) * row[element] * row[element])
            .sum();
        if carried <= 0.0 {
            continue;
        }
        let apart = 1.0 - dot(warm.directions[element], cold.directions[element]).clamp(-1.0, 1.0);
        total += carried * apart;
    }
    total
}

/// What a clustering costs, in the metric's own terms and without the square
/// root the report takes.
///
/// `targets[i]` is what object `i` radiates over the reference and
/// `radiated[slot]` what the live element in that slot does — the vectors the
/// fit was already made against, so this is the quantity the metric reports
/// and not a stand-in for it.
pub fn cost(
    clustering: &Clustering,
    objects: &[Object],
    live: &[usize],
    radiated: &[Vec<f64>],
    targets: &[Vec<f64>],
) -> f64 {
    let mut total = 0.0;
    for (object, source) in objects.iter().enumerate() {
        let energy = source.energy.max(0.0);
        if energy <= 0.0 {
            continue;
        }
        let target = &targets[object];
        let row = &clustering.weights[object];
        let mut difference = 0.0;
        let mut reference = 0.0;
        for (index, want) in target.iter().enumerate() {
            let mut got = 0.0;
            for (slot, element) in live.iter().enumerate() {
                let weight = row[*element];
                if weight != 0.0 {
                    got += weight * radiated[slot][index];
                }
            }
            difference += (got - want) * (got - want);
            reference += want * want;
        }
        if reference > 1e-12 {
            total += energy * difference / reference;
        }
    }
    total
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
