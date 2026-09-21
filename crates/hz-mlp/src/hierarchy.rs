//! The hierarchy of presentations the stored channels have to be.
//!
//! # Why the stored channels are not the elements
//!
//! A decoder stopping after substream `s` has that substream's channels **and
//! no others**, so the matrices it applies can only read those. Substream 0 of
//! an object programme carries two channels: a presentation stated there is a
//! 2×2 mix of those two, and no arrangement of *elements* can make a fold of
//! twelve out of it.
//!
//! Which was measured, not assumed. Arranging twelve elements — a ring at ear
//! height and four overhead — so that the leading channels carry the 2.0, 5.1
//! and 7.1 folds gets six of the seven rows in and stops. The two it cannot
//! place are the ones the 7.1 adds, and they fail because they reach the
//! **height** elements, which are stored in channels 8 to 11 and outside the
//! eight the 7.1 carries. No permutation fixes that: the heights have to be
//! folded into the channels the 7.1 carries *before* any arrangement runs.
//!
//! So the stored channels are a **hierarchy of presentations**. Channels 0 and
//! 1 carry the stereo fold itself; channels 2 to 5 carry what the 5.1 has that
//! the stereo did not; 6 and 7 what the 7.1 adds; the rest what restores the
//! elements. Each presentation reads a prefix, and its own matrix set turns
//! that prefix into its channels — the 5.1's left is its stereo left less the
//! centre and surround it was folded with, which is one lifting step.
//!
//! # What has to be true, exactly
//!
//! For presentation `k` carrying `n` channels:
//!
//! ```text
//! span{ stored[0], …, stored[n-1] }  ⊇  span{ presentation k's rows }
//! ```
//!
//! A superset is fine and a subset is fatal. The prefix has to *reach* every
//! row, since the presentation's matrix can only combine what it reads; it may
//! reach further, and it always does — the prefix that spans a 5.1 also spans
//! the stereo that was folded out of it.
//!
//! # How it is built
//!
//! Narrowest presentation first, each row reduced against the rows already
//! chosen. A row that adds a dimension is stored as it stands, so channel 2
//! carries the centre rather than some rotation of it; a row already in the
//! span is skipped, having nothing to carry.
//!
//! Two things fall out of that rather than being handled:
//!
//! - **A presentation can need fewer channels than it carries.** The low
//!   frequency channel of a 5.1 takes nothing from an object, so on a
//!   programme with no low frequency bed its row is zero and adds no
//!   dimension. The channel is filled with an element instead, which is what
//!   the last presentation needed there anyway.
//! - **The rows the last presentation wants come free.** By the time it is
//!   reached the prefix spans everything the narrower ones needed, and
//!   whatever is left over is completed with elements — which is exactly "what
//!   restores the elements", stated as a basis rather than as an intention.

use crate::arrange::expand;
use crate::matrix::{FRACTION, Primitive};

/// One presentation of the hierarchy.
#[derive(Debug, Clone, PartialEq)]
pub struct Presentation {
    /// How many stored channels a decoder stopping here has.
    pub channels: usize,
    /// What this presentation's channels are, as combinations of the elements.
    ///
    /// One row per channel it carries, in its own channel order. A row of
    /// zeros is allowed and means what it says: that channel takes nothing
    /// from the elements.
    pub rows: Vec<Vec<f64>>,
}

/// What the stored channels carry, and which presentation first needs each.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hierarchy {
    /// What each stored channel carries, as a combination of the elements.
    /// One per element, since the whole has to be invertible.
    pub rows: Vec<Vec<f64>>,
    /// The narrowest presentation that carries each channel, as a channel
    /// count. This is what [`crate::arrange::arrange`] takes as `within`.
    pub within: Vec<usize>,
    /// Which rows are fill: a channel no presentation's row took, holding an
    /// element — and *which* element is free, so a caller choosing them
    /// jointly with the hosts can say. See [`build_preferring`].
    pub filled: Vec<bool>,
}

/// How small a residual counts as nothing left.
///
/// A row is kept when reducing it against the rows already chosen leaves
/// something; this is what "something" means, relative to the row's own size,
/// so it does not depend on how loud the programme is.
const NOTHING_LEFT: f64 = 1e-9;

