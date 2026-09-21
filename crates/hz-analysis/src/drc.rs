//! Dynamic range metadata: the fields a decoder acts on, and the curve that
//! decides what goes in them.
//!
//! # What the standard gives, and what it does not
//!
//! ATSC A/52 §7.7 specifies the **encoding** of two gain words exactly, and
//! §5.4.2.8 the dialogue normalisation field. Those are implemented here from
//! the standard and are exact.
//!
//! It does **not** specify the named compression characteristics — *film
//! light*, *film standard*, *music light*, *music standard*, *speech* — that
//! an encoding job selects between. Those are the reference encoder's own
//! curves, and they are not in the public standard: A/52 describes the wire
//! format for a gain, the mechanics of applying it, and the *intent* of
//! compressing towards dialogue level, and stops there.
//!
//! An earlier version of this project's plan claimed A/52 §7.7 specified them
//! exactly and that implementing them was spec work. It does not, and it is
//! not. What is here instead is the shape a compression characteristic has —
//! a null band around dialogue level, a boost region below it and a cut region
//! above, each with a ratio and a limit — with parameters, and one documented
//! default that is **ours and not anybody else's**.
//!
//! The named characteristics are published, just not there: the format owner
//! gives them in its own metadata guide, so implementing them is reading a
//! specification rather than measuring an encoder. What that guide does not
//! give is the dynamics around the characteristic — attack, release, the
//! weighting the level is measured through, what limits a boost — and those
//! are ours to design and to fit. See `docs/drc.md`.

/// A/52 §5.4.2.8: how far dialogue sits below full scale, in whole decibels.
///
/// Five bits, valid 1..=31, and zero is reserved — a decoder that receives it
/// uses −31 dB. So the field cannot say "dialogue is at full scale" and cannot
/// say "quieter than −31"; both ends clamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialNorm(u8);

impl DialNorm {
    pub const MIN: u8 = 1;
    pub const MAX: u8 = 31;

    /// The field for a programme measured at `loudness` LUFS.
    ///
    /// Silence, or a programme quieter than −31 LUFS, becomes −31: that is the
    /// floor the field has, not a choice made here.
    pub fn for_loudness(loudness: f64) -> Self {
        if !loudness.is_finite() {
            return Self(Self::MAX);
        }
        let decibels = (-loudness).round();
        Self(decibels.clamp(f64::from(Self::MIN), f64::from(Self::MAX)) as u8)
    }

    /// The raw field value, 1..=31.
    pub fn value(self) -> u8 {
        self.0
    }

    /// The dialogue level this field states, in dB relative to full scale.
    pub fn level_db(self) -> f64 {
        -f64::from(if self.0 == 0 { Self::MAX } else { self.0 })
    }

    /// Read a field off the wire, mapping the reserved zero the way a decoder
    /// must.
    pub fn from_field(value: u8) -> Self {
        Self(if value == 0 || value > Self::MAX {
            Self::MAX
        } else {
            value
        })
    }
}

/// A/52 §7.7.1.2: the per-block dynamic range gain word.
///
/// Eight bits as `X0 X1 X2 . Y3 Y4 Y5 Y6 Y7`, where `X` is a signed three-bit
/// integer and `Y` an unsigned five-bit fraction with an implied leading one.
/// The gain is `2^(X+1) × (32+Y)/64`, which spans +23.95 dB to −24.08 dB, and
/// the all-zero code is unity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dynrng(pub u8);

/// A/52 §7.7.2.2: the per-frame heavy compression word.
///
/// Same idea with the split moved: `X` is a signed four-bit integer and `Y` an
/// unsigned four-bit fraction, giving `2^(X+1) × (16+Y)/32` — twice the range
/// at half the resolution, +47.89 dB to −48.16 dB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compr(pub u8);

