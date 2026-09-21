// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/mlpdsp.c — Copyright (c) 2007-2008 Ian Caulfield, 2009 Ramiro Polla
//   libavcodec/mlpdec.c — Copyright (c) 2007-2008 Ian Caulfield
// What was taken and what was changed is recorded in docs/provenance.md.

//! Lossless rematrixing: coding a channel as its difference from another.
//!
//! Two channels of a stereo mix are usually far more alike than either is like
//! silence, and prediction within a channel cannot see that. The format's
//! answer is a *primitive matrix*: one channel is replaced by itself minus a
//! weighted copy of another, and the decoder puts it back.
//!
//! # Why it is exactly invertible
//!
//! The decoder computes, for one destination channel:
//!
//! ```text
//! accumulator = Σ coefficient[k] · sample[k]        (64-bit, over every channel)
//! sample[dest] = accumulator >> 14
//! ```
//!
//! and the destination's own coefficient is 2¹⁴ — one, in the fourteen
//! fractional bits the format counts in. Since `2¹⁴·a` has no bits below the
//! shift, `(2¹⁴·a + b) >> 14` is exactly `a + (b >> 14)`, so the operation is
//! `sample[dest] += (Σ other) >> 14` and its inverse is the same expression
//! subtracted. No rounding survives the round trip, which is the whole
//! requirement.
//!
//! # What a substream's matrices may read, and why a fold is not free
//!
//! A decoder stopping after substream `s` has decoded that substream's
//! channels **and no others**, so the matrices it applies can only read those.
//! Substream 0 of an object programme carries channels 0 and 1; a presentation
//! stated there is therefore a 2×2 mix of those two, not a fold of all twelve
//! elements, and a row naming a source the presentation does not carry reads a
//! channel the decoder never filled.
//!
//! So writing real presentations is not a matter of declaring the fold. The
//! elements have to be *arranged* so the leading channels already carry what
//! each fold needs, with the last substream's rows rebuilding the elements
//! from all of them — which is what a shipped stream's permuted internal
//! channel order is for. [`Primitive::dense`] is the row such a presentation
//! is written with; putting the right thing in the channels is the rest of the
//! work, and `docs/mlp.md` says what it involves.
//!
//! # Where it sits
//!
//! The decoder reads residuals, runs the prediction filters, and *then*
//! applies the matrices. So an encoder matrixes first and filters the result —
//! which means the filter state a decoder carries is the state of the
//! *matrixed* channel, and this encoder's history buffers hold matrixed
//! samples for the same reason.

/// Fractional bits the format counts matrix coefficients in.
pub const FRACTION: u32 = 14;
/// A coefficient of one.
pub const UNITY: i32 = 1 << FRACTION;

/// Sources a matrix can read: the matrix channels, plus the two noise
/// channels a decoder synthesises under restart sync word A.
pub const MAX_SOURCES: usize = crate::format::MAX_CHANNELS + 2;

/// The most matrices a substream under restart sync word A or B may carry.
///
/// The count is a four-bit field and those two syntaxes write it as itself,
/// so they can *say* fifteen. What a decoder *reads* is eight: FFmpeg's
/// `MAX_MATRICES_TRUEHD`, which is the decoder inside every player that is
/// not Dolby's own, refuses a substream past it — and the reference streams
/// write two, six and eight in their first three substreams, the rows of
/// each presentation and nothing else. Written past eight, a stream was
/// refused unit by unit, and the errors cascaded into filter orders,
/// quantiser steps and sync words that were never wrong; the reference
/// decoder reads it fine, which is how it went unseen.
pub const MAX_MATRICES: usize = 8;
/// And under restart sync word C, which writes the count as one less than
/// itself — so it cannot say "no matrices" that way, and can say sixteen, one
/// per channel of the widest programme there is. FFmpeg never reads that
/// substream, and the reference writes nine there.
pub const MAX_MATRICES_IMMERSIVE: usize = 16;

/// Where the decoder's matrix accumulator has its point.
///
/// Coefficients are shifted up to meet it and the sum is shifted back down by
/// it, which comes to the same thing as [`FRACTION`] when nothing is moving —
/// and does not when something is, because the drift is folded down by this
/// same amount on its own first.
const ACCUMULATOR: u32 = 18;
const SCALE: u32 = ACCUMULATOR - FRACTION;