/// Build the hierarchy the stored channels have to be.
///
/// `presentations` must be given narrowest first and their channel counts must
/// increase; the last one is expected to carry every channel there is. Returns
/// `None` if they are not ordered, if one wants more channels than there are
/// elements, or if a row is not written over the elements.
pub fn build(presentations: &[Presentation], elements: usize) -> Option<Hierarchy> {
    let as_they_come: Vec<usize> = (0..elements).collect();
    build_preferring(presentations, elements, &as_they_come)
}

/// [`build`], filling the channels no presentation's row takes with elements
/// in the order `prefer` gives them.
///
/// Which elements fill those channels is free, and it is not nothing: a fill
/// row reaches one element and nothing else, so whichever element it takes is
/// an element no fold row can be hosted on. Filling with the elements the
/// folds want *least* leaves the ones they want most for hosting — which is
/// what keeps a stereo row on the element that carries half of it rather than
/// on one that carries none. See [`crate::arrange::order_elements`].
pub fn build_preferring(
    presentations: &[Presentation],
    elements: usize,
    prefer: &[usize],
) -> Option<Hierarchy> {
    if elements == 0 || prefer.len() != elements {
        return None;
    }
    let mut out = Hierarchy {
        rows: Vec::with_capacity(elements),
        within: Vec::with_capacity(elements),
        filled: Vec::with_capacity(elements),
    };
    // The rows already chosen, in row echelon form, to test what a candidate
    // adds. Kept beside `out.rows` rather than derived from it, so a candidate
    // costs one reduction instead of a solve.
    let mut echelon: Vec<Vec<f64>> = Vec::with_capacity(elements);

    let mut widest = 0;
    for presentation in presentations {
        if presentation.channels <= widest && !out.rows.is_empty() {
            return None;
        }
        if presentation.channels > elements {
            return None;
        }
        widest = presentation.channels;

        for row in &presentation.rows {
            if row.len() != elements {
                return None;
            }
        }

        // The widest presentation is the elements themselves, and which
        // element goes in which of its channels is not its business: the
        // channel assignment puts them in order on the way out. So its
        // channels are filled in the order preferred, which is what lets the
        // elements the folds want least be the ones no fold row is hosted
        // on. Taking its own row `j` for channel `j` instead filled the last
        // four channels with elements eight to eleven by number, whatever
        // they carried — measured as the two silent elements of a scene
        // hosting its stereo rows at a sixteenth.
        if presentation.channels >= elements {
            fill(&mut out, &mut echelon, elements, prefer);
            continue;
        }

        // 🔴 Channel `j` takes this presentation's own channel `j`, where it
        // adds one at all. Which looks like a detail and is what decides
        // whether the presentation can be *written*: a decoder computes each
        // output in a channel, and a channel can only compute an output it
        // holds some of. Store the centre in the channel the surround comes out
        // of and the two have to be swapped, which costs three matrices where
        // one would do.
        //
        // Taking whichever row adds a dimension next — which is what this did —
        // stores the 5.1's *left* in the first channel it adds, though its left
        // comes out where the stereo's does. The rows that go in are the ones
        // for the channels being added, and nothing else.
        for channel in out.rows.len()..presentation.channels {
            let Some(row) = presentation.rows.get(channel) else {
                break;
            };
            if adds_a_dimension(&mut echelon, row) {
                out.rows.push(row.clone());
                out.within.push(presentation.channels);
                out.filled.push(false);
                continue;
            }
            // It is already reachable, so it cannot be a basis vector and the
            // channel takes whatever else this presentation still has to add.
            if let Some(other) = presentation
                .rows
                .iter()
                .find(|row| adds_a_dimension(&mut echelon, row))
                .cloned()
            {
                out.rows.push(other);
                out.within.push(presentation.channels);
                out.filled.push(false);
            }
        }

        // A presentation that needs fewer channels than it carries: the rest
        // are filled with elements, which is what the last one wanted there.
        fill(&mut out, &mut echelon, presentation.channels, prefer);
    }

    // And whatever no presentation named, so the whole is invertible.
    fill(&mut out, &mut echelon, elements, prefer);
    (out.rows.len() == elements).then_some(out)
}

