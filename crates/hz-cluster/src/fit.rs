//! Fitting the elements to what an object actually radiates.
//!
//! The other two weightings answer a question about the object's *direction*:
//! move it to an element, or pan it over several. Neither answers the question
//! an object with a width poses, which is about its **directional energy** —
//! the shape of what it radiates, not the middle of it. Measured, both make a
//! wide object worse than leaving it alone.
//!
//! # What is fitted, and to what
//!
//! Rendering is linear in the object signals, so what a listener gets from
//! element `k` carrying weight `w_k` is `w_k · g(q_k)`, and what they should
//! have got is `g(p, size)`. Choosing the weights is therefore a least-squares
//! problem — and the only thing stopping it being solved directly is that the
//! encoder does not know which layout `g` will be.
//!
//! So it is solved against a **reference layout** instead — and which one is
//! not a detail. The obvious choice, a set of directions spread evenly over the
//! sphere, does not work: measured on 7.1.4, a wide object fitted against
//! twenty-four uniform directions costs 0.784, barely better than ignoring its
//! width. Panning with a width is not the same operator on two different
//! layouts, so a fit made on an idealised one is not a fit on a real one.
//!
//! Fitted instead against an ordinary immersive layout — the directions of a
//! 7.1.4 — the same object costs **0.121 on 5.1 and 0.137 on 7.1**, against
//! 0.633 and 0.669 for the rule that ignores its width. Neither of those is the
//! layout it was fitted against, which is what says it transfers rather than
//! memorises.
//!
//! # Why a fit is held towards the last one
//!
//! Solved from cold each block, the fit is free to answer a different question
//! every 26.7 ms. For an object sitting between three near-equidistant
//! elements the residual barely distinguishes `{A,B}` from `{A,C}`, so the
//! active set flips back and forth: [`crate::mix`] cross-fades the amplitude,
//! which stops the click, but the signal still moves from one channel to
//! another twenty-five times a second for no reason the scene gave.
//!
//! Nothing constrained that. The hysteresis holds an object's *element* and
//! the assignment holds an element's *number*; the weights had no memory at
//! all.
//!
//! A flip is an element going from **something to nothing**, which is a
//! property of the fit's *active set* rather than of the weights in it — so
//! what has to be sticky is the set. The fit is solved twice when there is a
//! previous answer: once free, and once confined to the elements the last
//! block used. If confining it costs no more than [`HELD_TOLERANCE`] of the
//! residual, the confined answer is taken and the object stays in the channels
//! it was in.
//!
//! The textbook answer — a Tikhonov pull towards the previous weights — was
//! written first and makes the flips *worse*. See [`HELD_TOLERANCE`] for what
//! it measured and why.
//!
//! # Why the weights may not be negative
//!
//! A negative weight subtracts an object from an element that other objects
//! are already in. The arithmetic allows it and the fit would use it: cancelling
//! solutions fit beautifully and fall apart as soon as anything else moves. So
//! the fit is constrained, which is what makes it non-negative least squares
//! rather than least squares.

use hz_core::Result;
use hz_render::{PointPanner, Renderer};

/// Directions of a uniform sphere, kept for the comparison that ruled it out.
///
/// Twenty-four is every direction the panner can hold, and it is finer than
/// any delivery layout — 7.1.4 has eleven that point anywhere. Finer did not
/// help: see the note at the top of this module.
const REFERENCE_DIRECTIONS: usize = 24;

/// The reference the fit is made against.
///
/// Built once and reused for every object and every block. The triangulation
/// is the expensive part and none of it depends on the scene.
///
/// Two shapes. A **set of directions** with a panner onto them, which is what
/// this was: a fit against an ordinary immersive layout, made in the hope that
/// it transfers to whatever is in the room. And the **presentations
/// themselves**, stacked: the gain vectors a delivery stream actually produces
/// on 2.0, 5.1, 7.1 and 7.1.4, one after another in one tall vector, so the
/// fit optimises exactly what the metric measures rather than a proxy for it.
pub enum Reference {
    /// A panner onto a set of directions.
    Directions { panner: PointPanner, rows: usize },
    /// Every presentation the stream is played through, stacked.
    Presentations {
        renderers: Vec<Box<dyn Renderer>>,
        rows: usize,
    },
}

