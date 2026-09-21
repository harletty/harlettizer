//! Where the elements start from.
//!
//! Lloyd's method finds the nearest local answer to where it started, so the
//! first guess matters more than the passes that follow it. Two rules are
//! written, and `docs/clustering.md` records what each measured.
//!
//! # By direction
//!
//! The rule the clustering had: the loudest object, then repeatedly the object
//! worst served by the seeds so far, weighted by its energy — "worst served"
//! being `1 − cos` to the nearest seed. It is an energy-times-angle rule, and
//! two things are wrong with it. It can open an element for an isolated quiet
//! object before a cluster of loud ones, since a quiet object far from every
//! seed scores as much as a loud one near one; and it does not know what an
//! element placed there would **capture** — an element serves everything
//! whose rendered gain vector overlaps its own, not only the object it sits
//! on.
//!
//! # By capture
//!
//! A candidate's score is the importance an element at its position would
//! serve that nothing serves yet. The serving kernel is the **overlap of
//! rendered gain vectors** over the presentations — the cosine between what
//! the candidate radiates and what each object radiates, which is already
//! computed for the fit and is the metric's own language. No radius and no
//! distance in the cube. Take the best candidate, subtract from every object
//! the share it now serves, repeat: the greedy maximisation of a submodular
//! coverage, which is within a constant of the best any seeding could do.
//!
//! Every object is a candidate, so an element starts on an object as it did;
//! what changes is which object, and why. It is measured, and it is not what
//! ships — see [`SEEDING`].

use crate::Object;
use crate::class::Judge;

/// How the elements start from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rule {
    /// The loudest, then the worst served by direction.
    #[default]
    Direction,
    /// The object an element on which would serve the most importance not
    /// yet served, by the overlap of rendered gain vectors.
    Capture,
}

/// The rule every fold in this project seeds by.
///
/// By direction, because the metric says so. Seeding by capture takes a
/// tenth off the worst object on `crowd40` and puts a fifth on its mean at
/// twelve and sixteen elements, ties on the broadband scenes, wins on the
/// ring at twelve and sixteen and loses at six and eight, and doubles the
/// flips of a scene with classes — see `docs/clustering.md` for the tables.
/// A seed that serves the most importance is not a seed the passes that
/// follow settle best from, and the passes are what decide. Kept compiled
/// and measurable, one constant away.
pub const SEEDING: Rule = Rule::Direction;

