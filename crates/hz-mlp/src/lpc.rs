//! Linear prediction: what to subtract from a sample before coding it.
//!
//! The idea is old and simple. Audio is correlated from one sample to the
//! next, so a weighted sum of the samples just gone is a good guess at the
//! next one, and coding the *error* of that guess takes fewer bits than coding
//! the sample. Nothing is lost: the decoder makes the same guess from the same
//! past and adds the error back.
//!
//! This module is the arithmetic — autocorrelation, Levinson-Durbin, and the
//! quantisation that turns real coefficients into the integers the format
//! carries. What the format then does with them is [`crate::filter`].
//!
//! Written from the method rather than ported: Levinson-Durbin is a textbook
//! recursion, and quantising a predictor is a decision about where to spend
//! precision that is better made deliberately than inherited.

/// The most taps the format's FIR filter carries.
pub const MAX_ORDER: usize = 8;

/// Coefficients for every order up to the maximum, and what each would cost.
#[derive(Debug, Clone)]
pub struct Predictors {
    /// `coefficients[order - 1][..order]` for each order.
    pub coefficients: [[f64; MAX_ORDER]; MAX_ORDER],
    /// How many orders are actually filled. Zero when the block is silent.
    pub orders: usize,
}

/// Fit predictors of every order to `samples`.
///
/// `samples` should include whatever history the filter will be started from,
/// because a block of forty samples is short for an eight-tap fit and the
/// context is free.
pub fn fit(samples: &[f64], max_order: usize) -> Predictors {
    let max_order = max_order.min(MAX_ORDER);
    let mut result = Predictors {
        coefficients: [[0.0; MAX_ORDER]; MAX_ORDER],
        orders: 0,
    };
    if max_order == 0 || samples.len() <= max_order {
        return result;
    }

    let mut autocorrelation = [0.0f64; MAX_ORDER + 1];
    correlate(samples, &mut autocorrelation[..=max_order]);

    // A silent or constant block has no correlation structure to find, and
    // dividing by its energy is how a predictor becomes a pile of infinities.
    if autocorrelation[0] <= 0.0 {
        return result;
    }

    levinson(&autocorrelation[..=max_order], &mut result);
    result.orders = max_order;
    result
}

/// Autocorrelation of a Welch-windowed view of the samples.
///
/// Windowed because the recursion below assumes the signal continues either
/// side of the block, and a rectangular window makes it assume a
/// discontinuity at each edge instead — which on a forty-sample block is most
/// of the block.
///
/// The windowed samples are held a chunk at a time and each lag is then a
/// contiguous dot product over them, which is what a machine is fast at. The
/// chunk carries the previous one's last few values in front of it so that a
/// lag can reach back across the seam, and each lag accumulates into its own
/// running total in ascending order — the same order, term for term, as the
/// interleaved version this replaces, so the sums are the same to the bit.
///
/// A fixed buffer with no chunking was tried once and is the thing to avoid:
/// it quietly returned zeroes for anything longer than itself, which reads as
/// "this signal has no structure" and turns the whole predictor off. Chunking
/// has no size to get wrong.
fn correlate(samples: &[f64], out: &mut [f64]) {
    /// The longest block taken with every lag at once. The filter search fits
    /// over its context and the block, at most 512 and 160 samples, so every
    /// call from it is shorter than this.
    const AT_ONCE: usize = 1024;

    let n = samples.len();
    let max_lag = out.len().saturating_sub(1);
    if n == 0 || n > AT_ONCE || max_lag > MAX_ORDER {
        correlate_chunked(samples, out);
        return;
    }
    let centre = (n as f64 - 1.0) / 2.0;

    // Every lag at once.
    //
    // Lag by lag, each sum is one chain of additions, and a chain runs at the
    // speed of one addition's latency however wide the machine is. Walked
    // position by position with an accumulator per lag, the nine chains run
    // side by side. Each still takes **its own terms in its own order**:
    // ascending position, and the terms a lag reaches before the signal
    // starts are products with the zeroes in front of it, which add nothing —
    // nought plus either nought is nought, and they all come before the first
    // real term. So every sum is the one [`correlate_chunked`] makes, to the
    // bit, and so is every predictor and every stream.
    let mut buffer = [0.0f64; MAX_ORDER + AT_ONCE];
    // The window depends on the length and nothing else, and the filter
    // search fits over the same length block after block: worked out once
    // and kept, by the same arithmetic, so each weight is the one this loop
    // would have made — a division a sample saved, and nothing else changed.
    WELCH.with(|window| {
        let mut window = window.borrow_mut();
        if window.len() != n {
            window.clear();
            window.extend((0..n).map(|index| {
                let offset = (index as f64 - centre) / (centre + 1.0);
                1.0 - offset * offset
            }));
        }
        for ((slot, sample), weight) in buffer[MAX_ORDER..MAX_ORDER + n]
            .iter_mut()
            .zip(samples)
            .zip(window.iter())
        {
            *slot = sample * weight;
        }
    });
    let mut sums = [0.0f64; MAX_ORDER + 1];
    for position in MAX_ORDER..MAX_ORDER + n {
        let here = buffer[position];
        for (lag, sum) in sums.iter_mut().enumerate() {
            *sum += here * buffer[position - lag];
        }
    }
    out.copy_from_slice(&sums[..out.len()]);
}