impl Reference {
    /// The reference the fit is made against: the presentations themselves.
    ///
    /// A delivery stream is played through four of them and three are folds.
    /// Fitting against a set of *directions* — even a good one — optimises a
    /// proxy: the fit restores the level against those directions and the
    /// folds then come out somewhere else. Fitting against the stacked gain
    /// vectors of the presentations optimises the quantity the metric reports,
    /// which is the definition of the right reference.
    pub fn new() -> Result<Self> {
        Self::over_presentations(crate::metric::delivery()?)
    }

    /// A reference over given presentations, stacked.
    pub fn over_presentations(renderers: Vec<Box<dyn Renderer>>) -> Result<Self> {
        let rows = renderers.iter().map(|r| r.channels()).sum();
        Ok(Self::Presentations { renderers, rows })
    }

    /// The reference this was: an ordinary immersive layout's directions.
    ///
    /// Kept because it is what the note at the top of this module measured,
    /// and because a fit against directions is the only shape available to an
    /// encoder that does not know its presentations.
    pub fn immersive_layout() -> Result<Self> {
        Self::over(&hz_render::speakers_of(&hz_render::Layout::surround_7_1_4()))
    }

    /// A uniform sphere, which is the reference that does **not** work. Kept
    /// so the comparison can be made rather than described.
    pub fn uniform() -> Result<Self> {
        Self::over(&sphere(REFERENCE_DIRECTIONS))
    }

    /// A reference over a given set of directions.
    ///
    /// Which set matters more than it should. A uniform sphere is the
    /// layout-independent choice and it does not transfer: fitted against 24
    /// evenly spread directions and measured on 7.1.4, a wide object costs
    /// 0.784, against 0.237 for the same fit made on 7.1.4 itself. Panning
    /// with a width is not the same operator on two different layouts, so a fit
    /// made on one is not a fit on the other.
    pub fn over(points: &[[f64; 3]]) -> Result<Self> {
        Ok(Self::Directions {
            panner: PointPanner::new(points)?,
            rows: points.len(),
        })
    }

    /// How tall the fitted vector is.
    pub fn rows(&self) -> usize {
        match self {
            Self::Directions { rows, .. } | Self::Presentations { rows, .. } => *rows,
        }
    }

    /// What a source at `position` with this `size` radiates, as one vector
    /// over whatever the reference is.
    pub fn radiated(&self, position: [f64; 3], size: f64, out: &mut Vec<f64>) {
        out.clear();
        match self {
            Self::Directions { panner, rows } => {
                let mut gains = vec![0.0f32; *rows];
                panner.gains(position, size, &mut gains);
                out.extend(gains.iter().map(|gain| f64::from(*gain)));
            }
            Self::Presentations { renderers, rows } => {
                out.resize(*rows, 0.0);
                let mut at = 0;
                for renderer in renderers {
                    let channels = renderer.channels();
                    renderer.gains(position, size, &mut out[at..at + channels]);
                    at += channels;
                }
            }
        }
    }
}

/// Points spread evenly over the sphere by the golden angle.
fn sphere(count: usize) -> Vec<[f64; 3]> {
    let golden = std::f64::consts::PI * (3.0 - 5f64.sqrt());
    (0..count)
        .map(|index| {
            let z = 1.0 - 2.0 * (index as f64 + 0.5) / count as f64;
            let radius = (1.0 - z * z).max(0.0).sqrt();
            let angle = golden * index as f64;
            [radius * angle.cos(), radius * angle.sin(), z]
        })
        .collect()
}

/// The elements the last block gave this object, and how much worse a fit
/// confined to them may be before it is given up.
#[derive(Debug, Clone, Copy)]
pub struct Held<'a> {
    /// The previous block's weight row, over the same elements as this one's.
    /// Which of them are non-zero is the set; how large they were is not used.
    pub previous: &'a [f64],
    /// How much residual a fit inside that set may cost, as a fraction.
    pub tolerance: f64,
}