/// The two words differ only in where the byte is split, so the arithmetic is
/// written once.
macro_rules! gain_word {
    ($name:ident, $exponent_bits:expr, $mantissa_bits:expr) => {
        impl $name {
            const EXPONENT_BITS: u32 = $exponent_bits;
            const MANTISSA_BITS: u32 = $mantissa_bits;
            const MANTISSA_MASK: u8 = ((1u16 << $mantissa_bits) - 1) as u8;
            /// The implied leading one, as a denominator.
            const MANTISSA_SCALE: f64 = (1u32 << ($mantissa_bits + 1)) as f64;
            const EXPONENT_MIN: i32 = -(1 << ($exponent_bits - 1));
            const EXPONENT_MAX: i32 = (1 << ($exponent_bits - 1)) - 1;

            /// The gain this word means, as a linear factor.
            pub fn to_linear(self) -> f64 {
                let exponent = sign_extend(self.0 >> Self::MANTISSA_BITS, Self::EXPONENT_BITS);
                let mantissa = self.0 & Self::MANTISSA_MASK;
                let numerator = f64::from(mantissa) + Self::MANTISSA_SCALE / 2.0;
                exp2(exponent + 1) * (numerator / Self::MANTISSA_SCALE)
            }

            /// The gain this word means, in decibels.
            pub fn to_db(self) -> f64 {
                20.0 * self.to_linear().log10()
            }

            /// The word nearest to a linear gain, clamped to what it can say.
            pub fn from_linear(gain: f64) -> Self {
                // NaN and anything at or below zero take the floor: a gain
                // word cannot say silence, and the floor is the closest it has.
                if gain.is_nan() || gain <= 0.0 {
                    return Self::floor();
                }

                // Split into an exponent and a mantissa in [1/2, 1), which is
                // the range the implied leading one gives.
                let mut exponent = gain.log2().floor() as i32 + 1;
                let mut scaled = (gain / exp2(exponent) * Self::MANTISSA_SCALE).round();

                // Rounding can push the mantissa to the top of its range, where
                // it belongs to the next exponent instead.
                if scaled >= Self::MANTISSA_SCALE {
                    exponent += 1;
                    scaled = (gain / exp2(exponent) * Self::MANTISSA_SCALE).round();
                }

                if exponent - 1 > Self::EXPONENT_MAX {
                    return Self::ceiling();
                }
                if exponent - 1 < Self::EXPONENT_MIN {
                    return Self::floor();
                }

                let mantissa = (scaled - Self::MANTISSA_SCALE / 2.0)
                    .clamp(0.0, f64::from(Self::MANTISSA_MASK)) as u8;
                let exponent = ((exponent - 1) as u8) & ((1u8 << Self::EXPONENT_BITS) - 1);
                Self((exponent << Self::MANTISSA_BITS) | mantissa)
            }

            /// The word nearest to a gain in decibels.
            pub fn from_db(db: f64) -> Self {
                Self::from_linear(10f64.powf(db / 20.0))
            }

            /// The quietest gain this word can state.
            pub fn floor() -> Self {
                Self(
                    ((Self::EXPONENT_MIN as u8) & ((1u8 << Self::EXPONENT_BITS) - 1))
                        << Self::MANTISSA_BITS,
                )
            }

            /// The loudest gain this word can state.
            pub fn ceiling() -> Self {
                Self(((Self::EXPONENT_MAX as u8) << Self::MANTISSA_BITS) | Self::MANTISSA_MASK)
            }

            /// Unity, which A/52 gives the all-zero code.
            pub fn unity() -> Self {
                Self(0)
            }
        }
    };
}

gain_word!(Dynrng, 3, 5);
gain_word!(Compr, 4, 4);

fn sign_extend(value: u8, bits: u32) -> i32 {
    let shift = 32 - bits;
    (i32::from(value) << shift) >> shift
}

fn exp2(exponent: i32) -> f64 {
    // `powi` rather than a shift: the exponent goes negative.
    2f64.powi(exponent)
}

/// The shape of a compression characteristic.
///
/// Levels are relative to dialogue: zero is dialogue level, positive is louder.
/// Below the null band the signal is boosted towards dialogue, above it the
/// signal is cut towards dialogue, and inside it nothing happens — which is
/// the point, since dialogue is the reference the listener sets by ear.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Characteristic {
    pub name: &'static str,
    /// How far above dialogue level cutting starts.
    pub cut_threshold_db: f64,
    /// How far below dialogue level boosting starts.
    pub boost_threshold_db: f64,
    /// Fraction of the excess above the cut threshold that is removed.
    pub cut_ratio: f64,
    /// Fraction of the shortfall below the boost threshold that is made up.
    pub boost_ratio: f64,
    pub max_cut_db: f64,
    pub max_boost_db: f64,
}