thread_local! {
    /// The Welch window of the last length [`correlate`] was asked for, per
    /// thread.
    static WELCH: std::cell::RefCell<Vec<f64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// [`correlate`] a chunk at a time and a lag at a time, for a block longer
/// than it takes at once.
fn correlate_chunked(samples: &[f64], out: &mut [f64]) {
    /// Windowed samples held at once.
    const CHUNK: usize = 512;

    out.fill(0.0);
    let n = samples.len();
    if n == 0 {
        return;
    }
    let max_lag = out.len() - 1;
    let centre = (n as f64 - 1.0) / 2.0;

    let mut buffer = [0.0f64; MAX_ORDER + CHUNK];
    // How many of the previous chunk's values sit in front of this one.
    let mut carried = 0usize;
    let mut at = 0usize;
    while at < n {
        let take = CHUNK.min(n - at);
        for (offset_in_chunk, slot) in buffer[carried..carried + take].iter_mut().enumerate() {
            let index = at + offset_in_chunk;
            let offset = (index as f64 - centre) / (centre + 1.0);
            *slot = samples[index] * (1.0 - offset * offset);
        }
        let filled = carried + take;

        for (lag, total) in out.iter_mut().enumerate() {
            // The first position whose partner is inside the buffer *and*
            // inside the signal.
            let start = carried.max(lag);
            let mut sum = *total;
            for position in start..filled {
                sum += buffer[position] * buffer[position - lag];
            }
            *total = sum;
        }

        carried = max_lag.min(filled);
        buffer.copy_within(filled - carried..filled, 0);
        at += take;
    }
}

/// Levinson-Durbin: solve for every order at once, which the recursion gives
/// for free on its way to the highest.
fn levinson(autocorrelation: &[f64], out: &mut Predictors) {
    let max_order = autocorrelation.len() - 1;
    let mut error = autocorrelation[0];
    let mut current = [0.0f64; MAX_ORDER];

    for order in 0..max_order {
        let mut accumulator = autocorrelation[order + 1];
        for (tap, coefficient) in current[..order].iter().enumerate() {
            accumulator -= coefficient * autocorrelation[order - tap];
        }
        // An error that has collapsed to nothing means the signal is perfectly
        // predicted already; going on divides by it.
        if error.abs() < f64::EPSILON {
            for remaining in order..max_order {
                out.coefficients[remaining] = out.coefficients[order.saturating_sub(1)];
            }
            return;
        }
        let reflection = accumulator / error;

        // Reflect the previous order's coefficients into this one.
        let previous = current;
        current[order] = reflection;
        for (tap, coefficient) in current[..order].iter_mut().enumerate() {
            *coefficient = previous[tap] - reflection * previous[order - 1 - tap];
        }
        error *= 1.0 - reflection * reflection;

        out.coefficients[order][..=order].copy_from_slice(&current[..=order]);
    }
}

/// Coefficients as the format carries them: integers and a right shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Quantised {
    pub coefficients: [i32; MAX_ORDER],
    pub order: usize,
    /// How far right to shift the accumulator. Four bits in the stream.
    pub shift: u32,
}

/// The most precision the format's coefficient field can hold.
///
/// The field is `coeff_bits` wide and signed, `coeff_bits` is at most 16, and
/// `coeff_bits + coeff_shift` must not exceed 16 — so fifteen bits of value
/// plus a sign is the ceiling, and this stays one under it to leave the
/// quantiser somewhere to round to.
pub const PRECISION: u32 = 15;
/// The filter shift is four bits.
pub const MAX_SHIFT: u32 = 15;

