//! Keeping the elements inside the codec's domain.
//!
//! An element is a sum, and two coherent objects in the same element exceed
//! full scale. The encoder clamped and counted; it did not prevent. Two
//! things prevent it, and they are not the same thing.
//!
//! # Prevent it in the fit
//!
//! What element `k` can reach is bounded by `Σ_i w_ik · g_i · peak_i` — every
//! object's peak, gained and weighted, adding coherently. When that bound
//! exceeds the domain, the objects it is made of are solved again with an
//! upper bound on their weight for that element, each bound its share of the
//! headroom, so that the coherent worst case of the element they share is
//! full scale and no more. That is what a limiter after the fact cannot do:
//! it does not know the weights. What an object loses on the bounded element
//! the fit moves to the others it may reach, and the level it cannot put
//! back it leaves off — which the metric's level column reports, since the
//! metric weighs a rendered level and not a clamp.
//!
//! # And only the residual goes through a limiter
//!
//! The bound is on the coherent worst case, so once it holds a peak cannot
//! pass full scale; what can is rounding, and a block whose bound was decided
//! on one set of weights while the mixer ramps to it from another. That is
//! the residual, and it goes through a limiter with **one gain for every
//! element**: a gain per element would move the image of an object spread
//! over several, and the metric would not see it move. Offline, the gain is
//! smoothed ahead of the peak rather than after it — see
//! [`crate::mix::Limiter`].
//!
//! Both are counted: how many blocks were bounded, how many were limited.

use crate::fit;
use crate::{Clustering, Object};

/// Full scale, in the unit the mixer sums in.
pub const CEILING: f64 = 1.0;

/// Whether the fit bounds an element's coherent peak to the domain.
///
/// Off, because the metric says so. The bound is on the coherent worst
/// case, and forty tones in nine contributions an element pass it in every
/// block while the mix itself passes full scale in a few samples of some:
/// on `crowd40` it fires in every block, costs a fifth of the fold's mean
/// and two thirds of its worst object in the encoder, and halves the clips
/// rather than ending them, since the weights ramp in from a block bounded
/// for other peaks. The limiter with one gain for every element ends them
/// for nothing the metric sees — see `docs/clustering.md`. Kept compiled
/// and measurable, from `harlettizer encode --headroom` and the harness.
pub const BOUNDING: bool = false;

/// Passes of the bound: a re-solve can push another element over, and one
/// more pass catches it. Three is where nothing has been measured to remain.
const PASSES: usize = 3;

/// Bound every element's coherent peak to the domain, re-solving the objects
/// it is made of. `rows[i]` is object `i`'s weights over the live elements,
/// and is rewritten; returns how many elements had to be bounded.
///
/// `held` and `masks` are what the free fit was made with, so that a bounded
/// object keeps the set it had and the class it is in.
#[allow(clippy::too_many_arguments)]
pub fn bound(
    rows: &mut [Vec<f64>],
    radiated: &[Vec<f64>],
    targets: &[Vec<f64>],
    objects: &[Object],
    live: &[usize],
    held: Option<&Clustering>,
    masks: Option<&[Vec<bool>]>,
    anchor: &mut Vec<f64>,
) -> usize {
    let slots = live.len();
    let mut bounded = vec![false; slots];
    let mut bounds = vec![f64::INFINITY; slots];
    let mut peaks = vec![0.0f64; slots];
    for _ in 0..PASSES {
        // The coherent worst case of every live element.
        peaks.fill(0.0);
        for (object, row) in rows.iter().enumerate() {
            let peak = objects[object].peak;
            if peak <= 0.0 {
                continue;
            }
            for (slot, weight) in row.iter().enumerate() {
                peaks[slot] += weight.abs() * peak;
            }
        }
        let over: Vec<usize> = (0..slots).filter(|slot| peaks[*slot] > CEILING).collect();
        if over.is_empty() {
            break;
        }
        for slot in &over {
            bounded[*slot] = true;
        }
        // Every object reaching an element that is over gets its share of
        // that element's headroom as a bound, and is solved again.
        for (object, row) in rows.iter_mut().enumerate() {
            if objects[object].pinned.is_some() {
                continue;
            }
            let reaches = over.iter().any(|slot| row[*slot] > 0.0);
            if !reaches {
                continue;
            }
            bounds.fill(f64::INFINITY);
            for slot in &over {
                if row[*slot] > 0.0 {
                    bounds[*slot] = row[*slot] * CEILING / peaks[*slot];
                }
            }
            let held = held.and_then(|held| {
                (held.weights.len() == objects.len() && held.elements() > 0).then(|| {
                    let was = &held.weights[object];
                    anchor.clear();
                    anchor.extend(
                        live.iter()
                            .map(|element| was.get(*element).copied().unwrap_or(0.0)),
                    );
                    fit::Held {
                        previous: anchor,
                        tolerance: fit::HELD_TOLERANCE,
                    }
                })
            });
            *row = fit::fitted_bounded(
                radiated,
                &targets[object],
                fit::MAX_PATHS,
                held,
                masks.map(|masks| &masks[object][..]),
                Some(&bounds),
            );
        }
    }
    bounded.iter().filter(|b| **b).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fit::Reference;
    use hz_render::Mode;

    /// Two coherent objects at full scale in one element are bounded so that
    /// their coherent sum is full scale and no more, and what they lose is
    /// level rather than place.
    #[test]
    fn two_coherent_objects_are_bounded_to_the_domain() {
        let reference = Reference::new().unwrap();
        let objects: Vec<Object> = [[0.0, 1.0, 0.0], [0.05, 1.0, 0.0]]
            .into_iter()
            .map(|position| Object {
                position,
                energy: 0.4,
                size: 0.0,
                pinned: None,
                mode: Mode::default(),
                peak: 0.9,
            })
            .collect();
        let live = vec![0usize];
        let mut radiated = vec![Vec::new()];
        reference.radiated([0.02, 1.0, 0.0], 0.0, &mut radiated[0]);
        let targets: Vec<Vec<f64>> = objects
            .iter()
            .map(|object| {
                let mut out = Vec::new();
                reference.radiated(object.position, 0.0, &mut out);
                out
            })
            .collect();
        let mut rows: Vec<Vec<f64>> = targets
            .iter()
            .map(|target| fit::fitted(&radiated, target))
            .collect();
        let before: f64 = rows.iter().map(|row| row[0] * 0.9).sum();
        assert!(before > CEILING, "the scene does not clip: {before}");
        let was = [rows[0][0], rows[1][0]];

        let mut anchor = Vec::new();
        let bounded = bound(
            &mut rows,
            &radiated,
            &targets,
            &objects,
            &live,
            None,
            None,
            &mut anchor,
        );
        assert_eq!(bounded, 1);
        let after: f64 = rows.iter().map(|row| row[0] * 0.9).sum();
        assert!(after <= CEILING + 1e-9 && after > CEILING - 1e-6, "{after}");
        // Both came down, and by the same share: each keeps its own weight
        // scaled by the one factor that brings their coherent sum to the
        // ceiling.
        let shares = [rows[0][0] / was[0], rows[1][0] / was[1]];
        assert!((shares[0] - shares[1]).abs() < 1e-9, "{shares:?}");
        assert!((shares[0] - CEILING / before).abs() < 1e-9, "{shares:?}");
    }
}