impl Characteristic {
    /// A general-purpose film curve.
    ///
    /// **This is ours.** It is not *film light*, *film standard* or any of the
    /// named characteristics under another name. Those are published — by the
    /// format owner, in its metadata guide — and when they are implemented
    /// they will be transcribed from it rather than approximated here, which
    /// is why this one does not pretend to be one of them.
    pub const fn wide() -> Self {
        Self {
            name: "wide",
            cut_threshold_db: 6.0,
            boost_threshold_db: -20.0,
            cut_ratio: 0.5,
            boost_ratio: 0.5,
            max_cut_db: 12.0,
            max_boost_db: 6.0,
        }
    }

    /// A firmer curve for restricted listening, where quiet must stay audible
    /// and loud must not startle.
    pub const fn narrow() -> Self {
        Self {
            name: "narrow",
            cut_threshold_db: 3.0,
            boost_threshold_db: -15.0,
            cut_ratio: 0.7,
            boost_ratio: 0.7,
            max_cut_db: 20.0,
            max_boost_db: 12.0,
        }
    }

    /// The gain to apply to a signal sitting `level_db` from dialogue level.
    pub fn gain_db(&self, level_db: f64) -> f64 {
        if level_db > self.cut_threshold_db {
            let excess = level_db - self.cut_threshold_db;
            -(excess * self.cut_ratio).min(self.max_cut_db)
        } else if level_db < self.boost_threshold_db {
            let shortfall = self.boost_threshold_db - level_db;
            (shortfall * self.boost_ratio).min(self.max_boost_db)
        } else {
            0.0
        }
    }

    /// The gain word for a signal at `level_db` from dialogue level.
    pub fn dynrng(&self, level_db: f64) -> Dynrng {
        Dynrng::from_db(self.gain_db(level_db))
    }
}

/// A gain characteristic **measured off shipped streams**, as a table.
///
/// [`Characteristic`] above is a shape with parameters and its defaults are
/// ours. This is the other thing: what reference streams actually state, read
/// out of their bitstreams and binned by the level they answer to. See
/// `docs/drc.md` for how it was taken and `cargo xtask drc` for taking it
/// again.
///
/// A table and not a formula because it is a measurement, and a formula would
/// claim a shape the measurement does not establish — the named
/// characteristics the format owner publishes are piecewise linear, but which
/// of them a stream was authored with is not in the stream, and the dynamics
/// that shaped each word are not either. Between the points it interpolates;
/// past either end it holds, which is what the ends of the measurement can
/// support and no more.
#[derive(Debug, Clone, Copy)]
pub struct Measured {
    pub name: &'static str,
    /// `(level of the presentation in dBFS, gain in dB)`, rising in level.
    pub points: &'static [(f64, f64)],
}

impl Measured {
    /// What to state for a presentation sitting at this level.
    pub fn gain_db(&self, level_db: f64) -> f64 {
        let points = self.points;
        match points {
            [] => 0.0,
            [only] => only.1,
            _ => {
                if level_db <= points[0].0 {
                    return points[0].1;
                }
                if level_db >= points[points.len() - 1].0 {
                    return points[points.len() - 1].1;
                }
                let above = points
                    .iter()
                    .position(|point| point.0 >= level_db)
                    .unwrap_or(points.len() - 1);
                let (low, high) = (points[above - 1], points[above]);
                let across = (level_db - low.0) / (high.0 - low.0);
                low.1 + across * (high.1 - low.1)
            }
        }
    }

    /// The word a decoder reads, for a presentation at this level.
    pub fn dynrng(&self, level_db: f64) -> Dynrng {
        Dynrng::from_db(self.gain_db(level_db))
    }
}

