//! Keeping apart the objects that render differently, and sharing the
//! elements between them.
//!
//! # The class
//!
//! An object's [`Mode`] — snap, zones, elevation, screen — is what a
//! renderer does with it beyond where it is put. Two objects of different
//! modes render differently however close they sit, so a fold that puts them
//! in one element changes what one of them means: a voice snapped to the
//! centre folded together with a free effect is no longer snapped, or the
//! effect is. No master to hand ever varies these fields, and an authored one
//! will.
//!
//! So a mode is a **class**, and the class is a hard constraint on the
//! partition: an element belongs to one class, an object is only ever owned
//! by an element of its class, the fit only ever spreads it over elements of
//! its class, the hysteresis never crosses a class, and the metadata written
//! for an element are its class's — exact, never a mixture. Nothing about a
//! class is a preference that a loud enough object could buy its way past.
//!
//! # Sharing the budget
//!
//! A bitstream's elements have to be shared between the classes, and the
//! rule is not a distance proxy but the **marginal gain of the metric**. The
//! seeding is incremental — the loudest object, then repeatedly the worst
//! served — so it yields in one pass what a class would cost with one seed,
//! with two, with three: a curve that never rises. The gain of a class's
//! `k`-th element is the fall of its curve from `k − 1` to `k`. Every class
//! present gets one element, then the rest go one at a time to the class
//! whose next element gains most; the Lloyd passes and the placement search
//! then run once, on the share kept.
//!
//! The cost is judged the way the metric judges — the rendered gain vectors
//! over the presentations — when the fit has them, and by direction when it
//! does not, which is only the two weightings kept for comparison.
//!
//! # And held between blocks
//!
//! A share that were recomputed from nothing every block would hand an
//! element from one class to another whenever two marginal gains crossed,
//! and what a listener would hear is the element crossing the room. So a
//! block starts from the share the last block had, and an element changes
//! class only when the class that would take it gains more than the class
//! that gives it up loses, by [`WORTH`] — the same medicine as the exchange
//! and the restart, for the same disease.
//!
//! # With one class
//!
//! Every master to hand. The share is the whole budget, the curve is not
//! consulted, and the partition is exactly what it was before there were
//! classes — which is asserted, since a master with nothing to keep apart
//! has to fold to the same bytes.

use crate::Object;
use crate::fit::Reference;
use hz_render::Mode;

/// How much more a class has to gain from an element than another loses
/// before the element changes class between blocks, as a fraction.
pub const WORTH: f64 = 0.05;

/// The class a bed channel folds as when it is not pinned: an object at its
/// speaker's place that snaps to the nearest speaker, which is what a speaker
/// feed is. Snapped objects of the mix render the same way and share it.
pub const BED: Mode = Mode {
    snap: true,
    zones: 0,
    elevation: true,
    screen: None,
};

/// The distinct modes among the given objects, in order of first appearance,
/// and which of them each object is.
pub fn classes(objects: &[Object], members: &[usize]) -> (Vec<Mode>, Vec<usize>) {
    let mut modes: Vec<Mode> = Vec::new();
    let mut class_of = Vec::with_capacity(members.len());
    for index in members {
        let mode = objects[*index].mode;
        let class = match modes.iter().position(|known| *known == mode) {
            Some(class) => class,
            None => {
                modes.push(mode);
                modes.len() - 1
            }
        };
        class_of.push(class);
    }
    (modes, class_of)
}

/// The metric's view of a scene, for judging what a class would cost.
pub struct Judge<'a> {
    pub reference: &'a Reference,
    /// What each object radiates over the reference, by object index.
    pub targets: &'a [Vec<f64>],
}

/// How a class would fare with its first `k` seeds, for every `k`.
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    /// The seeds in the order the clustering takes them, as object indices.
    pub seeds: Vec<usize>,
    /// What the class costs with the first `k` seeds: `cost[0]` with none,
    /// `cost[k]` with `k`. Never rises.
    pub cost: Vec<f64>,
    /// Whether anything in the class carries energy at all.
    pub present: bool,
}

impl Curve {
    /// What the class's next element would gain, with `k` already.
    pub fn gain(&self, k: usize) -> f64 {
        match (self.cost.get(k), self.cost.get(k + 1)) {
            (Some(with), Some(more)) => with - more,
            _ => 0.0,
        }
    }

    /// What giving up the last of `k` elements would lose.
    pub fn loss(&self, k: usize) -> f64 {
        if k == 0 { 0.0 } else { self.gain(k - 1) }
    }
}

