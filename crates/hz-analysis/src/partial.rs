//! Specific loudness, and what one sound adds to the loudness of the rest.
//!
//! # The law
//!
//! The loudness of a band is not its power. It grows as a *compressed* power
//! of the excitation the band receives — the model of Moore, Glasberg and
//! Baer, which ISO 532-2 standardises:
//!
//! ```text
//! N'(E)  =  C · [ (E + A)^α − A^α ]
//! ```
//!
//! `α` is a fifth, which is what makes ten times the intensity less than
//! twice the loudness. `A` is the term that brings the loudness to nought at
//! the threshold of hearing rather than at silence: twice the excitation a
//! tone at threshold in quiet produces, which the model puts at 4.72 in its own
//! excitation units for the part of the spectrum where the middle ear is flat,
//! five hundred hertz and above. And `C`, which turns the answer into sones,
//! is a scale that cancels in every ratio this project takes; it is not
//! written down so that it cannot be wrong.
//!
//! # What is not reproduced
//!
//! Below five hundred hertz the model raises the threshold and the exponent
//! with it, through a gain, an `α` and an `A` that vary with frequency. Those
//! tables are not carried here. The low end is discounted by the K filter
//! instead, which [`crate::bands`] has already applied by the time an
//! excitation reaches this, and the one set of constants above is used in
//! every band. Above an excitation of `10¹⁰` the model changes form and grows
//! faster again; that branch is not taken either, and the compressive one is
//! continued. Both are the kind of departure a calibration to a playback level
//! would have to revisit, and neither moves a comparison between two objects
//! that are both well above threshold, which is every comparison made here.
//!
//! # Excitation
//!
//! In the model's units: a one-kilohertz tone at nought decibels sound
//! pressure level excites about one. A power in full-scale units becomes an
//! excitation through a calibration — how loud the room plays full scale —
//! which is the caller's to state, since nothing in a stream says.
//!
//! # What a sound adds
//!
//! The importance of one sound among others is not its own loudness. It is
//! what its disappearance would change: the loudness of everything with it
//! in, less the loudness of everything without it, band by band. A sound in
//! a band nothing else occupies adds its whole loudness; the same sound forty
//! decibels under something else in the same band adds almost nothing,
//! because the law is flat up there. That is masking, expressed through the
//! compression rather than through a rule about it, and it is [`added`].

/// The exponent of the compressive law: a fifth.
pub const ALPHA: f64 = 0.2;

/// The threshold term: twice the excitation of a tone at threshold in quiet,
/// in the model's units, for the flat region of the middle ear.
pub const A: f64 = 4.72;

/// The specific loudness a band with this `excitation` produces, up to the
/// scale that would make it sones.
///
/// Nought at nought, and monotone. See the note at the top of this module for
/// the law and for what the constants are.
pub fn specific_loudness(excitation: f64) -> f64 {
    (excitation.max(0.0) + A).powf(ALPHA) - A.powf(ALPHA)
}

/// What a sound adds to the loudness of a band that `others` already excite:
/// the loudness with it in, less the loudness without it.
///
/// `own` and `others` are excitations. The answer is the sound's whole
/// loudness when nothing else is there and falls towards nought as the others
/// grow, which is what being masked means.
pub fn added(own: f64, others: f64) -> f64 {
    let others = others.max(0.0);
    specific_loudness(others + own.max(0.0)) - specific_loudness(others)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nought at silence, monotone, and compressive: ten times the intensity
    /// is not twice the loudness once the threshold is well behind.
    #[test]
    fn the_law_is_compressive() {
        assert_eq!(specific_loudness(0.0), 0.0);
        let mut last = 0.0;
        for decade in 0..12 {
            let now = specific_loudness(10f64.powi(decade));
            assert!(now > last, "{decade}: {now} after {last}");
            last = now;
        }
        // Far above the threshold the law is a fifth power: a decade of
        // intensity is a factor of 10^0.2, a little more for the threshold
        // term that is taken off both.
        let ratio = specific_loudness(1e9) / specific_loudness(1e8);
        assert!((ratio - 10f64.powf(ALPHA)).abs() < 0.03, "{ratio}");
        // And a doubling is well under a doubling.
        assert!(specific_loudness(2e8) < 1.2 * specific_loudness(1e8));
    }

    /// Alone, a sound adds its whole loudness; among others it adds less, and
    /// the more the others the less it adds.
    #[test]
    fn what_a_sound_adds_falls_with_what_it_is_heard_against() {
        let own = 1e8;
        assert_eq!(added(own, 0.0), specific_loudness(own));
        let mut last = f64::INFINITY;
        for decade in 4..12 {
            let now = added(own, 10f64.powi(decade));
            assert!(now < last, "{decade}: {now} after {last}");
            assert!(now > 0.0);
            last = now;
        }
    }

    /// The masking the compression expresses: a sound forty decibels under
    /// another in the same band adds a fifth of a ten-thousandth of that
    /// other's loudness, which is the derivative of a fifth power.
    #[test]
    fn forty_decibels_under_is_all_but_hidden() {
        let loud = 1e9;
        let quiet = loud * 1e-4;
        let share = added(quiet, loud) / specific_loudness(loud);
        assert!((share / (ALPHA * 1e-4) - 1.0).abs() < 0.05, "{share}");
        // Whereas alone it is about a seventh of the loud one — (10^-4)^0.2
        // is a sixth, and the threshold term takes a little more off the
        // quiet one — which is four orders of magnitude from being hidden.
        let alone = specific_loudness(quiet) / specific_loudness(loud);
        assert!(alone > 0.12 && alone < 0.16, "{alone}");
    }

    /// A negative excitation is not a thing, and is taken as silence rather
    /// than as a fractional power of a negative number.
    #[test]
    fn nothing_below_silence() {
        assert_eq!(specific_loudness(-1.0), 0.0);
        assert_eq!(added(-1.0, -1.0), 0.0);
        assert_eq!(added(1e6, -1.0), specific_loudness(1e6));
    }
}