/// One primitive matrix, as the stream carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Primitive {
    /// The channel this matrix rewrites.
    pub dest: usize,
    /// Coefficients in [`FRACTION`] fractional bits, indexed by source
    /// channel. The destination's own is [`UNITY`].
    pub coefficients: [i32; MAX_SOURCES],
    /// How many fractional bits the coefficients are *written* with. Fewer
    /// bits, smaller field; the coefficients must be multiples of
    /// `2^(FRACTION - frac_bits)`.
    pub frac_bits: u32,
    /// How far each coefficient moves per access unit, in the same fractional
    /// bits, or all zero for a matrix that stands still.
    ///
    /// Only restart sync word C can say this, and a decoder both **ramps**
    /// within an access unit and **accumulates** at the end of one: the
    /// coefficient at sample `n` of unit `k` is `c + k·d + d·n/frames`, and
    /// the next unit starts where this one ended. So one statement tracks a
    /// trend for as long as the matrix stands, and the encoder has to follow
    /// the same ramp sample for sample or the stream decodes to something
    /// else with every checksum intact.
    pub delta: [i32; MAX_SOURCES],
    /// A power of two the whole matrix is scaled by on the wire, which only
    /// restart sync word C can say. The coefficients here are in [`FRACTION`]
    /// fractional bits whatever it is: the shift changes how they are
    /// *written* — divided by `2^shift`, into a field that then reaches
    /// `2^(shift+1)` rather than two — and not what a decoder computes, which
    /// is `(Σ coefficient · sample) >> 14` either way. See [`Primitive::wide`].
    pub shift: u32,
}

impl Primitive {
    /// A matrix that replaces `dest` with `dest - weight · source`.
    ///
    /// `weight` is in [`FRACTION`] fractional bits. Returns `None` if it
    /// cannot be written: the field holds `frac_bits + 2` signed bits, which
    /// caps a weight just under two.
    pub fn predict(dest: usize, source: usize, weight: i32, frac_bits: u32) -> Option<Self> {
        debug_assert!(dest != source);
        if frac_bits > FRACTION {
            return None;
        }
        // The decoder adds `(coefficient · source) >> 14` to the destination
        // and this subtracts the same quantity, so they carry the same sign.
        let stored = weight;
        let step = 1i32 << (FRACTION - frac_bits);
        if stored % step != 0 {
            return None;
        }
        let limit = 1i32 << (frac_bits + 1);
        if !(-limit..limit).contains(&(stored / step)) {
            return None;
        }
        // Unity has to survive the same field.
        if !(-limit..limit).contains(&(UNITY / step)) {
            return None;
        }

        let mut coefficients = [0i32; MAX_SOURCES];
        coefficients[dest] = UNITY;
        coefficients[source] = stored;
        Some(Self {
            dest,
            coefficients,
            frac_bits,
            delta: [0; MAX_SOURCES],
            shift: 0,
        })
    }

    /// A matrix that replaces `dest` with an arbitrary weighted sum, its own
    /// channel included.
    ///
    /// # This one is not invertible, and does not have to be
    ///
    /// Every other constructor here builds a *lifting step*: the destination
    /// keeps a coefficient of one, so the operation is `dest += (Σ other) >> 14`
    /// and subtracting the same quantity undoes it. That is what a lossless
    /// coder needs and it is what [`predict`](Self::predict) writes.
    ///
    /// A **presentation** is the other use of the same field. A decoder that
    /// stops after an early substream applies that substream's matrices and
    /// hands back what they compute — so a two-channel presentation is a 2×2
    /// mix of the internal channels, with no unit coefficient anywhere in it,
    /// and a five-channel one is five dense rows. Nothing has to invert it,
    /// because a full decode applies the *last* substream's matrices and none
    /// of these. See [`crate::frame`].
    ///
    /// `coefficients` are in [`FRACTION`] fractional bits, indexed by source
    /// channel, and the destination's own is one of them. `None` if any of
    /// them cannot be written: each is a signed field of `frac_bits + 2` bits,
    /// which caps a weight just under two, and each has to be a multiple of
    /// the step those bits leave.
    pub fn dense(dest: usize, coefficients: &[i32], frac_bits: u32) -> Option<Self> {
        if frac_bits > FRACTION || coefficients.len() > MAX_SOURCES {
            return None;
        }
        let step = 1i32 << (FRACTION - frac_bits);
        let limit = 1i32 << (frac_bits + 1);
        let mut stored = [0i32; MAX_SOURCES];
        for (source, coefficient) in coefficients.iter().enumerate() {
            if coefficient % step != 0 || !(-limit..limit).contains(&(coefficient / step)) {
                return None;
            }
            stored[source] = *coefficient;
        }
        Some(Self {
            dest,
            coefficients: stored,
            frac_bits,
            delta: [0; MAX_SOURCES],
            shift: 0,
        })
    }

    /// The nearest writable row to a set of real gains.
    ///
    /// A fold's coefficients are real numbers and the field holds a multiple
    /// of `2^-frac_bits`, so they are rounded to it. `None` if any of them is
    /// outside what the field can say at all — which for a fold means a gain
    /// of two or more, and a fold that asks for one is a fold that would clip.
    pub fn rounded(dest: usize, gains: &[f64], frac_bits: u32) -> Option<Self> {
        let step = 1i32 << (FRACTION - frac_bits);
        let mut coefficients = vec![0i32; gains.len()];
        for (slot, gain) in coefficients.iter_mut().zip(gains) {
            let scaled = (gain * f64::from(UNITY) / f64::from(step)).round();
            if !scaled.is_finite() {
                return None;
            }
            *slot = (scaled as i32).checked_mul(step)?;
        }
        Self::dense(dest, &coefficients, frac_bits)
    }