/// The smallest shift the format's decoders accept.
///
/// The field is four bits, so nothing above fifteen fits — but a shift below
/// eight is rejected outright by at least one decoder, and a stream some
/// decoders refuse is not a stream. A predictor whose coefficients are too
/// large to represent at eight fractional bits is therefore not offered at
/// all, rather than clamped into something that decodes differently.
pub const MIN_SHIFT: u32 = 8;

/// Round real coefficients onto the integer grid the format carries.
///
/// The shift is chosen so the largest coefficient uses as much of the field as
/// it can: a predictor quantised into a field it barely fills is a predictor
/// whose fine structure has been rounded away, and the residual grows.
///
/// The rounding error of each coefficient is carried into the next, which
/// costs one add and recovers a fraction of a bit — the same trick a
/// noise-shaped requantiser uses, for the same reason.
pub fn quantise(coefficients: &[f64], order: usize) -> Option<Quantised> {
    quantise_within(coefficients, order, MAX_SHIFT)
}

/// As [`quantise`], with the shift capped at `max_shift`.
///
/// For a first filter that shares its shift with a second one: the second's
/// taps have to fit the coefficient field at that shift too, and they are
/// larger than one.
pub fn quantise_within(coefficients: &[f64], order: usize, max_shift: u32) -> Option<Quantised> {
    if order == 0 || order > MAX_ORDER || max_shift < MIN_SHIFT {
        return None;
    }
    let largest = coefficients[..order]
        .iter()
        .fold(0.0f64, |m, c| m.max(c.abs()));
    if largest <= 0.0 || !largest.is_finite() {
        return None;
    }

    // How far the largest coefficient can be shifted left before it overflows
    // the field.
    let headroom = (PRECISION - 1) as i32 - (largest.log2().floor() as i32) - 1;
    if headroom < MIN_SHIFT as i32 {
        return None;
    }
    let shift = headroom.min(max_shift as i32) as u32;

    let scale = f64::from(1u32 << shift);
    let limit = 1i32 << (PRECISION - 1);
    let mut out = Quantised {
        order,
        shift,
        ..Quantised::default()
    };
    let mut carried = 0.0f64;
    for (slot, coefficient) in out.coefficients[..order]
        .iter_mut()
        .zip(&coefficients[..order])
    {
        let wanted = coefficient * scale + carried;
        let rounded = wanted.round();
        carried = wanted - rounded;
        *slot = (rounded as i64).clamp(i64::from(-limit), i64::from(limit - 1)) as i32;
    }

    if out.coefficients[..order].iter().all(|c| *c == 0) {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first-order autoregressive signal has a known first-order predictor:
    /// its own coefficient. If the recursion cannot recover that, nothing else
    /// it produces means anything.
    /// A deterministic white-ish excitation, so the test does not depend on a
    /// random number generator's habits.
    fn excitation(count: usize) -> Vec<f64> {
        let mut state = 0x2545_f491u32;
        (0..count)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                f64::from(state >> 16) / f64::from(u16::MAX) - 0.5
            })
            .collect()
    }

    /// A first-order autoregressive signal has a known first-order predictor:
    /// its own coefficient. If the recursion cannot recover that, nothing else
    /// it produces means anything.
    #[test]
    fn a_known_process_gives_back_its_own_coefficient() {
        let a = 0.8;
        let mut value = 0.0;
        let samples: Vec<f64> = excitation(4000)
            .into_iter()
            .map(|e| {
                value = a * value + e;
                value
            })
            .collect();

        let fitted = fit(&samples, 4);
        assert!(fitted.orders >= 1);
        let first = fitted.coefficients[0][0];
        assert!(
            (first - a).abs() < 0.05,
            "recovered {first:.3} for a process with coefficient {a}"
        );
    }

    /// And a second-order one, where the reflection step actually does
    /// something: a resonator at a known frequency and radius has predictor
    /// `[2r·cos ω, −r²]`.
    #[test]
    fn a_resonator_gives_back_its_own_predictor() {
        let radius = 0.9;
        let omega = 0.4f64;
        let (a1, a2) = (2.0 * radius * omega.cos(), -radius * radius);
        let (mut back1, mut back2) = (0.0, 0.0);
        let samples: Vec<f64> = excitation(4000)
            .into_iter()
            .map(|e| {
                let value = a1 * back1 + a2 * back2 + e;
                back2 = back1;
                back1 = value;
                value
            })
            .collect();

        let fitted = fit(&samples, 4);
        assert!(fitted.orders >= 2);
        let recovered = fitted.coefficients[1];
        assert!(
            (recovered[0] - a1).abs() < 0.05 && (recovered[1] - a2).abs() < 0.05,
            "recovered [{:.3}, {:.3}] for [{a1:.3}, {a2:.3}]",
            recovered[0],
            recovered[1]
        );
    }

    /// The autocorrelation must not depend on how long the input is beyond
    /// what the signal says — an implementation with a fixed internal buffer
    /// passed every short test and returned zeroes on anything real.
    #[test]
    fn a_long_signal_is_correlated_like_a_short_one() {
        let short: Vec<f64> = (0..64).map(|n| (n as f64 * 0.3).sin()).collect();
        let long: Vec<f64> = (0..5000).map(|n| (n as f64 * 0.3).sin()).collect();
        for samples in [&short, &long] {
            let mut out = [0.0f64; 3];
            correlate(samples, &mut out);
            assert!(
                out[0] > 0.0,
                "{} samples correlated to nothing",
                samples.len()
            );
        }
    }

    /// Every lag at once makes the same sums, to the bit, as a chunk at a
    /// time and a lag at a time — at every length the filter search asks
    /// for, across the chunk's seam, and past what is taken at once.
    #[test]
    fn every_lag_at_once_is_the_same_to_the_bit() {
        let mut state = 0x2545_f491u32;
        for n in [1usize, 2, 8, 9, 40, 511, 512, 513, 552, 672, 1024, 1025] {
            for scale in [1.0f64, 1e-6, 8.0e6] {
                let samples: Vec<f64> = (0..n)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        (f64::from(state as i32) / f64::from(i32::MAX)) * scale
                    })
                    .collect();
                for lags in 1..=MAX_ORDER + 1 {
                    let mut fast = vec![0.0f64; lags];
                    correlate(&samples, &mut fast);
                    let mut slow = vec![0.0f64; lags];
                    correlate_chunked(&samples, &mut slow);
                    for (lag, (a, b)) in fast.iter().zip(&slow).enumerate() {
                        assert_eq!(
                            a.to_bits(),
                            b.to_bits(),
                            "{n} samples at {scale}, lag {lag}: {a} against {b}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn silence_has_no_predictor() {
        let fitted = fit(&[0.0; 64], 8);
        assert_eq!(fitted.orders, 0, "there is nothing to predict");
    }

    /// The quantiser has to spend the precision it is given: a predictor
    /// rounded into a field it barely uses has lost the detail the field was
    /// there to carry.
    ///
    /// Stated as "no larger shift would have been legal", which is the actual
    /// property. Asking instead that the field always come out nearly full is
    /// asking for something the format cannot always give: the shift has four
    /// bits, so a coefficient far below one runs out of shift before it runs
    /// out of field, and there is nothing to be done about that.
    #[test]
    fn quantising_spends_the_precision_it_is_given() {
        for scale in [0.002, 0.02, 0.2, 1.0, 1.8] {
            let coefficients = [scale, -scale / 3.0, scale / 7.0, 0.0, 0.0, 0.0, 0.0, 0.0];
            let quantised = quantise(&coefficients, 3).expect("a predictor");
            if quantised.shift == MAX_SHIFT {
                continue;
            }
            let one_more = (scale * f64::from(1u32 << (quantised.shift + 1))).round();
            assert!(
                one_more >= f64::from(1i32 << (PRECISION - 1)),
                "a coefficient of {scale} took shift {}, and {} would still have fit",
                quantised.shift,
                quantised.shift + 1
            );
        }
    }

    /// And it must never overflow it, whatever it is handed.
    #[test]
    fn quantising_never_overflows_the_field() {
        for scale in [1e-6, 0.001, 0.5, 1.0, 1.9] {
            let coefficients = [scale, -scale, scale, -scale, 0.0, 0.0, 0.0, 0.0];
            if let Some(quantised) = quantise(&coefficients, 4) {
                let limit = 1i32 << (PRECISION - 1);
                for coefficient in &quantised.coefficients[..4] {
                    assert!(
                        (-limit..limit).contains(coefficient),
                        "{coefficient} does not fit {PRECISION} signed bits"
                    );
                }
                assert!(quantised.shift <= MAX_SHIFT);
            }
        }
    }

    #[test]
    fn a_zero_predictor_is_no_predictor() {
        assert!(quantise(&[0.0; MAX_ORDER], 4).is_none());
    }
}