/// How much worse a fit may be for keeping the elements it had.
///
/// A fraction of the free answer's residual, so 0.5 is "half again as far from
/// the target". That sounds generous and is not: it only ever applies where
/// the free answer and the held one are both nearly right, because an object
/// the held set genuinely cannot reach has a residual many times larger and
/// loses.
///
/// Chosen the way everything here is chosen — the largest value that does not
/// cost error, then the smallest value that reaches the same benefit. On
/// `crowd40` into twelve elements, which is the only scene in the tree where
/// objects share elements and move at once, with the presentations' means
/// printed to five decimals so that "does not cost" is a measurement and not a
/// rounding:
///
/// | tolerance | 2.0 | 5.1 | 7.1 | 7.1.4 | flips a block |
/// |---|---|---|---|---|---|
/// | off | 0.01926 | 0.03456 | 0.04795 | 0.07426 | 4.74 |
/// | 0.01 | 0.01926 | 0.03456 | 0.04794 | 0.07426 | 3.39 |
/// | 0.1 | 0.01926 | 0.03456 | 0.04794 | 0.07426 | 3.37 |
/// | 0.3 | 0.01926 | 0.03456 | 0.04795 | 0.07426 | 2.97 |
/// | **0.5** | **0.01926** | **0.03456** | **0.04795** | **0.07426** | **2.79** |
/// | 1.0 | 0.01926 | 0.03456 | 0.04795 | 0.07426 | 2.79 |
/// | 2.0 | 0.02003 | 0.03561 | 0.04924 | 0.07511 | 1.27 |
///
/// Nothing above a half buys anything until two, where it starts paying — four
/// per cent of the mean, which is the set being held past the point where the
/// scene has left it. At sixteen elements the same value takes 5.40 flips a
/// block to 2.23 with the error unmoved, and on the two masters whose fold is
/// the identity it changes nothing, as it should: there is no second set to
/// choose from when every object has an element to itself.
///
/// # The obvious answer, measured and rejected
///
/// The textbook way to hold a least-squares answer near a previous one is a
/// Tikhonov pull towards it — `‖A w − b‖² + λ‖w − w_prev‖²`, which is still a
/// non-negative problem and still the same solver, with λ folded into its
/// inner products. It was written first, and it makes the flips **worse**:
///
/// | λ | mean error | worst | flips a block | paths |
/// |---|---|---|---|---|
/// | 0 | 0.044 | 0.377 | **4.74** | 2.22 |
/// | 0.01 | 0.044 | 0.377 | 5.40 | 2.23 |
/// | 0.03 | 0.044 | 0.377 | 6.16 | 2.24 |
/// | 0.1 | 0.045 | 0.378 | 7.50 | 2.28 |
/// | 0.3 | 0.047 | 0.396 | 8.27 | 2.32 |
/// | 1.0 | 0.057 | 0.646 | 8.03 | 2.47 |
/// | 3.0 | 0.089 | 0.879 | 9.37 | 2.67 |
///
/// The reason is in the gradient. A pull towards `w_prev` adds `λ w_prev[k]`
/// to the term that decides which element to free next, so it frees elements
/// the audio did not ask for merely because the last block used them — and
/// they come back with a weight near zero, which dies at the next block that
/// does not want it either. It manufactures exactly the event it was meant to
/// suppress, and the paths column shows it happening: 2.22 elements an object
/// at λ = 0, 2.67 at λ = 3.
///
/// A flip is not a large weight becoming a slightly different large weight. It
/// is an element going from **something to nothing**, which is a property of
/// the *active set* and not of the weights in it — so what has to be sticky is
/// the set, and a penalty on the weights is the wrong instrument however well
/// it is tuned.
pub const HELD_TOLERANCE: f64 = 0.5;

/// Fit the elements to what an object radiates, and put the level back.
///
/// A least-squares fit is the *projection* of the target onto what the
/// elements can reach, and a projection is shorter than what it approximates
/// whenever the target is not reachable — which for a width it never is. So the
/// raw fit is quieter than the object was, by eleven per cent on the first
/// scene it was tried on, and a fold that changes an object's level is wrong
/// however good its shape.
///
/// The shape is the part worth keeping, so it is kept and the level is put
/// back: scale the weights until what they radiate is as loud as what the
/// object did. Nothing about the fit's direction or spread changes.
pub fn fitted(radiated: &[Vec<f64>], target: &[f64]) -> Vec<f64> {
    fitted_within(radiated, target, MAX_PATHS)
}