/// The seeds of one class in the order the clustering would take them, and
/// the curve of what the class costs with each count of them, up to `limit`.
///
/// The seed *order* is the seeding rule's — by capture when there is a
/// judge to capture with, by direction otherwise, which is the order the
/// clustering itself starts from; the *cost* is the judge's when there is
/// one and the direction's when there is not.
pub fn curve(
    objects: &[Object],
    members: &[usize],
    judge: Option<&Judge<'_>>,
    limit: usize,
    seeding: crate::seed::Rule,
    floor: &crate::floor::Floor,
) -> Curve {
    let count = members.len();
    // An object under the room's floor seeds nothing and costs nothing to
    // leave unserved — see [`crate::floor`].
    let energies: Vec<f64> = members
        .iter()
        .map(|index| objects[*index].energy.max(0.0))
        .map(|energy| if floor.heard(energy) { energy } else { 0.0 })
        .collect();
    let directions: Vec<[f64; 3]> = members
        .iter()
        .map(|index| crate::direction(objects[*index].position))
        .collect();
    let present = energies.iter().any(|energy| *energy > 0.0);

    // The order the seeds come in, as indices into the members. By capture
    // when the judge is there to capture with; by direction — the loudest,
    // then repeatedly the worst served — otherwise.
    let order: Vec<usize> = match (seeding, judge) {
        (crate::seed::Rule::Capture, Some(judge)) => {
            crate::seed::capture(objects, members, judge, limit, floor)
        }
        _ => by_direction(&energies, &directions, limit),
    };

    // How far each member is from the nearest seed so far, by the judge for
    // costing the class. With no seed at all nothing serves anybody: the
    // whole of every object is lost, which the metric reads as one.
    let mut served = vec![1.0f64; count];
    let mut seeds = Vec::with_capacity(order.len());
    let mut cost = Vec::with_capacity(order.len() + 1);
    cost.push(energies.iter().sum::<f64>());
    let mut radiated = Vec::new();

    for next in order {
        seeds.push(members[next]);
        if let Some(judge) = judge {
            judge
                .reference
                .radiated(objects[members[next]].position, 0.0, &mut radiated);
        }
        for member in 0..count {
            let by_direction = 1.0 - crate::dot(directions[member], directions[next]);
            let by_judge = match judge {
                Some(judge) => {
                    let target = &judge.targets[members[member]];
                    let mut difference = 0.0;
                    let mut reference = 0.0;
                    for (want, got) in target.iter().zip(&radiated) {
                        difference += (got - want) * (got - want);
                        reference += want * want;
                    }
                    if reference > 1e-12 {
                        difference / reference
                    } else {
                        0.0
                    }
                }
                None => by_direction,
            };
            served[member] = served[member].min(by_judge);
        }
        cost.push(
            energies
                .iter()
                .zip(&served)
                .map(|(energy, served)| energy * served)
                .sum(),
        );
    }

    Curve {
        seeds,
        cost,
        present,
    }
}