/// Fill up to `upto` channels with elements that add a dimension, taken in
/// the order given.
fn fill(out: &mut Hierarchy, echelon: &mut Vec<Vec<f64>>, upto: usize, prefer: &[usize]) {
    let elements = prefer.len();
    for element in prefer.iter().copied() {
        if out.rows.len() >= upto {
            return;
        }
        let mut row = vec![0.0; elements];
        row[element] = 1.0;
        if adds_a_dimension(echelon, &row) {
            out.rows.push(row);
            out.within.push(upto);
            out.filled.push(true);
        }
    }
}

/// Does `row` reach anywhere the rows already chosen do not?
///
/// Reduces it against the echelon and keeps the residual there if so, which is
/// Gaussian elimination done one row at a time.
fn adds_a_dimension(echelon: &mut Vec<Vec<f64>>, row: &[f64]) -> bool {
    let size = row.iter().fold(0.0f64, |m, value| m.max(value.abs()));
    if size <= NOTHING_LEFT {
        return false;
    }
    let mut residual = row.to_vec();
    for chosen in echelon.iter() {
        let Some(pivot) = chosen.iter().position(|value| value.abs() > NOTHING_LEFT) else {
            continue;
        };
        let factor = residual[pivot] / chosen[pivot];
        if factor == 0.0 {
            continue;
        }
        for (value, take) in residual.iter_mut().zip(chosen) {
            *value -= factor * take;
        }
    }
    let left = residual.iter().fold(0.0f64, |m, value| m.max(value.abs()));
    if left <= NOTHING_LEFT * size {
        return false;
    }
    echelon.push(residual);
    true
}

/// How much of a presentation's own rows falls outside the channels it carries.
///
/// The property the whole hierarchy exists for, measured rather than declared:
/// expand each row in the stored channels and see how much of it lands past the
/// prefix that presentation reads. Zero means the presentation can produce that
/// row; anything else is the part it cannot.
///
/// Relative to the row's own size, so it does not depend on how loud the
/// programme is, and worst over every row of every presentation. `f64::INFINITY`
/// if the stored channels are not a basis at all.
///
/// 🔴 It is not zero for a cascade that has been through the arithmetic. The
/// coefficients are fourteen-bit, so a channel carries its row to about `2⁻¹⁴`
/// and no closer, and a fold is not required to be exact — only the elements
/// are, and the round trip is what guarantees those. Checking this against zero
/// tests the quantiser, not the hierarchy.
pub fn leak(hierarchy: &Hierarchy, presentations: &[Presentation]) -> f64 {
    let mut worst = 0.0f64;
    for presentation in presentations {
        for row in &presentation.rows {
            let size = row.iter().fold(0.0f64, |m, value| m.max(value.abs()));
            if size <= NOTHING_LEFT {
                continue;
            }
            let Some(g) = expand(&hierarchy.rows, row) else {
                return f64::INFINITY;
            };
            // Anything past the channels it carries is out of its reach.
            let outside = g
                .iter()
                .skip(presentation.channels)
                .fold(0.0f64, |m, coordinate| m.max(coordinate.abs()));
            worst = worst.max(outside / size);
        }
    }
    worst
}

/// Which presentation and row a hierarchy cannot carry at all.
///
/// [`leak`] with a threshold on it, for the cases where the answer is meant to
/// be yes or no rather than a number: a hierarchy built from exact rows, or one
/// built the wrong way round. `None` when every presentation reaches every row
/// of its own.
pub fn unreachable(
    hierarchy: &Hierarchy,
    presentations: &[Presentation],
) -> Option<(usize, usize)> {
    for (which, presentation) in presentations.iter().enumerate() {
        for (channel, row) in presentation.rows.iter().enumerate() {
            let size = row.iter().fold(0.0f64, |m, value| m.max(value.abs()));
            if size <= NOTHING_LEFT {
                continue;
            }
            let one = Hierarchy {
                rows: hierarchy.rows.clone(),
                within: hierarchy.within.clone(),
                filled: hierarchy.filled.clone(),
            };
            let just_this = Presentation {
                channels: presentation.channels,
                rows: vec![row.clone()],
            };
            if leak(&one, std::slice::from_ref(&just_this)) > 1e-6 {
                return Some((which, channel));
            }
        }
    }
    None
}

