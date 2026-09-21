//! Making an element earn the place it is in.
//!
//! An iteration of Lloyd's method moves a centre; it never hands an element
//! from one part of the scene to another. So an element given to a quiet object
//! by [`crate::hand_out_the_spares`] keeps it for as long as that object
//! exists, however much better it would be spent elsewhere later — and the
//! stability counters cannot report that, because they exclude elements
//! carrying almost nothing. An indicator that excludes a case by construction
//! is not a check on that case, which is an argument this project has already
//! made against itself once.
//!
//! # The move
//!
//! One, and it is judged rather than argued: take the element carrying the
//! least, put it on the object that is worst served, re-solve the weights, and
//! keep the result only if the metric says it costs less. The element carrying
//! the least is the cheapest thing in the scene to move — that is what carrying
//! the least means — and the object worst served is where an element is worth
//! the most, so it is the one exchange most likely to pay.
//!
//! It is not a heuristic dressed as an improvement: the score before and after
//! is the same quantity the metric reports,
//!
//! ```text
//! Σ_i  e_i · ‖ Σ_k w_ik·r(q_k) − r(p_i) ‖² / ‖ r(p_i) ‖²
//! ```
//!
//! over [`crate::fit::Reference`], which is the stacked gain vectors of the
//! four presentations. If the exchange does not lower it, nothing happens.
//!
//! # How often that is, and what it says about the case
//!
//! Less often than it sounds, and for a reason worth writing down: an element
//! that owns nothing but a whisper is not thereby idle. The fit is free to send
//! any object through it, and does — an element overhead is a direction a
//! floor-level object leans on to shape what it radiates on 7.1.4. So the
//! element under the whisper is usually carrying a share of something loud, and
//! taking it away costs more than it saves.
//!
//! Where it pays is where there is real slack: forty objects into **sixteen**
//! elements, the worst object goes from 0.293 to 0.216 and the mean from 0.035
//! to 0.033. Into **twelve** of the same, where every element is contested,
//! nothing is exchanged at all and every figure is unchanged. That is the
//! measurement doing its job in both directions.
//!
//! # Why it is not simply the spare rule again
//!
//! [`crate::hand_out_the_spares`] gives away elements **nothing chose**, which
//! is a case that only arises when the scene has fewer directions than the
//! bitstream has elements. This is the case where every element is in use and
//! one of them is being wasted — where a pair of loud objects share an element
//! while a twelfth sits under a whisper. Nothing before this could take it
//! back.

use crate::fit::{self, Reference};
use crate::{Clustering, Object};
use hz_render::Mode;

/// How many blocks between one look for a wasted element and the next.
///
/// An element spent badly is spent badly for a while — that is what makes it
/// worth taking back — so there is nothing to gain from asking every block,
/// and something to lose. Asked every block on a scene that only jitters, "the
/// object worst served" is a different object each time and the spare element
/// is dragged from one to the next: 48 objects displaced by a random three
/// degrees a block changed hands 5 times over a hundred blocks with the
/// exchange off and **57** with it asked every block. Asked every eighth, the
/// churn goes with it and the exchange still finds what it is for, which is a
/// steady waste and not a flicker.
///
/// The same cadence as [`crate::restart::RECONSIDER`], for the same reason.
pub const EVERY: u64 = 8;

/// How much the finished answer has to improve before an exchange is kept, as
/// a fraction of what it cost before.
///
/// Not a refinement. Without it the exchange fires on any gain at all, and on
/// a scene that only jitters "the object worst served" is a different object
/// every block, so the spare element is dragged from one to the next for
/// noise: measured on 48 objects displaced by a random three degrees a block,
/// 5 objects changed hands over a hundred blocks with the exchange off and
/// **61** with it on for any gain.
///
/// See [`worth_the_move`] for the absolute floor that goes with it, and
/// [`crate::restart::WORTH`], which is the same medicine for the same disease
/// one stage further on.
pub const WORTH: f64 = 0.05;