/// The seeds by direction, as indices into the members: the loudest, then
/// repeatedly the object worst served by the seeds so far — `1 − cos` to the
/// nearest of them — weighted by its energy, until nothing is left to serve
/// or `limit` is reached.
pub fn by_direction(energies: &[f64], directions: &[[f64; 3]], limit: usize) -> Vec<usize> {
    let count = energies.len();
    let mut apart = vec![1.0f64; count];
    let mut order = Vec::with_capacity(limit.min(count));
    while order.len() < limit.min(count) {
        let next = if order.is_empty() {
            (0..count).max_by(|a, b| {
                energies[*a]
                    .partial_cmp(&energies[*b])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        } else {
            (0..count)
                .map(|member| (member, energies[member] * apart[member]))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .filter(|(_, worth)| *worth > 0.0)
                .map(|(member, _)| member)
        };
        let Some(next) = next else { break };
        order.push(next);
        for member in 0..count {
            apart[member] =
                apart[member].min(1.0 - crate::dot(directions[member], directions[next]));
        }
    }
    order
}

/// Share `elements` between the classes.
///
/// One to every class present, then one at a time to the class whose next
/// element gains most — starting from `previous`, when there is one of the
/// right shape, and moving an element between classes only when the taker
/// gains more than the giver loses by `worth`. Elements beyond what any
/// class can use go to the first class present, where they are spares.
///
/// `None` when there are more classes present than elements: a class cannot
/// go without, and there is nothing to be done about it here.
pub fn allocate(
    curves: &[Curve],
    elements: usize,
    previous: Option<&[usize]>,
    worth: f64,
) -> Option<Vec<usize>> {
    let present: Vec<bool> = curves.iter().map(|curve| curve.present).collect();
    let needed = present.iter().filter(|p| **p).count();
    if needed > elements {
        return None;
    }
    if needed == 0 {
        // Nothing carries energy: every element is a spare.
        let mut share = vec![0; curves.len()];
        if let Some(first) = share.first_mut() {
            *first = elements;
        }
        return Some(share);
    }

    // Start from the last block's share when it is a share of this scene:
    // the same classes, every present one served, and the whole budget.
    let held = previous.filter(|previous| {
        previous.len() == curves.len()
            && previous.iter().sum::<usize>() == elements
            && previous
                .iter()
                .zip(&present)
                .all(|(share, present)| !present || *share > 0)
    });
    let mut share: Vec<usize> = match held {
        Some(previous) => previous.to_vec(),
        None => present.iter().map(|p| usize::from(*p)).collect(),
    };

    // Elements a class holds that it cannot use — a class that shrank, or
    // that has fewer objects than elements — go back to the pool.
    let usable = |class: usize| curves[class].seeds.len().max(usize::from(present[class]));
    for class in 0..curves.len() {
        if share[class] > usable(class) {
            share[class] = usable(class);
        }
        if !present[class] {
            share[class] = 0;
        }
    }

    // The class whose next element gains most, and the class whose last
    // element loses least — the first of each on a tie, so that a tie is
    // decided the same way every block.
    let most_gain = |share: &[usize]| {
        let mut best: Option<(usize, f64)> = None;
        for class in
            (0..curves.len()).filter(|class| present[*class] && share[*class] < usable(*class))
        {
            let gain = curves[class].gain(share[class]);
            if best.is_none_or(|(_, most)| gain > most) {
                best = Some((class, gain));
            }
        }
        best
    };
    let least_loss = |share: &[usize]| {
        let mut best: Option<(usize, f64)> = None;
        for class in (0..curves.len()).filter(|class| present[*class] && share[*class] > 1) {
            let loss = curves[class].loss(share[class]);
            if best.is_none_or(|(_, least)| loss < least) {
                best = Some((class, loss));
            }
        }
        best
    };

    // Fill the budget where it gains most.
    let mut remaining = elements - share.iter().sum::<usize>();
    while remaining > 0 {
        match most_gain(&share) {
            Some((class, _)) => share[class] += 1,
            // Every class has an element for every object it has: the rest
            // are spares, and they sit with the first class present.
            None => {
                if let Some(first) = (0..curves.len()).find(|class| present[*class]) {
                    share[first] += remaining;
                }
                remaining = 0;
                continue;
            }
        }
        remaining -= 1;
    }

    // And only from a held share: move an element from the class that loses
    // least to the class that gains most, while that pays by the margin.
    if held.is_some() {
        for _ in 0..elements {
            let (Some((taker, gain)), Some((giver, loss))) =
                (most_gain(&share), least_loss(&share))
            else {
                break;
            };
            if taker == giver || gain <= loss * (1.0 + worth) {
                break;
            }
            share[giver] -= 1;
            share[taker] += 1;
        }
    }

    Some(share)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(position: [f64; 3], energy: f64, mode: Mode) -> Object {
        Object {
            position,
            energy,
            size: 0.0,
            pinned: None,
            mode,
            peak: 0.0,
        }
    }

    fn snapped() -> Mode {
        Mode {
            snap: true,
            ..Mode::default()
        }
    }

    /// Classes are modes, in the order they are met, and every object knows
    /// its own.
    #[test]
    fn a_class_is_a_mode() {
        let objects = vec![
            object([0.0, 1.0, 0.0], 1.0, Mode::default()),
            object([1.0, 0.0, 0.0], 1.0, snapped()),
            object([0.0, -1.0, 0.0], 1.0, Mode::default()),
        ];
        let (modes, class_of) = classes(&objects, &[0, 1, 2]);
        assert_eq!(modes, vec![Mode::default(), snapped()]);
        assert_eq!(class_of, vec![0, 1, 0]);
    }

    /// The curve starts at everything lost, falls with every seed, and
    /// reaches nought once every object is a seed; the first seed is the
    /// loudest.
    #[test]
    fn the_curve_falls_to_nothing() {
        let objects: Vec<Object> = (0..6)
            .map(|n| {
                let angle = n as f64 * std::f64::consts::TAU / 6.0;
                object(
                    [angle.sin(), angle.cos(), 0.0],
                    if n == 3 { 4.0 } else { 1.0 },
                    Mode::default(),
                )
            })
            .collect();
        let members: Vec<usize> = (0..6).collect();
        let whole = curve(
            &objects,
            &members,
            None,
            6,
            crate::seed::Rule::Direction,
            &crate::floor::Floor::default(),
        );
        assert!(whole.present);
        assert_eq!(whole.seeds[0], 3);
        assert_eq!(whole.cost.len(), 7);
        assert!((whole.cost[0] - 9.0).abs() < 1e-12);
        for pair in whole.cost.windows(2) {
            assert!(pair[1] <= pair[0] + 1e-12, "{:?}", whole.cost);
        }
        assert!(whole.cost[6].abs() < 1e-12);
        // A limit stops the seeds and the curve with them.
        assert_eq!(
            curve(
                &objects,
                &members,
                None,
                2,
                crate::seed::Rule::Direction,
                &crate::floor::Floor::default()
            )
            .cost
            .len(),
            3
        );
    }

    /// One each to begin with, then to whoever gains most; nothing to a
    /// class that carries nothing; and more classes than elements is not a
    /// share.
    #[test]
    fn the_share_goes_where_it_pays() {
        let steep = Curve {
            seeds: vec![0, 1, 2, 3],
            cost: vec![10.0, 4.0, 1.0, 0.4, 0.0],
            present: true,
        };
        let flat = Curve {
            seeds: vec![4, 5],
            cost: vec![2.0, 1.5, 1.0],
            present: true,
        };
        let silent = Curve {
            seeds: vec![6],
            cost: vec![0.0, 0.0],
            present: false,
        };
        let curves = vec![steep.clone(), flat.clone(), silent.clone()];
        assert_eq!(allocate(&curves, 4, None, WORTH), Some(vec![3, 1, 0]));
        assert_eq!(allocate(&curves, 5, None, WORTH), Some(vec![3, 2, 0]));
        // A tie goes to the first class, every block alike.
        let tied = vec![
            Curve {
                seeds: vec![0, 1],
                cost: vec![2.0, 1.0, 0.0],
                present: true,
            },
            Curve {
                seeds: vec![2, 3],
                cost: vec![2.0, 1.0, 0.0],
                present: true,
            },
        ];
        assert_eq!(allocate(&tied, 3, None, WORTH), Some(vec![2, 1]));
        // Beyond what the classes can use, the spares sit with the first.
        assert_eq!(allocate(&curves, 8, None, WORTH), Some(vec![6, 2, 0]));
        assert_eq!(
            allocate(&[steep.clone(), flat.clone()], 1, None, WORTH),
            None
        );
        // A single class takes the lot.
        assert_eq!(allocate(&[steep], 12, None, WORTH), Some(vec![12]));
    }

    /// A held share is kept unless moving an element pays by the margin,
    /// and a held share of the wrong shape is not held at all.
    #[test]
    fn a_share_is_held_between_blocks() {
        let a = Curve {
            seeds: vec![0, 1, 2],
            cost: vec![10.0, 5.0, 4.0, 3.0],
            present: true,
        };
        let b = Curve {
            seeds: vec![3, 4, 5],
            cost: vec![10.0, 5.0, 3.96, 3.0],
            present: true,
        };
        let curves = vec![a, b];
        // From nothing, b's second element gains 1.04 against a's 1.0.
        assert_eq!(allocate(&curves, 3, None, WORTH), Some(vec![1, 2]));
        // Held the other way round, four per cent is not worth the move.
        assert_eq!(allocate(&curves, 3, Some(&[2, 1]), WORTH), Some(vec![2, 1]));
        // Unless the margin is nought.
        assert_eq!(allocate(&curves, 3, Some(&[2, 1]), 0.0), Some(vec![1, 2]));
        // A share of the wrong size is not a share of this scene.
        assert_eq!(allocate(&curves, 3, Some(&[1, 1]), WORTH), Some(vec![1, 2]));
        // And a class that shrank gives its elements back to the pool.
        let shrunk = vec![
            Curve {
                seeds: vec![0],
                cost: vec![1.0, 0.0],
                present: true,
            },
            curves[1].clone(),
        ];
        assert_eq!(allocate(&shrunk, 3, Some(&[2, 1]), WORTH), Some(vec![1, 2]));
    }
}