/// What a presentation declares, over the channels the stream actually stores.
///
/// A presentation's rows are written over the *elements*, and a decoder never
/// sees those — it sees whatever the arrangement left in the channels. So each
/// row is expanded in that, and what comes out is what the substream declares.
///
/// # 🔴 The rows are applied one after another, not all at once
///
/// A substream's matrices are a list and a decoder runs it in order, each one
/// *assigning* its destination. So the row for channel 1 reads a channel 0 that
/// the row before it has already overwritten, and an `n × n` mix written as `n`
/// rows is only right if no row needs a channel an earlier row has taken.
///
/// Which, for a hierarchy, it does not: the channels a presentation inherits
/// need correcting by the ones it *adds* — a 5.1's left is its stereo left less
/// the centre and surround that were folded into it, so channel 0 reads
/// channels 2 and up — and the channels it adds already hold their own rows and
/// need only a scale. The matrix is upper triangular, and running it top to
/// bottom is exactly right.
///
/// It is checked rather than assumed. A row reaching a channel below its own is
/// refused, because writing it would be writing something that quietly decodes
/// to the wrong mix.
///
/// # A coefficient the field cannot hold, said in two rows
///
/// The field stops a little under two and a real 7.1 asks for 2.203. Since each
/// row *assigns*, a row can be said at a fraction of its size and then scaled
/// back up by a row that reads only itself: `ch = Σ (c/1.9)·ch`, then
/// `ch = 1.9·ch`. Two matrices instead of one, and the same mix.
///
/// `None` if a row reaches past the prefix its presentation carries, if it
/// reaches a channel already written, or if it would take more than
/// [`AT_MOST_ROWS`] to say.
pub fn rows_over(
    held: &[Vec<f64>],
    presentation: &Presentation,
    shifts: &[u8],
    frac_bits: u32,
) -> Option<(Vec<Primitive>, Vec<u8>)> {
    let n = presentation.channels.min(held.len());
    if presentation.rows.len() < n {
        return None;
    }

    // What the presentation wants, over the channels it reads.
    let mut want = vec![vec![0.0f64; n]; n];
    for (channel, row) in presentation.rows.iter().enumerate().take(n) {
        let size = row.iter().fold(1e-12f64, |m, value| m.max(value.abs()));
        let g = expand(held, row)?;
        // Nothing past the prefix: a decoder stopping here never filled those.
        if g.iter()
            .skip(n)
            .any(|coordinate| coordinate.abs() > SETTLED * size)
        {
            if std::env::var_os("HZ_FOLD").is_some() {
                eprintln!(
                    "fold: the {n}-channel presentation's output {channel} reaches past its prefix: {:?}",
                    g.iter()
                        .skip(n)
                        .map(|c| (c / size * 1e3).round() / 1e3)
                        .collect::<Vec<_>>()
                );
            }
            return None;
        }
        want[channel].copy_from_slice(&g[..n]);
    }

    // 🔴 One matrix per channel, in an order that makes it possible.
    //
    // Each row **assigns** its destination, so a row written after another
    // reads that one's *result* rather than what was there before — which is
    // not a hazard to be worked around but the mechanism. The reference's 5.1
    // is six matrices for six channels, destinations 5, 1, 0, 3, 2, 4, each
    // reading whatever the ones before it left. Factoring into two triangles
    // instead costs twice the matrices and a permutation on top, which is 23
    // rows against the 15 a substream may declare.
    //
    // The order is not free: writing a channel replaces what it held, so the
    // channels have to go on spanning everything the rows still to come are
    // made of. A channel may therefore only take a row it holds some of, and
    // which pair to take next is chosen by what the row would cost to say —
    // its widest coefficient against the destination's own dead bits.
    // In coordinates over the channels, not over the elements: `want[o][k]` is
    // how much of channel `k` output `o` is made of, and the state says the
    // same of what the channels hold as the rows replace them.
    let mut state: Vec<Vec<f64>> = (0..n)
        .map(|channel| {
            let mut row = vec![0.0; n];
            row[channel] = 1.0;
            row
        })
        .collect();
    let mut rows = Vec::new();
    let mut assignment = vec![0u8; n];
    let mut taken = vec![false; n];
    let mut left: Vec<usize> = (0..n).collect();

    while !left.is_empty() {
        // 🔴 The channel of the same number first. The cost of a row says
        // nothing about what it leaves for the rows after it, and a greedy
        // reading of it takes a cheap cross pairing now for an impossible one
        // later — measured as the 5.1's left computed in channel 1 because that
        // channel's dead bits made it look cheap, leaving its right to be
        // computed in channel 0 at a coefficient of ten. The hierarchy stores
        // each presentation's channel `j` in channel `j` precisely so that the
        // diagonal is there to be taken.
        let mut best: Option<(f64, usize, usize, Vec<f64>)> = None;
        let mut straight: Option<(f64, usize, usize, Vec<f64>)> = None;
        for output in left.iter().copied() {
            let g = expand(&state, &want[output])?;
            let widest_of = g.iter().fold(0.0f64, |m, c| m.max(c.abs()));
            for channel in 0..n {
                // 🔴 Relative, and not a machine epsilon either. The cascade's
                // coefficients are fourteen-bit, so a channel a row has nothing
                // of still shows about `1e-4` of it — and a pivot on that
                // leaves the channels all but singular, which came out as the
                // next row asking for a hundred and fifty thousand. A channel
                // has to hold a real part of a row to compute it.
                if taken[channel] || g[channel].abs() <= WORTH_COMPUTING * widest_of {
                    continue;
                }
                let back = f64::from(shifts.get(channel).copied().unwrap_or(0));
                let widest = widest_of / back.exp2();
                if channel == output {
                    straight = Some((widest, output, channel, g.clone()));
                }
                if best.as_ref().is_none_or(|(cost, ..)| widest < *cost) {
                    best = Some((widest, output, channel, g.clone()));
                }
            }
        }
        let Some((_, output, channel, g)) = straight.or(best) else {
            if std::env::var_os("HZ_FOLD").is_some() {
                eprintln!(
                    "fold: no channel holds a real part of an output the {n}-channel presentation still needs: {left:?}"
                );
            }
            return None;
        };

        // What the channel has to end up holding: the row, made smaller by the
        // dead bits a decoder will shift it back up by.
        let back = f64::from(shifts.get(channel).copied().unwrap_or(0));
        let smaller = (-back).exp2();
        let row: Vec<f64> = g.iter().map(|coefficient| coefficient * smaller).collect();
        if say(&mut rows, channel, &row, frac_bits).is_none() {
            if std::env::var_os("HZ_FOLD").is_some() {
                eprintln!(
                    "fold: output {output} of the {n}-channel presentation asks channel {channel} for {:?}",
                    row.iter()
                        .map(|c| (c * 1e3).round() / 1e3)
                        .collect::<Vec<_>>()
                );
            }
            return None;
        }
        state[channel] = want[output]
            .iter()
            .map(|coordinate| coordinate * smaller)
            .collect();
        assignment[channel] = output as u8;
        taken[channel] = true;
        left.retain(|which| *which != output);
    }

    if rows.len() > AT_MOST_ROWS {
        return None;
    }
    Some((rows, assignment))
}