/// And how much in absolute terms, per unit of energy in the scene.
///
/// A fraction alone is not enough where both costs are about nothing — the
/// argument is [`crate::restart::WORTH_AT_LEAST`]'s, and the unit is the same:
/// the cost is `Σ e_i · relative²`, so this is a hundredth of one object.
pub const WORTH_AT_LEAST: f64 = 1e-4;

/// Whether the finished answer earned the move it made.
pub fn worth_the_move(before: f64, after: f64, objects: &[Object]) -> bool {
    if after >= before * (1.0 - WORTH) {
        return false;
    }
    let energy: f64 = objects.iter().map(|object| object.energy.max(0.0)).sum();
    before - after > WORTH_AT_LEAST * energy
}

/// Try the one exchange, and keep it if the metric agrees.
///
/// `targets[i]` is what object `i` radiates over the reference and
/// `radiated[slot]` what the live element in that slot does; `radiated` is left
/// describing whatever positions the clustering ends up with.
///
/// Returns whether anything changed, so a caller can count how often the fold
/// needed this.
///
/// With `classed`, the element that moves takes the class of the object it
/// moves to, and every object is re-solved over the elements of its own
/// class alone — see [`crate::class`].
#[allow(clippy::too_many_arguments)]
pub fn earn_it(
    clustering: &mut Clustering,
    objects: &[Object],
    live: &[usize],
    reference: &Reference,
    targets: &[Vec<f64>],
    radiated: &mut [Vec<f64>],
    scratch: &mut Scratch,
    classed: bool,
) -> bool {
    // Two live elements at least, or there is nothing to take one from.
    if live.len() < 2 || objects.is_empty() {
        return false;
    }
    // A pinned element is a speaker feed and is not anybody's to spend.
    let pinned: Vec<bool> = live
        .iter()
        .map(|element| {
            objects.iter().enumerate().any(|(object, source)| {
                source.pinned.is_some() && clustering.owners[object] == *element
            })
        })
        .collect();

    // What each element carries, and how badly each object is served.
    scratch.carried.clear();
    scratch.carried.resize(live.len(), 0.0);
    let mut worst = None;
    let mut worst_cost = 0.0;
    for (object, row) in clustering.weights.iter().enumerate() {
        let energy = objects[object].energy.max(0.0);
        for (slot, element) in live.iter().enumerate() {
            scratch.carried[slot] += energy * row[*element] * row[*element];
        }
        // A pinned object is where it was told to be and has nothing to gain.
        if objects[object].pinned.is_some() {
            continue;
        }
        let cost = energy * relative(object, clustering, live, radiated, targets);
        if cost > worst_cost {
            worst_cost = cost;
            worst = Some(object);
        }
    }
    let Some(worst) = worst.filter(|_| worst_cost > 0.0) else {
        return false;
    };

    // The cheapest element to move is the one carrying the least. A pinned
    // one is not for sale at any price, and neither is the last element of a
    // class that has anything to carry: a class present keeps an element,
    // whatever the exchange would gain — see [`crate::class`].
    let last_of_its_class = |slot: usize| {
        classed && {
            let mode = clustering.modes[live[slot]];
            live.iter()
                .filter(|element| clustering.modes[**element] == mode)
                .count()
                == 1
                && objects
                    .iter()
                    .any(|object| object.mode == mode && object.energy > 0.0)
        }
    };
    let Some(spare) = (0..live.len())
        .filter(|slot| !pinned[*slot] && !last_of_its_class(*slot))
        .min_by(|a, b| {
            scratch.carried[*a]
                .partial_cmp(&scratch.carried[*b])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    else {
        return false;
    };
    // Moving an element on to an object it is already on is not a move.
    if clustering.positions[live[spare]] == objects[worst].position {
        return false;
    }

    let before = total(clustering, objects, live, radiated, targets);

    // Try it: the element goes to the object, and every object shares itself
    // out again over where the elements now are.
    scratch.was_position = clustering.positions[live[spare]];
    scratch.was_direction = clustering.directions[live[spare]];
    scratch.was_mode = clustering.modes.get(live[spare]).copied();
    scratch.was_radiated.clear();
    scratch.was_radiated.extend_from_slice(&radiated[spare]);
    scratch.was_weights.clear();
    scratch
        .was_weights
        .extend(clustering.weights.iter().cloned());

    clustering.positions[live[spare]] = objects[worst].position;
    clustering.directions[live[spare]] = crate::direction(objects[worst].position);
    if let Some(mode) = clustering.modes.get_mut(live[spare]) {
        *mode = objects[worst].mode;
    }
    reference.radiated(objects[worst].position, 0.0, &mut radiated[spare]);
    for (object, target) in targets.iter().enumerate() {
        let allowed = classed.then(|| {
            scratch.allowed.clear();
            scratch.allowed.extend(
                live.iter()
                    .map(|element| clustering.modes[*element] == objects[object].mode),
            );
            &scratch.allowed[..]
        });
        let row = fit::fitted_within_near(radiated, target, fit::MAX_PATHS, None, allowed);
        for (slot, element) in live.iter().enumerate() {
            clustering.weights[object][*element] = row[slot];
        }
    }

    let after = total(clustering, objects, live, radiated, targets);
    if after < before {
        // An object is never left owned by an element of another class: the
        // ones the element leaves behind go to the nearest element of their
        // own. With one class there is nothing to leave.
        let moved = live[spare];
        for (object, owner) in clustering.owners.iter_mut().enumerate() {
            if *owner != moved || objects[object].mode == clustering.modes[moved] {
                continue;
            }
            let here = crate::direction(objects[object].position);
            if let Some(home) = live
                .iter()
                .copied()
                .filter(|element| clustering.modes[*element] == objects[object].mode)
                .max_by(|a, b| {
                    let near =
                        |e: usize| crate::dot(crate::direction(clustering.positions[e]), here);
                    near(*a)
                        .partial_cmp(&near(*b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            {
                *owner = home;
            }
        }
        return true;
    }

    // It did not pay, so nothing happened.
    clustering.positions[live[spare]] = scratch.was_position;
    clustering.directions[live[spare]] = scratch.was_direction;
    if let (Some(mode), Some(was)) = (clustering.modes.get_mut(live[spare]), scratch.was_mode) {
        *mode = was;
    }
    radiated[spare].clear();
    radiated[spare].extend_from_slice(&scratch.was_radiated);
    let width = clustering.weights.len();
    clustering
        .weights
        .clone_from_slice(&scratch.was_weights[..width]);
    false
}

/// The whole scene's cost, in the metric's own terms.
fn total(
    clustering: &Clustering,
    objects: &[Object],
    live: &[usize],
    radiated: &[Vec<f64>],
    targets: &[Vec<f64>],
) -> f64 {
    (0..objects.len())
        .map(|object| {
            objects[object].energy.max(0.0) * relative(object, clustering, live, radiated, targets)
        })
        .sum()
}

/// How far one object lands from where it asked to be, squared and relative to
/// its own gain vector — one term of the metric, before the square root the
/// report takes.
fn relative(
    object: usize,
    clustering: &Clustering,
    live: &[usize],
    radiated: &[Vec<f64>],
    targets: &[Vec<f64>],
) -> f64 {
    let target = &targets[object];
    let row = &clustering.weights[object];
    let mut difference = 0.0;
    let mut reference = 0.0;
    for (row_index, want) in target.iter().enumerate() {
        let mut got = 0.0;
        for (slot, element) in live.iter().enumerate() {
            let weight = row[*element];
            if weight != 0.0 {
                got += weight * radiated[slot][row_index];
            }
        }
        difference += (got - want) * (got - want);
        reference += want * want;
    }
    if reference > 1e-12 {
        difference / reference
    } else {
        0.0
    }
}

/// The buffers the exchange needs to be able to undo itself.
#[derive(Debug, Default)]
pub struct Scratch {
    carried: Vec<f64>,
    was_position: [f64; 3],
    was_direction: [f64; 3],
    was_mode: Option<Mode>,
    was_radiated: Vec<f64>,
    was_weights: Vec<Vec<f64>>,
    /// One object's class mask over the live elements.
    allowed: Vec<bool>,
}