/// What the presentations wider than stereo state, measured.
///
/// Eight reference streams, the six- and eight-channel presentations, about
/// 24 000 gain words. The three wide substreams agree closely enough to be one
/// curve: at −24 dBFS they state −1.69, −1.69 and −1.98 dB.
///
/// A null band to about −33 dBFS, then a ratio that rises from 1.2:1 through
/// 1.5:1 to around 3:1 above −15 dBFS. The points are the bin medians.
pub const WIDE: Measured = Measured {
    name: "wide, measured",
    points: &[
        (-40.5, 1.1),
        (-37.5, 0.4),
        (-34.5, 0.1),
        (-31.5, 0.0),
        (-28.5, -0.6),
        (-25.5, -1.1),
        (-22.5, -1.7),
        (-19.5, -2.8),
        (-16.5, -4.1),
        (-13.5, -6.0),
        (-10.5, -8.4),
        (-7.5, -9.9),
        (-4.5, -12.0),
    ],
};

/// And what the two-channel presentation states, which is not the same curve.
///
/// The same shape pulled down and steepened: never at unity, and −0.94 dB/dB
/// between −24 and −18 dBFS, which is steep enough to be a limiter. That gap is
/// what a downmix costs — the fewer the channels, the more the sum needs
/// holding back.
///
/// 🔴 It applies to a presentation that **is** a downmix. A stream whose
/// narrow presentations are copies of the first channels rather than folds of
/// all of them has nothing for this curve to protect, and stating it would
/// compress for a summation that never happened.
pub const STEREO: Measured = Measured {
    name: "two-channel, measured",
    points: &[
        (-40.5, -1.2),
        (-37.5, -1.3),
        (-34.5, -2.1),
        (-31.5, -3.4),
        (-28.5, -4.4),
        (-25.5, -6.2),
        (-22.5, -9.0),
        (-19.5, -11.9),
        (-16.5, -14.2),
        (-13.5, -16.7),
        (-10.5, -18.6),
        (-7.5, -20.8),
    ],
};

/// The level a measured curve is a function of.
///
/// A one-pole leaky integrator over the presentation's power, which is the
/// simplest detector that could be right — and the correlation between what
/// reference streams state and what this reads peaks sharply at a time
/// constant near [`TAU_MS`], between 0.80 and 0.98 on every reference stream
/// tried. That peak is the evidence for it; the residual spread of 1 to 1.7 dB
/// is the evidence that the reference's detector is not exactly this one.
#[derive(Debug, Clone, Copy)]
pub struct Level {
    per_step: f64,
    held: f64,
}

/// What the detector's time constant measures as, in milliseconds.
pub const TAU_MS: f64 = 700.0;

impl Level {
    /// A detector fed `rate` times a second.
    pub fn new(rate: f64) -> Self {
        Self {
            per_step: (1.0 / (TAU_MS / 1000.0 * rate)).clamp(f64::MIN_POSITIVE, 1.0),
            held: 0.0,
        }
    }

    /// Feed one step's mean power, and read the level in dBFS.
    ///
    /// Full scale on one channel is a power of one, and a presentation's power
    /// is summed over its channels — which is how the measurement took it, so
    /// it is how a caller has to feed it.
    pub fn feed(&mut self, power: f64) -> f64 {
        self.held += self.per_step * (power.max(0.0) - self.held);
        10.0 * self.held.max(1e-12).log10()
    }