/// One row of a triangle, said in as many matrices as the field needs.
///
/// A row that is already what the channel holds is not said at all. One that
/// asks for more than the field can hold is said small and scaled back up by a
/// matrix reading only itself, which is a thing an assigning row can do.
fn say(rows: &mut Vec<Primitive>, dest: usize, row: &[f64], frac_bits: u32) -> Option<()> {
    // A row within the field's own step of doing nothing is doing nothing. Not
    // an exact test: the coefficients come out of a factorisation of quantised
    // numbers, so a row that should be the identity misses it by about `2⁻¹⁴`,
    // and writing it costs a matrix out of the fifteen a substream has.
    let idle = row.iter().enumerate().all(|(channel, coefficient)| {
        let want = f64::from(u8::from(channel == dest));
        (coefficient - want).abs() < IDLE
    });
    if idle {
        return Some(());
    }

    let mut spread = row.iter().fold(0.0f64, |m, c| m.max(c.abs()));
    let mut scalings = 0usize;
    while spread > REACH {
        spread /= REACH;
        scalings += 1;
        if scalings > AT_MOST_SCALINGS {
            return None;
        }
    }
    let shrunk = REACH.powi(scalings as i32);
    let said: Vec<f64> = row.iter().map(|c| c / shrunk).collect();
    rows.push(Primitive::rounded(dest, &said, frac_bits)?);
    for _ in 0..scalings {
        let mut back = vec![0.0; row.len()];
        back[dest] = REACH;
        rows.push(Primitive::rounded(dest, &back, frac_bits)?);
    }
    Some(())
}