/// As [`fitted`], keeping the elements the last block used when they are
/// nearly as good, and reaching only the elements `allowed` permits — the
/// ones of the object's class, when there are classes.
pub fn fitted_near(
    radiated: &[Vec<f64>],
    target: &[f64],
    held: Option<Held<'_>>,
    allowed: Option<&[bool]>,
) -> Vec<f64> {
    fitted_within_near(radiated, target, MAX_PATHS, held, allowed)
}

/// How many elements one object may be spread over.
///
/// A fit left to itself uses as many elements as reduce the residual at all,
/// and the last few contribute almost nothing to the shape while costing a
/// great deal elsewhere: every element an object reaches is a path its signal
/// takes to a speaker, and paths that a wider layout keeps apart arrive
/// together on a narrower one. Three is what vector-base amplitude panning
/// itself uses — a triangle — and it is what the cap is set to. What the caps
/// cost is measured in `a_cap_on_how_many_elements_an_object_reaches`.
pub const MAX_PATHS: usize = 3;

/// As [`fitted`], with a cap on how many elements may carry the object.
pub fn fitted_within(radiated: &[Vec<f64>], target: &[f64], paths: usize) -> Vec<f64> {
    fitted_within_near(radiated, target, paths, None, None)
}

/// As [`fitted_within`], keeping the elements the last block used when they
/// are nearly as good.
///
/// The level is put back **after** the set has been chosen, as it is without
/// one: which elements carry the object is decided on the residual, and
/// whichever set wins is then scaled to be as loud as the object was. A held
/// fit that came out quiet would otherwise be a fold that turns an object down
/// for having stayed in its channels.
pub fn fitted_within_near(
    radiated: &[Vec<f64>],
    target: &[f64],
    paths: usize,
    held: Option<Held<'_>>,
    allowed: Option<&[bool]>,
) -> Vec<f64> {
    fitted_bounded(radiated, target, paths, held, allowed, None)
}

/// As [`fitted_within_near`], with an upper bound on each element's weight.
///
/// The bound is what keeps an element's peak inside the codec's domain — see
/// [`crate::headroom`] — and it is honoured the way a bounded least-squares
/// solve honours one: whatever the free answer puts past its bound is pinned
/// at the bound, what the pinned elements now radiate is taken off the
/// target, and the rest are solved again for what is left, until nothing is
/// over. A weight pinned at its bound stays there, which is the one thing
/// the full method of Stark and Parker would reconsider and this does not;
/// with sixteen elements at most and a bound on one or two of them it makes
/// no difference that has been measured.
///
/// The level is put back afterwards as far as the bounds allow: the weights
/// are scaled up to the object's own level, or to the first bound they meet,
/// whichever comes first. An object that comes out quieter for it is an
/// object the domain had no room for, and the metric's level column says so.
pub fn fitted_bounded(
    radiated: &[Vec<f64>],
    target: &[f64],
    paths: usize,
    held: Option<Held<'_>>,
    allowed: Option<&[bool]>,
    bounds: Option<&[f64]>,
) -> Vec<f64> {
    let n = radiated.len();
    let mut weights = nnls_within_near(radiated, target, paths, held, allowed);
    let bounds = bounds.filter(|bounds| bounds.len() == n);

    if let Some(bounds) = bounds {
        let mut pinned = vec![false; n];
        let mut rest = Vec::new();
        let mut free_allowed = Vec::new();
        for _ in 0..n {
            let over: Vec<usize> = (0..n)
                .filter(|k| !pinned[*k] && weights[*k] > bounds[*k])
                .collect();
            if over.is_empty() {
                break;
            }
            for k in over {
                pinned[k] = true;
                weights[k] = bounds[k];
            }
            // What is left of the target once the pinned elements have
            // radiated their bounded share of it.
            rest.clear();
            rest.extend_from_slice(target);
            for (k, column) in radiated.iter().enumerate() {
                if pinned[k] {
                    for (slot, value) in rest.iter_mut().zip(column) {
                        *slot -= bounds[k] * value;
                    }
                }
            }
            free_allowed.clear();
            free_allowed
                .extend((0..n).map(|k| !pinned[k] && allowed.is_none_or(|allowed| allowed[k])));
            let paths_left = paths.saturating_sub(pinned.iter().filter(|p| **p).count());
            let solved = nnls_within_near(radiated, &rest, paths_left, None, Some(&free_allowed));
            for k in 0..n {
                if !pinned[k] {
                    weights[k] = solved[k];
                }
            }
        }
    }

    // The level put back, as far as the bounds allow.
    let mut got = vec![0.0; target.len()];
    for (column, weight) in radiated.iter().zip(&weights) {
        if *weight != 0.0 {
            for (slot, value) in got.iter_mut().zip(column) {
                *slot += weight * value;
            }
        }
    }
    let reached: f64 = got.iter().map(|v| v * v).sum::<f64>().sqrt();
    let wanted: f64 = target.iter().map(|v| v * v).sum::<f64>().sqrt();
    if reached > 1e-12 {
        let mut scale = wanted / reached;
        if let Some(bounds) = bounds {
            for (weight, bound) in weights.iter().zip(bounds) {
                if *weight > 0.0 && bound.is_finite() {
                    scale = scale.min(bound / weight);
                }
            }
        }
        for weight in &mut weights {
            *weight *= scale;
        }
    }
    weights
}