    /// What it currently reads, without feeding it.
    pub fn level_db(&self) -> f64 {
        10.0 * self.held.max(1e-12).log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A/52 gives the all-zero code to unity, and states the two ends of each
    /// word's range. If the arithmetic is wrong these are where it shows.
    #[test]
    fn the_gain_words_have_the_range_the_standard_states() {
        assert_eq!(Dynrng::unity().to_linear(), 1.0);
        assert!((Dynrng::ceiling().to_db() - 23.95).abs() < 0.01);
        assert!((Dynrng::floor().to_db() + 24.08).abs() < 0.01);

        assert_eq!(Compr::unity().to_linear(), 1.0);
        assert!((Compr::ceiling().to_db() - 47.89).abs() < 0.01);
        assert!((Compr::floor().to_db() + 48.16).abs() < 0.01);
    }

    /// Table 7.29: the three most significant bits, in 6.02 dB steps.
    #[test]
    fn the_exponent_table_matches_the_standard() {
        for (bits, gain_db) in [
            (0b011u8, 24.08),
            (0b010, 18.06),
            (0b001, 12.04),
            (0b000, 6.02),
            (0b111, 0.0),
            (0b110, -6.02),
            (0b101, -12.04),
            (0b100, -18.06),
        ] {
            // With the mantissa at its implied leading one — the top of its
            // range is 63/64, so the exact table value is the half-step above.
            let word = Dynrng(bits << 5);
            let expected = gain_db - 6.0206;
            assert!(
                (word.to_db() - expected).abs() < 0.01,
                "{bits:03b}: {} vs {expected}",
                word.to_db()
            );
        }
    }

    /// Every one of the 256 codes has to survive being read and written back,
    /// or an encoder cannot round-trip its own output.
    #[test]
    fn every_code_round_trips() {
        for raw in 0..=255u8 {
            let word = Dynrng(raw);
            assert_eq!(
                Dynrng::from_linear(word.to_linear()),
                word,
                "dynrng {raw:#04x}"
            );

            let word = Compr(raw);
            assert_eq!(
                Compr::from_linear(word.to_linear()),
                word,
                "compr {raw:#04x}"
            );
        }
    }

    /// And a gain asked for lands within half a step of the word.
    ///
    /// Half a step is 0.14 dB, not the 0.125 that "0.25 dB resolution" — the
    /// figure A/52 itself quotes — suggests. The mantissa is a fraction with
    /// an implied leading one, so its steps are widest at the bottom of its
    /// range: 32→33 is 0.267 dB while 62→63 is 0.14. The resolution is a
    /// worst case, not a constant.
    #[test]
    fn a_requested_gain_lands_within_half_a_step() {
        const HALF_STEP_DB: f64 = 0.14;

        let mut db = -24.0;
        while db <= 23.9 {
            let word = Dynrng::from_db(db);
            assert!(
                (word.to_db() - db).abs() < HALF_STEP_DB,
                "{db} dB became {}",
                word.to_db()
            );
            db += 0.37;
        }

        // `compr` trades resolution for range: half a step is twice as wide.
        let mut db = -48.0;
        while db <= 47.8 {
            let word = Compr::from_db(db);
            assert!(
                (word.to_db() - db).abs() < 2.0 * HALF_STEP_DB,
                "compr: {db} dB became {}",
                word.to_db()
            );
            db += 0.61;
        }
    }

    #[test]
    fn a_gain_beyond_the_range_clamps_rather_than_wrapping() {
        assert_eq!(Dynrng::from_db(40.0), Dynrng::ceiling());
        assert_eq!(Dynrng::from_db(-40.0), Dynrng::floor());
        assert_eq!(Dynrng::from_linear(0.0), Dynrng::floor());
        assert_eq!(Dynrng::from_linear(-1.0), Dynrng::floor());
    }

    /// The field says how far dialogue is below full scale, in whole dB, and
    /// it has a floor of −31 that is the format's and not a choice here.
    #[test]
    fn dialnorm_states_the_measured_level() {
        assert_eq!(DialNorm::for_loudness(-23.0).value(), 23);
        assert_eq!(DialNorm::for_loudness(-23.4).value(), 23);
        assert_eq!(DialNorm::for_loudness(-23.6).value(), 24);
        assert_eq!(DialNorm::for_loudness(-27.0).level_db(), -27.0);
    }

    #[test]
    fn dialnorm_clamps_at_both_ends_and_maps_the_reserved_value() {
        assert_eq!(DialNorm::for_loudness(-0.2).value(), DialNorm::MIN);
        assert_eq!(DialNorm::for_loudness(-45.0).value(), DialNorm::MAX);
        assert_eq!(
            DialNorm::for_loudness(f64::NEG_INFINITY).value(),
            DialNorm::MAX
        );
        // Zero is reserved; a decoder that receives it uses −31.
        assert_eq!(DialNorm::from_field(0).level_db(), -31.0);
    }

    /// Dialogue itself is left alone. A curve that moves it is a curve that
    /// defeats the reference the whole system is built on.
    #[test]
    fn the_null_band_leaves_dialogue_untouched() {
        for characteristic in [Characteristic::wide(), Characteristic::narrow()] {
            assert_eq!(characteristic.gain_db(0.0), 0.0, "{}", characteristic.name);
            assert_eq!(characteristic.gain_db(1.0), 0.0);
            assert_eq!(characteristic.gain_db(-5.0), 0.0);
        }
    }

    #[test]
    fn loud_is_cut_and_quiet_is_boosted_within_limits() {
        let curve = Characteristic::wide();

        assert!(curve.gain_db(20.0) < 0.0, "loud was not cut");
        assert!(curve.gain_db(-40.0) > 0.0, "quiet was not boosted");

        assert!((curve.gain_db(100.0) + curve.max_cut_db).abs() < 1e-9);
        assert!((curve.gain_db(-100.0) - curve.max_boost_db).abs() < 1e-9);
    }

    /// The curve must never fold back on itself: a louder input that comes out
    /// quieter than a softer one is a pumping artefact with a schedule.
    #[test]
    fn the_curve_is_monotonic_in_output_level() {
        for curve in [Characteristic::wide(), Characteristic::narrow()] {
            let mut previous = f64::NEG_INFINITY;
            let mut level = -80.0;
            while level <= 40.0 {
                let output = level + curve.gain_db(level);
                assert!(
                    output >= previous - 1e-9,
                    "{}: {level} dB came out below the level before it",
                    curve.name
                );
                previous = output;
                level += 0.25;
            }
        }
    }

    /// The measured curves have the shape the measurement reported: a null
    /// band on the wide one, none on the stereo one, and both monotone.
    #[test]
    fn the_measured_curves_keep_the_shape_they_were_measured_with() {
        for curve in [WIDE, STEREO] {
            // Monotone in output level: a louder input that came out quieter
            // than a softer one is a pumping artefact with a schedule.
            let mut previous = f64::NEG_INFINITY;
            let mut level = -60.0;
            while level < 0.0 {
                let output = level + curve.gain_db(level);
                assert!(
                    output >= previous - 1e-9,
                    "{}: {level} dBFS came out below the level before it",
                    curve.name
                );
                previous = output;
                level += 0.25;
            }
            // And it holds past the ends rather than extrapolating into a
            // region nothing was measured in.
            assert_eq!(curve.gain_db(-120.0), curve.points[0].1);
            assert_eq!(curve.gain_db(6.0), curve.points[curve.points.len() - 1].1);
        }
        // The wide curve leaves quiet material alone; the stereo one never
        // does, because a downmix sums whatever it is given.
        assert!(WIDE.gain_db(-31.5).abs() < 0.01);
        assert!(STEREO.gain_db(-31.5) < -3.0);
        // And the stereo curve cuts harder at every level they share.
        for level in [-36.0, -30.0, -24.0, -18.0, -12.0] {
            assert!(
                STEREO.gain_db(level) < WIDE.gain_db(level),
                "at {level} dBFS the downmix was not held back harder"
            );
        }
    }

    /// The detector reaches a steady level, and takes about its time constant
    /// to get most of the way there.
    #[test]
    fn the_detector_settles_on_its_time_constant() {
        const RATE: f64 = 1200.0; // access units a second
        let mut level = Level::new(RATE);
        // A step to −20 dBFS, held.
        let power = 10f64.powf(-20.0 / 10.0);
        let mut after_tau = 0.0;
        let steps = (TAU_MS / 1000.0 * RATE) as usize;
        for step in 0..steps * 8 {
            let db = level.feed(power);
            if step + 1 == steps {
                after_tau = db;
            }
        }
        // One time constant is 1 − 1/e of the way in power, which is −1.9 dB
        // short of the target.
        assert!(
            (after_tau - (-20.0 - 1.9)).abs() < 0.3,
            "one time constant reached {after_tau:.2} dBFS"
        );
        assert!((level.level_db() + 20.0).abs() < 0.05, "it settles on it");
    }

    #[test]
    fn a_characteristic_produces_a_word_a_decoder_can_read() {
        let curve = Characteristic::wide();
        let word = curve.dynrng(30.0);
        assert!(word.to_db() < 0.0);
        assert!((word.to_db() - curve.gain_db(30.0)).abs() < 0.14);
    }
}