/// The most a presentation's row may be scaled back up after being said small.
///
/// Two of them covers a coefficient of `1.9³`, near seven, which no fold has
/// ever asked for — the largest measured is 2.203.
const AT_MOST_SCALINGS: usize = 2;

/// How much of a row a channel has to hold before it can compute it.
///
/// A hundredth. Anything less is the quantisation the cascade left behind
/// rather than a share of the row, and taking it as a pivot leaves the channels
/// nearly singular.
const WORTH_COMPUTING: f64 = 0.01;

/// How near the identity a row has to be before it is not written at all.
///
/// Half the field's own step, so a row this skips is a row that would have been
/// written as the identity anyway.
const IDLE: f64 = 1.0 / 32768.0;

/// How much of a coefficient one row can say. The field stops just under two.
const REACH: f64 = 1.9;

/// The most rows one substream may declare.
///
/// The count is a four-bit field, but a decoder stops reading at eight under
/// restart sync words A and B — see [`crate::matrix::MAX_MATRICES`] — and the
/// rows share that count with the coding matrices. A 7.1 takes the eight; a
/// presentation that would need a scaling row on top of them is not written,
/// and the interval goes without folds, which a decoder plays, where a ninth
/// row is one it refuses.
const AT_MOST_ROWS: usize = crate::matrix::MAX_MATRICES;

/// How far outside its own channels a presentation's row may still land.
///
/// Relative to the row's own size, and not zero: the coefficients are
/// fourteen-bit, a step scaled by `2^k` is on a grid `2^k` coarser, and a
/// cascade accumulates a few of them. A stereo row stored at eight times its
/// size leaves two parts in a thousand of the 5.1's right on a channel the 5.1
/// cannot read — fifty-four decibels down, which a fold that is itself good
/// to a twentieth does not notice, and which refused the whole 5.1 while this
/// was a thousandth.
const SETTLED: f64 = 4e-3;

/// The fractional bits a presentation's rows are written with.
///
/// The most the field has. A fold is a set of real gains and the rows are only
/// ever read, never inverted, so there is nothing to gain by writing them
/// coarsely — the bits cost the same either way and a restart header states
/// them once.
pub const PRESENTATION_BITS: u32 = FRACTION;

