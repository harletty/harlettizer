//! Turning the objects' audio into the elements'.
//!
//! The clustering says how much of each object each element carries. This
//! applies it: element `k` gets `Σ_i w_ik · g_i · x_i`, where `x_i` is the
//! object's signal and `g_i` its own gain. That is the *only* place a clustered
//! stream's audio comes from, and it has one property the metadata cannot fix
//! afterwards.
//!
//! # The weights have to ramp
//!
//! A clustering is decided per block and the weights change from one block to
//! the next. Applying each block's weights to its own samples and stopping
//! there puts a step discontinuity in every element at every block boundary —
//! twenty-five a second — and a step in a signal is a click. So the weights are
//! interpolated linearly across the block, from what the previous block ended
//! at to what this one asks for, which is what the elements' *positions* do
//! too: a decoder ramps a position across an access unit rather than jumping
//! it.
//!
//! Linearly in amplitude and not in power, which loses a little energy in the
//! middle of a crossfade between two elements carrying the same object. That is
//! the trade the format makes for positions and it is the one made here: a
//! power law would hold the energy and put a kink in the amplitude, and a kink
//! is audible where a fraction of a decibel is not.
//!
//! # The gain goes on before the mix, not after
//!
//! An object's gain is the mix's own instruction about how loud it is, and two
//! objects sharing an element have different ones. Applying it after the mix
//! would apply one object's gain to another's signal.

/// Mix the objects into the elements for one block.
///
/// `signals[i]` is object `i`'s samples and `gains[i]` its own gain, both for
/// this block. Generic over what holds the samples so that a caller keeping
/// its own buffers can pass them directly rather than building a vector of
/// borrows every block. `from[i][k]` is the weight element `k` carried of object `i` at
/// the end of the previous block and `to[i][k]` what this block asks for; the
/// weight used at sample `n` of `frames` is interpolated between them.
///
/// `out[k]` is filled, not added to.
///
/// # Panics
/// If the shapes disagree: every signal is `frames` long, every weight row is
/// `out.len()` wide, and there are as many rows as signals.
pub fn mix<S: AsRef<[f32]>>(
    signals: &[S],
    gains: &[f64],
    from: &[Vec<f64>],
    to: &[Vec<f64>],
    frames: usize,
    out: &mut [Vec<f32>],
) {
    assert_eq!(signals.len(), gains.len(), "a gain for every object");
    assert_eq!(signals.len(), from.len(), "a weight row for every object");
    assert_eq!(signals.len(), to.len(), "a weight row for every object");

    for element in out.iter_mut() {
        element.clear();
        element.resize(frames, 0.0);
    }
    if frames == 0 {
        return;
    }
    // One over the block, so the ramp is a multiply rather than a divide per
    // sample. The last sample of the block is one step short of `to`, which is
    // what makes the next block's first sample continuous with it.
    let step = 1.0 / frames as f64;

    for (object, signal) in signals.iter().enumerate() {
        let signal = signal.as_ref();
        assert_eq!(signal.len(), frames, "object {object} is not one block");
        let gain = gains[object];
        if gain == 0.0 {
            continue;
        }
        for (element, target) in out.iter_mut().enumerate() {
            let start = from[object][element];
            let end = to[object][element];
            // A weight that is zero at both ends contributes nothing at any
            // point between them, which is most of this matrix.
            if start == 0.0 && end == 0.0 {
                continue;
            }
            let slope = (end - start) * step;
            for (frame, (sample, output)) in signal.iter().zip(target.iter_mut()).enumerate() {
                let weight = start + slope * frame as f64;
                *output += (f64::from(*sample) * gain * weight) as f32;
            }
        }
    }
}