    /// Whether this matrix moves at all.
    pub fn moves(&self) -> bool {
        self.delta.iter().any(|delta| *delta != 0)
    }

    /// The coefficient as the stream stores it: on the grid its fractional
    /// bits set, and divided by the matrix's shift where it has one.
    pub fn stored(&self, source: usize) -> i32 {
        self.coefficients[source] >> (FRACTION - self.frac_bits + self.shift)
    }

    /// What the decoder will add to the destination, for one sample.
    ///
    /// `unit` counts access units since this matrix was stated and `sample`
    /// counts within the unit, which together place the coefficient on its
    /// ramp. Written in the decoder's own widths and shifts rather than in the
    /// obvious ones: the drift is folded down by eighteen bits *before* it is
    /// scaled by the sample index, and reproducing that loss is the difference
    /// between a stream that decodes and one that decodes to something else.
    #[inline]
    fn correction(&self, frame: &[i32], unit: usize, sample: usize, frames: usize) -> i64 {
        let mut accumulator = 0i64;
        let mut drift = 0i64;
        for (source, coefficient) in self.coefficients.iter().enumerate() {
            // A source with no coefficient is not in the stream at all — the
            // mask names the non-zero ones and the steps follow the mask — so
            // whatever `delta` says of it, the decoder applies nothing, and
            // neither does this.
            if source == self.dest || *coefficient == 0 {
                continue;
            }
            let delta = self.delta[source];
            let value = i64::from(frame.get(source).copied().unwrap_or(0));
            let carried = i64::from(*coefficient) + unit as i64 * i64::from(delta);
            accumulator += (carried << SCALE) * value;
            if delta != 0 {
                drift += (i64::from(delta) << SCALE) * value;
            }
        }
        if drift != 0 {
            // The decoder's own reciprocal, integer-divided, then shifted up by
            // two — not a division by the frame count.
            let reciprocal = (1i64 << 16) / frames.max(1) as i64;
            accumulator += (drift >> ACCUMULATOR) * sample as i64 * (reciprocal << 2);
        }
        accumulator >> ACCUMULATOR
    }

    /// The bits this matrix costs to describe in a given substream.
    ///
    /// A closed form standing in for writing it into a
    /// [`BitCounter`](hz_core::bits::BitCounter); the two are held to agree by
    /// `the_closed_form_agrees_with_the_counter` below.
    ///
    /// Which substream matters, because the three restart sync words describe
    /// a matrix three different ways. All three spend a bit per source saying
    /// which coefficients are there and then write only those; what differs is
    /// the fixed part and how many sources there are.
    pub fn cost_bits(&self, range: &crate::format::Substream) -> usize {
        use crate::format::RestartSync;

        let sources = range.matrix_sources().min(MAX_SOURCES);
        let present = self.coefficients[..sources]
            .iter()
            .filter(|c| **c != 0)
            .count();
        let coefficients = sources + present * (self.frac_bits as usize + 2);
        match range.sync {
            // Destination, fractional bits, the bypass flag.
            RestartSync::A => 4 + 4 + 1 + coefficients,
            // And a dither scale of its own.
            RestartSync::B => 4 + 4 + 1 + 4 + coefficients,
            // Destination, fractional bits, a shift on the whole matrix, the
            // width of its bypassed low bits, and a dither scale.
            RestartSync::C => 4 + 4 + 3 + 2 + 4 + coefficients,
        }
    }
}

/// Apply the inverse of a sequence of matrices to a block, in place.
///
/// The decoder applies them in order, each seeing what the ones before it
/// produced, so the encoder applies their inverses in the opposite order.
/// `planes[channel][frame]` is channel-major, as everything else here is.
pub fn apply_inverse<P: AsRef<[i32]> + AsMut<[i32]>>(
    matrices: &[Primitive],
    planes: &mut [P],
    frames: usize,
    unit: usize,
) {
    let mut frame_buffer = [0i32; MAX_SOURCES];
    for frame in 0..frames {
        for (slot, plane) in frame_buffer.iter_mut().zip(planes.iter()) {
            *slot = plane.as_ref()[frame];
        }
        for matrix in matrices.iter().rev() {
            let correction = matrix.correction(&frame_buffer, unit, frame, frames);
            frame_buffer[matrix.dest] -= correction as i32;
        }
        for (plane, value) in planes.iter_mut().zip(&frame_buffer) {
            plane.as_mut()[frame] = *value;
        }
    }
}