/// What a decoder is left holding after running `rows`, for testing what they
/// say against what was wanted.
///
/// Each row **assigns** its destination from a combination of every channel as
/// it stands, so they are run in order and each sees what the one before it
/// left. Then `assignment` says which output channel each of them is.
pub fn as_decoded(
    held: &[Vec<f64>],
    rows: &[Primitive],
    assignment: &[u8],
    n: usize,
) -> Vec<Vec<f64>> {
    let elements = held.first().map_or(0, Vec::len);
    let mut channels: Vec<Vec<f64>> = held.iter().take(n).cloned().collect();
    for row in rows {
        let mut now = vec![0.0; elements];
        for (channel, held) in channels.iter().enumerate() {
            let coefficient =
                f64::from(row.coefficients[channel]) / f64::from(1i32 << row.frac_bits);
            if coefficient == 0.0 {
                continue;
            }
            for (slot, value) in now.iter_mut().zip(held) {
                *slot += coefficient * value;
            }
        }
        channels[row.dest] = now;
    }
    let mut out = vec![vec![0.0; elements]; n];
    for (channel, holding) in channels.into_iter().enumerate() {
        let output = assignment.get(channel).map_or(channel, |o| usize::from(*o));
        if output < n {
            out[output] = holding;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stereo, a 5.1 and the elements, over six elements.
    fn nested() -> Vec<Presentation> {
        // Six elements: a 5.1's worth, with the stereo folded out of them.
        let five = |channel: usize| {
            let mut row = vec![0.0; 6];
            row[channel] = 1.0;
            row
        };
        // Left only = L + 0.707·C + 0.707·Ls, which is what a fold states.
        let stereo = vec![
            vec![1.0, 0.0, 0.707, 0.0, 0.707, 0.0],
            vec![0.0, 1.0, 0.707, 0.0, 0.0, 0.707],
        ];
        vec![
            Presentation {
                channels: 2,
                rows: stereo,
            },
            Presentation {
                channels: 6,
                rows: (0..6).map(five).collect(),
            },
        ]
    }

    /// Every presentation reaches every row it wants, in the channels it has.
    ///
    /// The whole point of the hierarchy, stated as the decoder would find out.
    #[test]
    fn each_presentation_reaches_its_own_rows() {
        let presentations = nested();
        let hierarchy = build(&presentations, 6).expect("a hierarchy");
        assert_eq!(hierarchy.rows.len(), 6, "one row per element");
        assert_eq!(hierarchy.within, vec![2, 2, 6, 6, 6, 6]);
        assert_eq!(unreachable(&hierarchy, &presentations), None);

        // The leading channels are the stereo fold itself, stored as it
        // stands — not a rotation of it, and not the elements.
        assert_eq!(hierarchy.rows[0], presentations[0].rows[0]);
        assert_eq!(hierarchy.rows[1], presentations[0].rows[1]);
    }

    /// The stereo fold cannot be reached from two elements, which is the whole
    /// reason this exists.
    #[test]
    fn storing_the_elements_instead_does_not_reach_it() {
        let presentations = nested();
        // What arranging elements would give: each channel an element.
        let elements = Hierarchy {
            rows: (0..6)
                .map(|element| {
                    let mut row = vec![0.0; 6];
                    row[element] = 1.0;
                    row
                })
                .collect(),
            within: vec![2, 2, 6, 6, 6, 6],
            filled: vec![false; 6],
        };
        let (presentation, channel) =
            unreachable(&elements, &presentations).expect("the stereo cannot be reached");
        assert_eq!(presentation, 0, "the stereo is the one that fails");
        assert_eq!(channel, 0, "on its very first row");
    }

    /// A channel that takes nothing from the elements carries an element.
    ///
    /// The low frequency channel of a 5.1 is fed by a bed, not by objects, so
    /// on a programme without one its row is zero. It adds no dimension and
    /// cannot be stored, and the channel is not wasted: it takes what the last
    /// presentation needed there anyway.
    #[test]
    fn a_channel_that_takes_nothing_carries_an_element() {
        // Two elements make the stereo, and the three-channel presentation
        // adds nothing to them: its centre is silent and its left and right
        // are what the stereo already had. So it spans two dimensions across
        // three channels, and the third is free.
        let stereo = vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]];
        let three = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
        ];
        let presentations = vec![
            Presentation {
                channels: 2,
                rows: stereo,
            },
            Presentation {
                channels: 3,
                rows: three,
            },
        ];
        let hierarchy = build(&presentations, 3).expect("a hierarchy");
        assert_eq!(hierarchy.rows.len(), 3);

        // The third channel carries the element nobody asked for, which is
        // what the last presentation needed there.
        assert_eq!(hierarchy.rows[2], vec![0.0, 0.0, 1.0]);
        assert_eq!(hierarchy.within, vec![2, 2, 3]);
        assert_eq!(unreachable(&hierarchy, &presentations), None);
    }

    /// Presentations have to come narrowest first.
    #[test]
    fn presentations_out_of_order_are_refused() {
        let wide = Presentation {
            channels: 4,
            rows: (0..4)
                .map(|channel| {
                    let mut row = vec![0.0; 4];
                    row[channel] = 1.0;
                    row
                })
                .collect(),
        };
        let narrow = Presentation {
            channels: 2,
            rows: vec![vec![1.0, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0]],
        };
        assert!(build(&[wide, narrow], 4).is_none());
    }
}

#[cfg(test)]
mod declared {
    use super::*;