/// The seeds by capture, as indices into `members`, at most `limit` of them.
///
/// `judge` is what each object radiates; without one there is nothing to
/// overlap, and the caller falls back to the directional rule.
pub fn capture(
    objects: &[Object],
    members: &[usize],
    judge: &Judge<'_>,
    limit: usize,
    floor: &crate::floor::Floor,
) -> Vec<usize> {
    let count = members.len();
    // An object under the room's floor is nothing to serve — see
    // [`crate::floor`].
    let energies: Vec<f64> = members
        .iter()
        .map(|index| objects[*index].energy.max(0.0))
        .map(|energy| if floor.heard(energy) { energy } else { 0.0 })
        .collect();
    let total: f64 = energies.iter().sum();
    let lengths: Vec<f64> = members
        .iter()
        .map(|index| {
            judge.targets[*index]
                .iter()
                .map(|g| g * g)
                .sum::<f64>()
                .sqrt()
        })
        .collect();
    // How much of each object is served already: the largest overlap with any
    // seed so far, nought before there is one.
    let mut served = vec![0.0f64; count];
    let mut seeds: Vec<usize> = Vec::with_capacity(limit.min(count));
    let mut taken = vec![false; count];
    // The overlap of every pair, computed once: at forty objects and
    // twenty-eight rows it is a few tens of thousands of multiplications.
    let overlap = |a: usize, b: usize| -> f64 {
        let (ta, tb) = (&judge.targets[members[a]], &judge.targets[members[b]]);
        let lengths = lengths[a] * lengths[b];
        if lengths <= 1e-12 {
            return 0.0;
        }
        (ta.iter().zip(tb).map(|(x, y)| x * y).sum::<f64>() / lengths).clamp(0.0, 1.0)
    };
    let mut kernel = vec![0.0f64; count * count];
    for a in 0..count {
        kernel[a * count + a] = 1.0;
        for b in (a + 1)..count {
            let k = overlap(a, b);
            kernel[a * count + b] = k;
            kernel[b * count + a] = k;
        }
    }

    while seeds.len() < limit.min(count) {
        // What an element on each candidate would newly serve.
        let mut best: Option<(usize, f64)> = None;
        for candidate in 0..count {
            if taken[candidate] {
                continue;
            }
            let gain: f64 = (0..count)
                .map(|object| {
                    energies[object]
                        * (kernel[candidate * count + object] - served[object]).max(0.0)
                })
                .sum();
            if best.is_none_or(|(_, most)| gain > most) {
                best = Some((candidate, gain));
            }
        }
        // Nothing left worth serving — which includes an object on the same
        // spot as a seed, whose overlap is one to within rounding.
        let Some((candidate, _)) = best.filter(|(_, gain)| *gain > 1e-9 * total) else {
            break;
        };
        taken[candidate] = true;
        seeds.push(candidate);
        for object in 0..count {
            served[object] = served[object].max(kernel[candidate * count + object]);
        }
    }
    seeds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fit::Reference;
    use hz_render::Mode;

    fn scene(placed: &[([f64; 3], f64)]) -> Vec<Object> {
        placed
            .iter()
            .map(|(position, energy)| Object {
                position: *position,
                energy: *energy,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            })
            .collect()
    }

    fn judged(objects: &[Object]) -> (Reference, Vec<Vec<f64>>) {
        let reference = Reference::new().unwrap();
        let targets = objects
            .iter()
            .map(|object| {
                let mut out = Vec::new();
                reference.radiated(object.position, object.size, &mut out);
                out
            })
            .collect();
        (reference, targets)
    }

    /// The first seed goes where an element would serve most — a cluster of
    /// loud objects — and not to the loudest single object across the room
    /// from them; the second goes to what the first left unserved.
    #[test]
    fn a_seed_goes_where_it_serves_most() {
        let objects = scene(&[
            ([-0.2, 1.0, 0.0], 1.0),
            ([0.0, 1.0, 0.0], 1.0),
            ([0.2, 1.0, 0.0], 1.0),
            ([0.0, -1.0, 0.0], 1.5),
        ]);
        let (reference, targets) = judged(&objects);
        let judge = Judge {
            reference: &reference,
            targets: &targets,
        };
        let members: Vec<usize> = (0..4).collect();
        let seeds = capture(
            &objects,
            &members,
            &judge,
            2,
            &crate::floor::Floor::default(),
        );
        assert_eq!(seeds.len(), 2);
        assert!(
            seeds[0] < 3,
            "the first seed went to the loud loner: {seeds:?}"
        );
        assert_eq!(
            seeds[1], 3,
            "the second seed did not go to what was left: {seeds:?}"
        );
    }

    /// No more seeds than there is something to serve: objects on the same
    /// spot are one seed, and silence earns none.
    #[test]
    fn seeds_stop_when_nothing_is_left_to_serve() {
        let objects = scene(&[
            ([0.0, 1.0, 0.0], 1.0),
            ([0.0, 1.0, 0.0], 1.0),
            ([1.0, 0.0, 0.0], 0.0),
        ]);
        let (reference, targets) = judged(&objects);
        let judge = Judge {
            reference: &reference,
            targets: &targets,
        };
        let seeds = capture(
            &objects,
            &[0, 1, 2],
            &judge,
            3,
            &crate::floor::Floor::default(),
        );
        assert_eq!(seeds.len(), 1, "{seeds:?}");
        assert!(capture(&objects, &[], &judge, 3, &crate::floor::Floor::default()).is_empty());
    }
}