/// The weight that best predicts `target` from `source`, in [`FRACTION`]
/// fractional bits, rounded onto a `frac_bits` grid.
///
/// Least squares, which is the right answer for the quantity that matters:
/// the residual's energy, and so — to within the shape of its distribution —
/// how many bits it takes.
pub fn best_weight(target: &[i32], source: &[i32], frac_bits: u32) -> Option<i32> {
    let mut cross = 0i128;
    let mut energy = 0i128;
    for (a, b) in target.iter().zip(source) {
        cross += i128::from(*a) * i128::from(*b);
        energy += i128::from(*b) * i128::from(*b);
    }
    if energy == 0 {
        return None;
    }

    let scaled = (cross << FRACTION) / energy;
    let step = i128::from(1i32 << (FRACTION - frac_bits));
    // Round to the nearest representable weight rather than truncating: half a
    // step of bias on every sample is a bit that need not have been spent.
    let rounded = ((scaled + step / 2).div_euclid(step)) * step;

    let limit = i128::from(1i32 << (frac_bits + 1)) * step;
    if !(-limit..limit).contains(&rounded) || rounded == 0 {
        return None;
    }
    Some(rounded as i32)
}

/// The most sources one matrix is fitted against.
///
/// A primitive matrix can read every channel the substream has decoded, and
/// the format writes a presence bit per source either way — so a second and a
/// third source cost only their own coefficients. Past three the normal
/// equations get close to singular on real material and the weights start
/// chasing noise, which the cost check then refuses anyway at the price of
/// having computed it. Measured again once the channels carried folds, where
/// a channel is a sum of half the elements and the case for more looked
/// strongest: eight sources, tried in order and stopping at the first that
/// did not pay, wrote a stream 0.1 % *larger* than three did.
pub const MAX_FIT_SOURCES: usize = 3;

/// The weights that best predict `target` from several sources at once, in
/// [`FRACTION`] fractional bits, each rounded onto a `frac_bits` grid.
///
/// Least squares again, but jointly: fitting each source on its own and adding
/// the results is not the same answer when the sources are themselves
/// correlated, and in a bed they always are. The normal equations are at most
/// [`MAX_FIT_SOURCES`] square, so they are solved where they stand.
///
/// `None` if the system is singular — two sources that carry the same signal —
/// or if any weight does not fit the field. The caller falls back to fewer
/// sources.
pub fn best_weights(target: &[i32], sources: &[&[i32]], frac_bits: u32) -> Option<Vec<i32>> {
    let n = sources.len();
    if n == 0 || n > MAX_FIT_SOURCES {
        return None;
    }
    // Normal equations: (SᵀS) w = Sᵀt.
    let mut normal = [[0.0f64; MAX_FIT_SOURCES]; MAX_FIT_SOURCES];
    let mut cross = [0.0f64; MAX_FIT_SOURCES];
    for (row, source) in sources.iter().enumerate() {
        for (column, other) in sources.iter().enumerate().take(row + 1) {
            let sum: f64 = source
                .iter()
                .zip(*other)
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum();
            normal[row][column] = sum;
            normal[column][row] = sum;
        }
        cross[row] = source
            .iter()
            .zip(target)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum();
    }

    let weights = solve(&mut normal, &mut cross, n)?;
    let step = f64::from(1i32 << (FRACTION - frac_bits));
    let limit = i64::from(1i32 << (frac_bits + 1)) * step as i64;
    let mut out = Vec::with_capacity(n);
    for weight in &weights[..n] {
        if !weight.is_finite() {
            return None;
        }
        // Round to the nearest representable weight rather than truncating:
        // half a step of bias on every sample is a bit that need not have been
        // spent.
        let scaled = (weight * f64::from(1u32 << FRACTION) / step).round() * step;
        let rounded = scaled as i64;
        if !(-limit..limit).contains(&rounded) {
            return None;
        }
        out.push(rounded as i32);
    }
    // A matrix all of whose weights round to nothing is not a matrix.
    out.iter().any(|w| *w != 0).then_some(out)
}