/// Solve `min ‖A·x − b‖` for `x ≥ 0`, by Lawson and Hanson's method.
///
/// `a` is column-major: `a[k]` is what element `k` radiates over the reference
/// directions, and `b` is what the object radiates. Both have the same length.
///
/// The method: start with everything at zero, repeatedly free whichever
/// element would most reduce the residual, solve for the freed ones without the
/// constraint, and if that asks for a negative weight, walk back to the
/// boundary and pin whatever hit zero. It terminates because a set of freed
/// elements is never visited twice.
pub fn nnls(a: &[Vec<f64>], b: &[f64]) -> Vec<f64> {
    nnls_within(a, b, usize::MAX)
}

/// As [`nnls`], stopping once `paths` elements have been freed.
///
/// The cap is on the *active set*, not on the answer afterwards: freeing a
/// fourth element and then dropping the smallest weight is a different and
/// worse solution than never freeing it, because the three that remain are
/// solved without it.
pub fn nnls_within(a: &[Vec<f64>], b: &[f64], paths: usize) -> Vec<f64> {
    nnls_within_near(a, b, paths, None, None)
}

/// As [`nnls_within`], keeping the elements a previous answer used when they
/// are nearly as good.
///
/// Solved twice: once free, and once with only the previous answer's elements
/// allowed to be freed. If confining it costs no more than `tolerance` of the
/// free answer's residual, the confined answer is the one returned — so an
/// object between three near-equidistant elements stops trading two of them
/// back and forth every block for a difference the residual cannot see.
///
/// The second solve costs what the first does, and only runs when there is a
/// previous set to try and the free answer did not land on it anyway.
///
/// `allowed`, when given, is which elements the object may reach at all —
/// the ones of its class — and both solves stay inside it.
pub fn nnls_within_near(
    a: &[Vec<f64>],
    b: &[f64],
    paths: usize,
    held: Option<Held<'_>>,
    allowed: Option<&[bool]>,
) -> Vec<f64> {
    let n = a.len();
    let allowed = allowed.filter(|allowed| allowed.len() == n);
    let free = solve_within(a, b, paths, allowed);
    // A previous row of the wrong width came from a block with a different set
    // of elements, and there is nothing in it to keep.
    let Some(held) = held.filter(|held| held.previous.len() == n && held.tolerance >= 0.0) else {
        return free;
    };
    let kept_set: Vec<bool> = held
        .previous
        .iter()
        .enumerate()
        .map(|(index, weight)| *weight != 0.0 && allowed.is_none_or(|allowed| allowed[index]))
        .collect();
    if !kept_set.iter().any(|kept| *kept) {
        return free;
    }
    // The free answer already stayed where it was: there is nothing to choose
    // between, and no reason to solve it again to find that out.
    if (0..n).all(|index| (free[index] != 0.0) == kept_set[index]) {
        return free;
    }

    let kept = solve_within(a, b, paths, Some(&kept_set));
    if residual_of(a, b, &kept) <= residual_of(a, b, &free) * (1.0 + held.tolerance) {
        kept
    } else {
        free
    }
}