    /// What a substream declares decodes to the presentation it names.
    ///
    /// The whole of `rows_over`, checked the way a decoder finds out: run the
    /// rows in order, each assigning its destination from the channels as they
    /// stand, shift each back up by its own amount, then read them out through
    /// the channel assignment.
    ///
    /// 🔴 With the dead bits **different per channel**, which is the case that
    /// matters and the one the reference is in — its stereo substream states
    /// `[3, 4]`. A channel shifted down by three and one shifted down by four
    /// are not the same units, so a row that mixes them carries the ratio: the
    /// sources' shifts go into what is asked for, the destination's into the
    /// row that finally writes it.
    #[test]
    fn the_rows_decode_to_the_presentation() {
        // A hierarchy the way one really comes out: the leading channels carry
        // the stereo fold, the next four what the 5.1 adds — and *not* in the
        // 5.1's own channel order, which is what makes the matrix permute.
        let stereo_left = vec![1.0, 0.2, 0.7, 0.1, 0.3, 0.0];
        let stereo_right = vec![0.2, 1.0, 0.7, 0.1, 0.0, 0.3];
        let five: Vec<Vec<f64>> = (0..6)
            .map(|channel| {
                let mut row = vec![0.0; 6];
                row[channel] = 1.0;
                row
            })
            .collect();
        let presentations = [
            Presentation {
                channels: 2,
                rows: vec![stereo_left, stereo_right],
            },
            Presentation {
                channels: 6,
                rows: five,
            },
        ];
        // The reference's own spread: every channel within one of the others,
        // which is what its 5.1 substream states — `[3, 2, 4, 3, 3, 3]`. A
        // channel much further off than that asks a row for a ratio the field
        // cannot say, and the reference does not have that problem because it
        // restates the shifts in every substream. This one states them once.
        let shifts = [3u8, 4, 4, 3, 4, 3];

        // What the encoder hands the hierarchy: each element's own shift put
        // into what is asked for, since a channel shifted down by four is in
        // units sixteen times smaller.
        let scaled: Vec<Presentation> = presentations
            .iter()
            .map(|presentation| Presentation {
                channels: presentation.channels,
                rows: presentation
                    .rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .enumerate()
                            .map(|(element, gain)| gain * f64::from(shifts[element]).exp2())
                            .collect()
                    })
                    .collect(),
            })
            .collect();

        let built = build(&scaled, 6).expect("a hierarchy");
        let cascade = crate::arrange::arrange(&built.rows, &built.within, 6).expect("a cascade");

        // And what the channels really hold, which is those combinations of the
        // *shifted* elements.
        let stored: Vec<Vec<f64>> = cascade
            .held
            .iter()
            .map(|holding| {
                holding
                    .iter()
                    .enumerate()
                    .map(|(element, weight)| weight / f64::from(shifts[element]).exp2())
                    .collect()
            })
            .collect();

        for (which, presentation) in scaled.iter().enumerate() {
            let (rows, assignment) =
                rows_over(&cascade.held, presentation, &shifts, PRESENTATION_BITS).expect("rows");
            let mut decoded = as_decoded(&stored, &rows, &assignment, presentation.channels);

            // A decoder shifts each channel back up by its own amount, after
            // the matrices and before the assignment reads them out. `as_decoded`
            // has already read them out, so this undoes that ordering.
            let mut out = vec![vec![0.0; 6]; presentation.channels];
            for (channel, holding) in decoded.drain(..).enumerate() {
                out[channel] = holding;
            }
            for (channel, order) in assignment.iter().enumerate() {
                let by = f64::from(shifts[channel]).exp2();
                for weight in out[usize::from(*order)].iter_mut() {
                    *weight *= by;
                }
            }

            // Against what was wanted before any of the shifts went in.
            for (channel, want) in presentations[which].rows.iter().enumerate() {
                let size = want.iter().fold(1e-12f64, |m, v| m.max(v.abs()));
                let worst = out[channel]
                    .iter()
                    .zip(want)
                    .fold(0.0f64, |m, (got, want)| m.max((got - want).abs()));
                assert!(
                    worst < 1e-2 * size,
                    "{} channels, channel {channel}: off by {:.3e} of {size:.3}",
                    presentation.channels,
                    worst
                );
            }
        }
    }
}