/// Gaussian elimination with partial pivoting, for a system of at most
/// [`MAX_FIT_SOURCES`].
fn solve(
    matrix: &mut [[f64; MAX_FIT_SOURCES]; MAX_FIT_SOURCES],
    rhs: &mut [f64; MAX_FIT_SOURCES],
    n: usize,
) -> Option<[f64; MAX_FIT_SOURCES]> {
    // A scale to judge a pivot against, so "singular" means singular relative
    // to the energies involved rather than to an absolute epsilon.
    let scale = (0..n).fold(0.0f64, |m, row| m.max(matrix[row][row].abs()));
    if scale <= 0.0 {
        return None;
    }
    for step in 0..n {
        let pivot = (step..n).max_by(|a, b| {
            matrix[*a][step]
                .abs()
                .partial_cmp(&matrix[*b][step].abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
        if matrix[pivot][step].abs() <= scale * 1e-9 {
            return None;
        }
        matrix.swap(step, pivot);
        rhs.swap(step, pivot);
        let (above, below) = matrix.split_at_mut(step + 1);
        let pivot_row = &above[step];
        for (offset, row) in below[..n - step - 1].iter_mut().enumerate() {
            let factor = row[step] / pivot_row[step];
            for (cell, reference) in row[step..n].iter_mut().zip(&pivot_row[step..n]) {
                *cell -= factor * reference;
            }
            rhs[step + 1 + offset] -= factor * rhs[step];
        }
    }
    let mut out = [0.0f64; MAX_FIT_SOURCES];
    for row in (0..n).rev() {
        let mut value = rhs[row];
        for column in row + 1..n {
            value -= matrix[row][column] * out[column];
        }
        out[row] = value / matrix[row][row];
    }
    Some(out)
}

/// The most a matrix may be scaled by on the wire, as a power of two.
///
/// The field is three bits and a decoder reads it as one less than written, so
/// it runs from a halving to `2^6`. The halving is never useful here.
pub const MAX_SHIFT: u32 = 6;

impl Primitive {
    /// A lifting step whose coefficients may reach past two.
    ///
    /// The field a coefficient is written in holds a little under two, and a
    /// row that needs more used to be said as several steps clamped to that —
    /// five of them for a row hosted on a channel holding an eighth of it,
    /// which is five matrices out of the sixteen a substream has. Restart
    /// sync word C has a shift on the whole matrix for exactly this: the
    /// coefficients are written divided by `2^shift`, so the field reaches
    /// `2^(shift+1)`, on a grid `2^shift` coarser. This picks the smallest
    /// shift the coefficients fit, rounds them onto its grid, and keeps the
    /// destination at one — which the grid can say for as long as the shift
    /// does not exceed the fractional bits.
    ///
    /// `coefficients` are in [`FRACTION`] fractional bits, the destination's
    /// own ignored. `None` past what [`MAX_SHIFT`] reaches, a coefficient of a
    /// hundred and twenty-eight: a row hosted on a channel that holds almost
    /// none of it, and the caller's cue to host it elsewhere.
    pub fn wide(dest: usize, coefficients: &[i32], frac_bits: u32) -> Option<Self> {
        if frac_bits > FRACTION || coefficients.len() > MAX_SOURCES || dest >= MAX_SOURCES {
            return None;
        }
        let widest = coefficients
            .iter()
            .enumerate()
            .filter(|(source, _)| *source != dest)
            .map(|(_, coefficient)| coefficient.unsigned_abs())
            .max()
            .unwrap_or(0);
        let limit = 1u32 << (frac_bits + 1);
        let mut shift = 0u32;
        loop {
            let grid = 1u32 << (FRACTION - frac_bits + shift);
            // Rounded onto the grid, the widest still has to be inside the
            // field.
            if (widest + grid / 2) / grid < limit {
                break;
            }
            shift += 1;
            if shift > MAX_SHIFT.min(frac_bits) {
                return None;
            }
        }
        let grid = i64::from(1i32 << (FRACTION - frac_bits + shift));
        let mut stored = [0i32; MAX_SOURCES];
        for (source, coefficient) in coefficients.iter().enumerate() {
            if source == dest {
                continue;
            }
            // To the nearest grid point, as `rounded` does.
            let steps = (i64::from(*coefficient) + grid / 2).div_euclid(grid);
            stored[source] = (steps * grid) as i32;
        }
        stored[dest] = UNITY;
        Some(Self {
            dest,
            coefficients: stored,
            frac_bits,
            delta: [0; MAX_SOURCES],
            shift,
        })
    }

    /// A matrix that replaces `dest` with `dest` less a weighted sum of
    /// several sources.
    ///
    /// The single-source case is [`Primitive::predict`]; this is the same
    /// thing with more of them, and the wire format already carries it — a
    /// presence bit per source and a coefficient for each one present.
    pub fn fit(dest: usize, sources: &[usize], weights: &[i32], frac_bits: u32) -> Option<Self> {
        if frac_bits > FRACTION || sources.len() != weights.len() || sources.is_empty() {
            return None;
        }
        let step = 1i32 << (FRACTION - frac_bits);
        let limit = 1i32 << (frac_bits + 1);
        if !(-limit..limit).contains(&(UNITY / step)) {
            return None;
        }
        let mut coefficients = [0i32; MAX_SOURCES];
        coefficients[dest] = UNITY;
        for (source, weight) in sources.iter().zip(weights) {
            if *source == dest || *source >= MAX_SOURCES {
                return None;
            }
            if weight % step != 0 || !(-limit..limit).contains(&(weight / step)) {
                return None;
            }
            // Two sources that are the same channel would overwrite each other
            // and the second would be lost between the fit and the stream.
            if coefficients[*source] != 0 {
                return None;
            }
            coefficients[*source] = *weight;
        }
        Some(Self {
            dest,
            coefficients,
            frac_bits,
            delta: [0; MAX_SOURCES],
            shift: 0,
        })
    }
}

#[cfg(test)]
mod tests {

    /// A dense row says what it was given, to the field's step.
    ///
    /// Every other constructor here keeps the destination at one; this is the
    /// one that does not, because a presentation's rows have no unit
    /// coefficient anywhere in them.
    #[test]
    fn a_dense_row_carries_its_own_destination() {
        // The two-channel rows a shipped stream states, near enough.
        let row = Primitive::rounded(0, &[0.796, -0.713], FRACTION).expect("inside the field");
        assert_eq!(row.dest, 0);
        // Its own coefficient is not one, which is the whole point.
        assert_ne!(row.coefficients[0], UNITY);
        for (source, gain) in [(0usize, 0.796f64), (1, -0.713)] {
            let got = f64::from(row.coefficients[source]) / f64::from(UNITY);
            assert!(
                (got - gain).abs() < 1.0 / f64::from(UNITY),
                "{got} against {gain}"
            );
        }
    }

    /// The field caps a coefficient just under two, and a gain past it is
    /// refused rather than wrapped into something a decoder would apply.
    #[test]
    fn a_gain_the_field_cannot_say_is_refused() {
        assert!(Primitive::rounded(0, &[1.9, 0.1], FRACTION).is_some());
        assert!(Primitive::rounded(0, &[2.5, 0.1], FRACTION).is_none());
        // And fewer fractional bits mean a coarser grid, not a smaller range:
        // a gain off the grid is rounded on to it by `rounded` and refused by
        // `dense`, which takes what it is given.
        assert!(Primitive::rounded(0, &[0.5, 0.25], 6).is_some());
        assert!(Primitive::dense(0, &[UNITY / 3, 0], 6).is_none());
    }
    use super::*;

    /// The three restart sync words describe a matrix three different ways,
    /// and what they cost differs by more than a bit or two. Nothing else in
    /// the crate would notice if this drifted: a mis-priced matrix produces a
    /// stream that is valid and slightly too big.
    #[test]
    fn each_syntax_prices_a_matrix_its_own_way() {
        use crate::format::{RestartSync, Substream};

        let matrix = Primitive::predict(0, 1, UNITY / 2, FRACTION).expect("a writable matrix");
        let at = |sync, max_matrix| {
            matrix.cost_bits(&Substream {
                first: 0,
                last: max_matrix,
                max_matrix,
                sync,
            })
        };

        // Two coefficients present — the destination's own and the source's —
        // at sixteen bits each, over the sources each syntax declares.
        let sources_a = 1 + 3; // plus the two noise channels only A has
        let sources_b = 1 + 1;
        assert_eq!(at(RestartSync::A, 1), 4 + 4 + 1 + sources_a + 2 * 16);
        assert_eq!(at(RestartSync::B, 1), 4 + 4 + 1 + 4 + sources_b + 2 * 16);
        assert_eq!(
            at(RestartSync::C, 1),
            4 + 4 + 3 + 2 + 4 + sources_b + 2 * 16
        );

        // And the mask grows with the matrix span, which is what makes the
        // immersive syntax dearer in the substream that has sixteen of them.
        assert_eq!(
            at(RestartSync::C, 15) - at(RestartSync::C, 1),
            16 - 2,
            "one mask bit per source"
        );
    }

    /// The decoder's forward application, written from its own description so
    /// the inverse has something other than itself to be checked against.
    /// The decoder, written the way the decoder writes it — every source
    /// included, the destination's own coefficient among them, in its widths
    /// and its shifts. Not a rearrangement of the encoder's arithmetic, which
    /// would agree with itself about a mistake.
    fn apply_forward(matrices: &[Primitive], planes: &mut [Vec<i32>], frames: usize, unit: usize) {
        let mut frame_buffer = [0i32; MAX_SOURCES];
        for frame in 0..frames {
            for (slot, plane) in frame_buffer.iter_mut().zip(planes.iter()) {
                *slot = plane[frame];
            }
            for matrix in matrices {
                let mut accumulator = 0i64;
                let mut drift = 0i64;
                for (source, coefficient) in matrix.coefficients.iter().enumerate() {
                    let value = i64::from(frame_buffer[source]);
                    // The decoder reads a step only for a source the mask
                    // names, which is one with a coefficient.
                    let delta = if *coefficient == 0 {
                        0
                    } else {
                        i64::from(matrix.delta[source])
                    };
                    let carried = i64::from(*coefficient) + unit as i64 * delta;
                    accumulator += (carried << SCALE) * value;
                    drift += (delta << SCALE) * value;
                }
                if drift != 0 {
                    let reciprocal = (1i64 << 16) / frames as i64;
                    accumulator += (drift >> ACCUMULATOR) * frame as i64 * (reciprocal << 2);
                }
                frame_buffer[matrix.dest] = (accumulator >> ACCUMULATOR) as i32;
            }
            for (plane, value) in planes.iter_mut().zip(&frame_buffer) {
                plane[frame] = *value;
            }
        }
    }

    /// A matrix that moves has to invert too, at every point on its ramp and
    /// in every unit of the interval it stands for.
    ///
    /// The ramp is not a division by the frame count: the decoder folds the
    /// drift down by eighteen bits *before* scaling it by the sample index, and
    /// multiplies by an integer reciprocal of its own. Reproducing the obvious
    /// arithmetic instead of that one gives a stream whose every checksum
    /// verifies and whose samples are wrong.
    #[test]
    fn a_matrix_that_moves_still_inverts() {
        for step in [1i32, -1, 37, -128, 512] {
            let mut matrix = Primitive::predict(0, 1, UNITY / 3, FRACTION).unwrap();
            matrix.delta[1] = step;
            assert!(matrix.moves());

            // Every unit of a restart interval, since the coefficient has
            // accumulated a unit's worth of drift in each.
            for unit in [0usize, 1, 7, 15] {
                let original = pair(0x5bd1_e995 ^ unit as u32, 160);
                let mut planes = original.clone();
                apply_inverse(std::slice::from_ref(&matrix), &mut planes, 160, unit);
                apply_forward(std::slice::from_ref(&matrix), &mut planes, 160, unit);
                assert_eq!(planes, original, "step {step}, unit {unit}");
            }
        }
    }

    /// A step on a source that has no coefficient is not in the stream: the
    /// mask names the sources with one, and the steps follow the mask. So it
    /// must not be applied either, or the encoder subtracts a ramp the decoder
    /// never adds back — which is how two intervals of a programme decoded to
    /// the wrong samples with every checksum intact.
    #[test]
    fn a_step_on_an_absent_coefficient_is_not_applied() {
        let mut with = Primitive::predict(0, 1, UNITY / 3, FRACTION).unwrap();
        // Channel 2 has no coefficient and is given a step anyway.
        with.delta[2] = 40;
        let without = Primitive::predict(0, 1, UNITY / 3, FRACTION).unwrap();

        let mut planes = pair(0x1357_9bdf, 200);
        planes.push(planes[0].iter().map(|s| s / 3 + 7).collect());
        let mut a = planes.clone();
        let mut b = planes.clone();
        apply_inverse(std::slice::from_ref(&with), &mut a, 200, 3);
        apply_inverse(std::slice::from_ref(&without), &mut b, 200, 3);
        assert_eq!(a, b, "a step the stream cannot carry changed the coding");
        apply_forward(std::slice::from_ref(&with), &mut a, 200, 3);
        assert_eq!(a, planes);
    }

    /// And a matrix that does not move is coded exactly as it was before the
    /// ramp existed: the two arithmetics agree where they overlap, which is
    /// what says the wider one did not disturb anything.
    #[test]
    fn a_still_matrix_is_unchanged_by_the_wider_arithmetic() {
        let matrix = Primitive::predict(1, 0, UNITY / 5, FRACTION).unwrap();
        assert!(!matrix.moves());
        let original = pair(0x1000_0001, 240);
        let mut planes = original.clone();
        apply_inverse(std::slice::from_ref(&matrix), &mut planes, 240, 0);

        // The same fold, written the way it was when nothing could move.
        let mut expected = original.clone();
        let (left, right) = expected.split_at_mut(1);
        for (source, destination) in left[0].iter().zip(right[0].iter_mut()) {
            let correction = (i64::from(matrix.coefficients[0]) * i64::from(*source)) >> FRACTION;
            *destination -= correction as i32;
        }
        assert_eq!(planes, expected);
    }

    fn pair(seed: u32, frames: usize) -> Vec<Vec<i32>> {
        let mut state = seed;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 9) as i32 & 0x000f_ffff) - 0x0008_0000
        };
        let left: Vec<i32> = (0..frames).map(|_| next()).collect();
        // Correlated, but not identical.
        let right: Vec<i32> = left.iter().map(|l| l / 2 + next() / 8).collect();
        vec![left, right]
    }

    #[test]
    fn the_inverse_undoes_the_decoder_exactly() {
        for weight in [UNITY, -UNITY, UNITY / 2, -3 * UNITY / 4, 1, -1] {
            for (dest, source) in [(0usize, 1usize), (1, 0)] {
                let Some(matrix) = Primitive::predict(dest, source, weight, FRACTION) else {
                    continue;
                };
                let original = pair(0x1234_5678, 200);
                let mut planes = original.clone();
                apply_inverse(std::slice::from_ref(&matrix), &mut planes, 200, 0);
                assert_ne!(planes, original, "the matrix did nothing");
                apply_forward(std::slice::from_ref(&matrix), &mut planes, 200, 0);
                assert_eq!(planes, original, "weight {weight}, {dest} from {source}");
            }
        }
    }

    /// Several matrices in sequence, where the order of the inverses matters
    /// and getting it backwards would still round-trip for a single one.
    #[test]
    fn a_sequence_inverts_in_the_right_order() {
        let matrices = [
            Primitive::predict(0, 1, UNITY / 2, FRACTION).unwrap(),
            Primitive::predict(1, 0, UNITY / 4, FRACTION).unwrap(),
        ];
        let original = pair(0x9e37_79b9, 300);
        let mut planes = original.clone();
        apply_inverse(&matrices, &mut planes, 300, 0);
        apply_forward(&matrices, &mut planes, 300, 0);
        assert_eq!(planes, original);
    }

    #[test]
    fn a_weight_of_one_is_the_difference_of_the_channels() {
        let matrix = Primitive::predict(0, 1, UNITY, FRACTION).unwrap();
        let mut planes = vec![vec![1000, -2000], vec![300, 400]];
        apply_inverse(std::slice::from_ref(&matrix), &mut planes, 2, 0);
        assert_eq!(planes[0], vec![700, -2400]);
        assert_eq!(planes[1], vec![300, 400], "the source is untouched");
    }

    /// Decorrelation has to actually reduce the signal, or there is no point.
    #[test]
    fn predicting_one_channel_from_the_other_shrinks_it() {
        let planes = pair(0xdead_beef, 400);
        let weight = best_weight(&planes[0], &planes[1], FRACTION).expect("a weight");
        let matrix = Primitive::predict(0, 1, weight, FRACTION).expect("a matrix");

        let before: i64 = planes[0].iter().map(|s| i64::from(s.abs())).sum();
        let mut matrixed = planes.clone();
        apply_inverse(std::slice::from_ref(&matrix), &mut matrixed, 400, 0);
        let after: i64 = matrixed[0].iter().map(|s| i64::from(s.abs())).sum();

        assert!(
            after < before,
            "decorrelating grew the channel: {after} against {before}"
        );
    }

    /// What a matrix costs to describe and what describing it emits have to be
    /// the same number in all three syntaxes. A mis-priced matrix produces a
    /// stream that is valid and slightly too big, which nothing else notices.
    #[test]
    fn the_closed_form_agrees_with_the_counter() {
        use crate::format::{RestartSync, Substream};
        use hz_core::bits::BitCounter;

        for sync in [RestartSync::A, RestartSync::B, RestartSync::C] {
            for max_matrix in [1usize, 5, 7, 15] {
                if sync == RestartSync::A && max_matrix > 5 {
                    // A's two extra sources push it past what a decoder
                    // accepts, so the encoder never writes it there.
                    continue;
                }
                let range = Substream {
                    first: 0,
                    last: max_matrix,
                    max_matrix,
                    sync,
                };
                for frac_bits in [2u32, 7, FRACTION] {
                    let step = 1i32 << (FRACTION - frac_bits);
                    let matrices: Vec<Primitive> = (1..=max_matrix.min(4))
                        .filter_map(|dest| Primitive::predict(dest, dest - 1, step * 3, frac_bits))
                        .collect();
                    if matrices.is_empty() {
                        continue;
                    }
                    let declared: Vec<&Primitive> = matrices.iter().collect();

                    let mut counter = BitCounter::new();
                    crate::frame::write_matrix_params(&mut counter, &declared, &range);
                    let described: usize =
                        declared.iter().map(|matrix| matrix.cost_bits(&range)).sum();

                    // What is around the matrices rather than in them: the
                    // count, and for the immersive syntax the two bits that
                    // open it and the one that says nothing interpolates.
                    let framing = match sync {
                        RestartSync::A | RestartSync::B => 4,
                        RestartSync::C => 1 + 1 + 4 + 1,
                    };
                    assert_eq!(
                        described + framing,
                        counter.bits(),
                        "{sync:?}, {} matrices over {max_matrix} channels at {frac_bits} bits",
                        declared.len()
                    );
                }
            }
        }
    }

    /// A weight the field cannot hold has to be refused rather than truncated
    /// into a matrix the decoder would apply differently.
    #[test]
    fn a_weight_that_does_not_fit_is_refused() {
        assert!(Primitive::predict(0, 1, 4 * UNITY, FRACTION).is_none());
        // And one that is not on the grid its precision implies.
        assert!(Primitive::predict(0, 1, UNITY + 1, 2).is_none());
    }

    #[test]
    fn a_silent_source_predicts_nothing() {
        assert!(best_weight(&[1, 2, 3], &[0, 0, 0], FRACTION).is_none());
    }
}