/// How far a set of weights lands from the target.
fn residual_of(a: &[Vec<f64>], b: &[f64], x: &[f64]) -> f64 {
    let mut residual = b.to_vec();
    for (column, weight) in a.iter().zip(x) {
        if *weight != 0.0 {
            for (slot, value) in residual.iter_mut().zip(column) {
                *slot -= weight * value;
            }
        }
    }
    residual.iter().map(|r| r * r).sum::<f64>().sqrt()
}

/// The solver itself: Lawson and Hanson over the columns `allowed` permits.
///
/// `allowed` of `None` is every column, which is the plain problem. A mask
/// confines the active set to a stated subset without changing anything else
/// about the method — the freed columns are still solved together and without
/// the constraint, which is what makes a confined answer a real answer rather
/// than a free one with terms struck out.
fn solve_within(a: &[Vec<f64>], b: &[f64], paths: usize, allowed: Option<&[bool]>) -> Vec<f64> {
    let n = a.len();
    let mut x = vec![0.0; n];
    if n == 0 {
        return x;
    }
    let m = b.len();
    let permitted = |index: usize| allowed.is_none_or(|allowed| allowed[index]);

    // How much freeing each element would reduce the residual, which is what
    // decides the order they are tried in.
    let gradient = |x: &[f64], out: &mut Vec<f64>| {
        let mut residual = b.to_vec();
        for (column, weight) in a.iter().zip(x) {
            if *weight != 0.0 {
                for (slot, value) in residual.iter_mut().zip(column) {
                    *slot -= weight * value;
                }
            }
        }
        out.clear();
        out.extend(a.iter().map(|column| {
            column
                .iter()
                .zip(&residual)
                .map(|(v, r)| v * r)
                .sum::<f64>()
        }));
    };

    let mut free = vec![false; n];
    let mut w = Vec::with_capacity(n);
    let tolerance = 1e-9 * b.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-12);

    // At most one pass per element, plus room for the boundary walks; a bound
    // rather than a hope, so a solve cannot run away on strange input.
    for _ in 0..3 * n + 8 {
        gradient(&x, &mut w);
        let candidate = (0..n)
            .filter(|index| !free[*index] && permitted(*index))
            .max_by(|a, b| {
                w[*a]
                    .partial_cmp(&w[*b])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(candidate) = candidate.filter(|index| w[*index] > tolerance) else {
            break;
        };
        if free.iter().filter(|freed| **freed).count() >= paths {
            break;
        }
        free[candidate] = true;

        loop {
            let solved = least_squares(a, b, &free, m);
            if free
                .iter()
                .enumerate()
                .all(|(index, freed)| !freed || solved[index] > 0.0)
            {
                x = solved;
                break;
            }
            // Walk from the current point towards the unconstrained answer
            // until the first weight hits zero, and pin everything that did.
            let step = (0..n)
                .filter(|index| free[*index] && solved[*index] <= 0.0)
                .map(|index| x[index] / (x[index] - solved[index]))
                .fold(f64::INFINITY, f64::min);
            let step = if step.is_finite() { step } else { 0.0 };
            for index in 0..n {
                x[index] += step * (solved[index] - x[index]);
                if free[index] && x[index] <= 1e-12 {
                    free[index] = false;
                    x[index] = 0.0;
                }
            }
            if !free.iter().any(|freed| *freed) {
                break;
            }
        }
    }

    for (weight, freed) in x.iter_mut().zip(&free) {
        if !freed {
            *weight = 0.0;
        }
    }
    x
}

/// Unconstrained least squares over the freed elements only, by the normal
/// equations.
///
/// The freed set is at most sixteen wide — the elements a bitstream carries —
/// so the normal equations are a sixteen-by-sixteen solve and their worse
/// conditioning costs nothing that matters here.
fn least_squares(a: &[Vec<f64>], b: &[f64], free: &[bool], _rows: usize) -> Vec<f64> {
    let columns: Vec<usize> = (0..a.len()).filter(|index| free[*index]).collect();
    let size = columns.len();
    let mut normal = vec![0.0; size * size];
    let mut rhs = vec![0.0; size];

    for (row, column) in columns.iter().enumerate() {
        rhs[row] = a[*column].iter().zip(b).map(|(v, t)| v * t).sum();
        for (other_row, other) in columns.iter().enumerate() {
            normal[row * size + other_row] =
                a[*column].iter().zip(&a[*other]).map(|(u, v)| u * v).sum();
        }
    }
    // A ridge that is negligible against the scale of the problem, so that a
    // freed set with two elements in the same place has an answer rather than
    // a division by zero.
    for row in 0..size {
        normal[row * size + row] += 1e-12;
    }

    let solved = solve(&mut normal, &mut rhs, size);
    let mut out = vec![0.0; a.len()];
    if solved {
        for (row, column) in columns.iter().enumerate() {
            out[*column] = rhs[row];
        }
    }
    out
}

/// Gaussian elimination with partial pivoting, in place.
fn solve(matrix: &mut [f64], rhs: &mut [f64], size: usize) -> bool {
    for step in 0..size {
        let pivot = (step..size)
            .max_by(|a, b| {
                matrix[a * size + step]
                    .abs()
                    .partial_cmp(&matrix[b * size + step].abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(step);
        if matrix[pivot * size + step].abs() < 1e-14 {
            return false;
        }
        if pivot != step {
            for column in 0..size {
                matrix.swap(step * size + column, pivot * size + column);
            }
            rhs.swap(step, pivot);
        }
        for row in step + 1..size {
            let factor = matrix[row * size + step] / matrix[step * size + step];
            if factor == 0.0 {
                continue;
            }
            for column in step..size {
                matrix[row * size + column] -= factor * matrix[step * size + column];
            }
            rhs[row] -= factor * rhs[step];
        }
    }
    for step in (0..size).rev() {
        let mut value = rhs[step];
        for column in step + 1..size {
            value -= matrix[step * size + column] * rhs[column];
        }
        rhs[step] = value / matrix[step * size + step];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A problem whose answer is obvious: the target is one column, so that
    /// column gets one and the rest get nothing.
    #[test]
    fn a_target_that_is_a_column_picks_that_column() {
        let a = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let x = nnls(&a, &[0.0, 1.0, 0.0]);
        assert!((x[1] - 1.0).abs() < 1e-9, "{x:?}");
        assert!(x[0] < 1e-9 && x[2] < 1e-9, "{x:?}");
    }

    /// A target between two columns is shared between them, in the proportion
    /// it sits at.
    #[test]
    fn a_target_between_two_columns_is_shared() {
        let a = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let x = nnls(&a, &[0.3, 0.7]);
        assert!(
            (x[0] - 0.3).abs() < 1e-9 && (x[1] - 0.7).abs() < 1e-9,
            "{x:?}"
        );
    }

    /// The constraint doing its job: unconstrained, this problem wants a
    /// negative weight, and the answer must not have one.
    #[test]
    fn nothing_comes_back_negative() {
        // Two columns pointing the same way, and a target between them but
        // beyond one — the unconstrained fit subtracts.
        let a = vec![vec![1.0, 1.0], vec![1.0, 0.0]];
        let x = nnls(&a, &[0.0, 1.0]);
        assert!(x.iter().all(|w| *w >= 0.0), "{x:?}");
        // And it is the best such answer: the residual beats pinning either.
        let residual = |x: &[f64]| {
            (0..2)
                .map(|row| {
                    let got: f64 = a.iter().zip(x).map(|(col, w)| col[row] * w).sum();
                    let want = [0.0, 1.0][row];
                    (got - want) * (got - want)
                })
                .sum::<f64>()
        };
        assert!(residual(&x) <= residual(&[0.0, 1.0]) + 1e-12, "{x:?}");
        assert!(residual(&x) <= residual(&[1.0, 0.0]) + 1e-12, "{x:?}");
    }

    /// An exactly solvable overdetermined problem, so that the solve itself is
    /// checked and not just the constraint.
    #[test]
    fn an_overdetermined_problem_is_solved_exactly() {
        let a = vec![
            vec![1.0, 2.0, 3.0, 4.0],
            vec![0.0, 1.0, 1.0, 2.0],
            vec![1.0, 0.0, 2.0, 1.0],
        ];
        let wanted = [0.5, 2.0, 0.25];
        let b: Vec<f64> = (0..4)
            .map(|row| a.iter().zip(&wanted).map(|(col, w)| col[row] * w).sum())
            .collect();
        let x = nnls(&a, &b);
        for (got, want) in x.iter().zip(&wanted) {
            assert!((got - want).abs() < 1e-6, "{x:?} against {wanted:?}");
        }
    }

    /// A set that fits nearly as well is kept, and one that does not is not.
    ///
    /// Two columns either side of a target and a third far away. Held to the
    /// far one, the fit has to give it up — no tolerance covers a residual
    /// that large. Held to one of the two near ones, it stays there, because
    /// half again as far from the target is a difference the residual can see
    /// and a listener cannot.
    #[test]
    fn a_set_is_kept_when_it_costs_little_and_dropped_when_it_costs_much() {
        // A fourth row no column reaches, so that no set fits exactly: with an
        // exact free answer no tolerance can keep anything, since half again
        // as far as nothing is nothing.
        let a = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.8, 0.6, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
        ];
        let target = [1.0, 0.05, 0.0, 0.4];

        // Free, this reaches for the first two columns.
        let free = nnls_within(&a, &target, 2);
        assert!(
            free[0] != 0.0 && free[1] != 0.0 && free[2] == 0.0,
            "{free:?}"
        );

        // Held to the first column alone, which is 0.8 % further out: kept.
        let kept = nnls_within_near(
            &a,
            &target,
            2,
            Some(Held {
                previous: &[1.0, 0.0, 0.0],
                tolerance: HELD_TOLERANCE,
            }),
            None,
        );
        assert!(kept[1] == 0.0 && kept[2] == 0.0, "{kept:?}");

        // Held to the third column, which points somewhere else entirely and
        // is two and a half times further out: given up.
        let given_up = nnls_within_near(
            &a,
            &target,
            2,
            Some(Held {
                previous: &[0.0, 0.0, 1.0],
                tolerance: HELD_TOLERANCE,
            }),
            None,
        );
        assert_eq!(
            given_up, free,
            "a set that cannot reach the target was kept"
        );
    }

    /// A held set that the free answer lands on anyway costs nothing to hold,
    /// and holding nothing is the same problem as not holding.
    #[test]
    fn holding_nothing_is_the_plain_problem() {
        let a = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let target = [0.3, 0.7, 0.0];
        let free = nnls_within(&a, &target, 3);
        for previous in [vec![0.0; 3], vec![0.0; 2]] {
            let held = nnls_within_near(
                &a,
                &target,
                3,
                Some(Held {
                    previous: &previous,
                    tolerance: HELD_TOLERANCE,
                }),
                None,
            );
            assert_eq!(held, free, "holding {previous:?} changed the answer");
        }
    }

    /// The reference has to be a set a panner can actually work with, and one
    /// whose directions are spread rather than bunched.
    #[test]
    fn the_reference_is_a_usable_sphere() {
        let points = sphere(REFERENCE_DIRECTIONS);
        assert_eq!(points.len(), REFERENCE_DIRECTIONS);
        for point in &points {
            let length = (point[0] * point[0] + point[1] * point[1] + point[2] * point[2]).sqrt();
            assert!((length - 1.0).abs() < 1e-9, "{point:?} is not a direction");
        }
        // No two closer than a few degrees, which is what "spread" has to mean
        // for a triangulation to have something to work with.
        for (index, a) in points.iter().enumerate() {
            for b in &points[index + 1..] {
                let cosine = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
                assert!(cosine < 0.999, "two reference directions coincide");
            }
        }
        Reference::new().expect("the reference carries a panner");
    }

    /// And an object with no width fitted onto elements that include its own
    /// position should land on that element and nowhere else.
    #[test]
    fn a_point_on_an_element_fits_to_that_element() {
        let reference = Reference::new().unwrap();
        let elements = [
            [0.0, 1.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
        ];
        let mut columns = Vec::new();
        let mut scratch = Vec::new();
        for element in &elements {
            reference.radiated(*element, 0.0, &mut scratch);
            columns.push(scratch.clone());
        }
        let mut target = Vec::new();
        reference.radiated(elements[1], 0.0, &mut target);

        let x = nnls(&columns, &target);
        assert!((x[1] - 1.0).abs() < 0.02, "{x:?}");
        for (index, weight) in x.iter().enumerate() {
            if index != 1 {
                assert!(*weight < 0.02, "element {index} got {weight}");
            }
        }
    }
}