/// A limiter on what the bound leaves: one gain for every element, smoothed
/// ahead of the peak.
///
/// See [`crate::headroom`] for why the gain is common — a gain per element
/// would move the image of an object spread over several — and why there is
/// little for this to do once the fit is bounded. Offline, the whole block
/// is in hand before any of it is written, so the gain reaches its lowest
/// *at* the peak and comes down over [`Limiter::ATTACK`] samples before it,
/// rather than reacting after; it then releases over [`Limiter::RELEASE`]
/// samples. The gain at the end of a block carries into the next, so the
/// join is continuous; a peak in the first attack's worth of a block is
/// reached with what ramp there is.
#[derive(Debug, Clone)]
pub struct Limiter {
    /// The gain at the end of the last block, which the next starts from.
    gain: f64,
    /// The gain every sample needs on its own, and the gain envelope made
    /// of it, both reused.
    needed: Vec<f64>,
    envelope: Vec<f64>,
    /// How many blocks the gain went below one in.
    pub limited: u64,
    /// The loudest peak seen before limiting, as a fraction of the ceiling.
    pub peak: f64,
}

impl Limiter {
    /// Samples the gain comes down over before a peak, two milliseconds at
    /// forty-eight kilohertz.
    pub const ATTACK: usize = 96;
    /// Samples the gain comes back up over after one, fifty milliseconds.
    pub const RELEASE: usize = 2400;

    pub fn new() -> Self {
        Self {
            gain: 1.0,
            needed: Vec::new(),
            envelope: Vec::new(),
            limited: 0,
            peak: 0.0,
        }
    }

    /// Hold every element of the block under `ceiling`, with one gain for
    /// all of them. Returns whether the gain went below one in this block.
    pub fn apply(&mut self, out: &mut [Vec<f32>], ceiling: f64) -> bool {
        self.apply_to(out, ceiling, &[])
    }

    /// As [`Limiter::apply`], over only the elements the caller says carry
    /// anything. An empty `live` means all of them, which is what a fold
    /// wants: every element it writes is a mix.
    ///
    /// An overlay is the other case. Most of its elements are *copied* — taken
    /// from the master and never passed through here at all — so their rows
    /// are left at nought, and scanning them twice a block to discover that
    /// is the one piece of work an overlay can simply not do. It changes no
    /// answer: a row of noughts can neither set the peak nor need a gain.
    pub fn apply_to(&mut self, out: &mut [Vec<f32>], ceiling: f64, live: &[bool]) -> bool {
        let frames = out.first().map_or(0, Vec::len);
        if frames == 0 {
            return false;
        }
        let carries =
            |element: usize| live.is_empty() || live.get(element).copied().unwrap_or(true);
        // What the gain has to be at every sample for nothing to pass the
        // ceiling: one where nothing does.
        self.needed.clear();
        self.needed.resize(frames, 1.0);
        let mut over = false;
        for (index, element) in out.iter().enumerate() {
            if !carries(index) {
                continue;
            }
            for (frame, sample) in element.iter().enumerate() {
                let magnitude = f64::from(sample.abs());
                self.peak = self.peak.max(magnitude / ceiling);
                if magnitude > ceiling {
                    over = true;
                    self.needed[frame] = self.needed[frame].min(ceiling / magnitude);
                }
            }
        }
        if !over && self.gain >= 1.0 {
            return false;
        }
        // Ahead of every peak: the gain at a sample is the lowest needed in
        // the attack's worth of samples after it, so it is down by the time
        // the peak arrives. Walked backwards, a running minimum over a
        // window is one comparison a sample, and a look back over the window
        // only when the minimum it was carrying falls out of it.
        self.envelope.clear();
        self.envelope.resize(frames, 1.0);
        let mut ahead = 1.0f64;
        let mut since = 0usize;
        for frame in (0..frames).rev() {
            let needed = self.needed[frame];
            if needed <= ahead {
                ahead = needed;
                since = 0;
            } else {
                since += 1;
                if since > Self::ATTACK {
                    ahead = self.needed[frame..(frame + Self::ATTACK + 1).min(frames)]
                        .iter()
                        .copied()
                        .fold(1.0, f64::min);
                    since = 0;
                }
            }
            self.envelope[frame] = ahead;
        }
        // And released after it: the gain rises no faster than the release
        // allows, from wherever the last block left it.
        let rise = 1.0 / Self::RELEASE as f64;
        let mut gain = self.gain;
        let mut lowest = 1.0f64;
        for frame in 0..frames {
            gain = (gain + rise).min(1.0).min(self.envelope[frame]);
            self.envelope[frame] = gain;
            lowest = lowest.min(gain);
        }
        self.gain = gain;
        if lowest >= 1.0 {
            return false;
        }
        for (index, element) in out.iter_mut().enumerate() {
            if !carries(index) {
                continue;
            }
            for (sample, gain) in element.iter_mut().zip(&self.envelope) {
                *sample = (f64::from(*sample) * gain) as f32;
            }
        }
        self.limited += 1;
        true
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block that passes the ceiling comes out under it, with one gain on
    /// every element, down before the peak rather than after it.
    #[test]
    fn the_limiter_holds_the_ceiling_with_one_gain() {
        let frames = 4800;
        let mut out = vec![vec![0.5f32; frames], vec![0.1f32; frames]];
        // A peak at the middle of the block, on one element only.
        out[0][2400] = 1.5;
        let mut limiter = Limiter::new();
        assert!(limiter.apply(&mut out, 1.0));
        assert!(out[0].iter().all(|s| *s <= 1.0 + 1e-6));
        assert!((out[0][2400] - 1.0).abs() < 1e-3, "{}", out[0][2400]);
        // The other element came down by the same gain at the peak.
        assert!((out[1][2400] - 0.1 / 1.5).abs() < 1e-3, "{}", out[1][2400]);
        // And the gain was already down before the peak, not only at it.
        assert!(out[0][2400 - Limiter::ATTACK / 2] < 0.5 - 1e-3);
        assert!((out[0][0] - 0.5).abs() < 1e-6);
        assert_eq!(limiter.limited, 1);
        // A block under the ceiling is left alone once the gain has
        // released.
        let mut quiet = vec![vec![0.2f32; frames]];
        limiter.apply(&mut quiet, 1.0);
        let mut again = vec![vec![0.2f32; frames]];
        assert!(!limiter.apply(&mut again, 1.0));
        assert!(again[0].iter().all(|s| (*s - 0.2).abs() < 1e-6));
    }

    /// An object on one element, at unit weight, comes out as itself.
    #[test]
    fn an_object_with_an_element_to_itself_is_unchanged() {
        let signal: Vec<f32> = (0..40).map(|n| (n as f32 - 20.0) / 40.0).collect();
        let weights = vec![vec![1.0, 0.0]];
        let mut out = vec![Vec::new(), Vec::new()];
        mix(&[&signal], &[1.0], &weights, &weights, 40, &mut out);
        assert_eq!(out[0], signal);
        assert!(out[1].iter().all(|s| *s == 0.0));
    }

    /// Its own gain goes on before the mix, so two objects sharing an element
    /// keep their own.
    #[test]
    fn each_object_carries_its_own_gain() {
        let a = vec![1.0f32; 8];
        let b = vec![1.0f32; 8];
        let weights = vec![vec![1.0], vec![1.0]];
        let mut out = vec![Vec::new()];
        mix(&[&a, &b], &[0.25, 0.5], &weights, &weights, 8, &mut out);
        assert!(
            out[0].iter().all(|s| (*s - 0.75).abs() < 1e-6),
            "{:?}",
            out[0]
        );
    }

    /// A weight that changes between blocks arrives gradually, not as a step:
    /// a step in a signal is a click, twenty-five times a second.
    #[test]
    fn a_weight_that_changes_ramps_across_the_block() {
        let signal = vec![1.0f32; 4];
        let from = vec![vec![0.0]];
        let to = vec![vec![1.0]];
        let mut out = vec![Vec::new()];
        mix(&[&signal], &[1.0], &from, &to, 4, &mut out);
        assert_eq!(out[0], vec![0.0, 0.25, 0.5, 0.75]);
        // And the block after it starts where this one left off, which is what
        // makes the join continuous rather than nearly so.
        mix(&[&signal], &[1.0], &to, &to, 4, &mut out);
        assert_eq!(out[0], vec![1.0, 1.0, 1.0, 1.0]);
    }

    /// A silent object costs nothing and contributes nothing.
    #[test]
    fn a_silent_object_is_skipped() {
        let signal = vec![1.0f32; 4];
        let weights = vec![vec![1.0]];
        let mut out = vec![Vec::new()];
        mix(&[&signal], &[0.0], &weights, &weights, 4, &mut out);
        assert_eq!(out[0], vec![0.0; 4]);
    }
}
