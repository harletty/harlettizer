//! ADM timestamps, and why the spelling matters.
//!
//! BS.2076 writes a time two ways: the decimal form, `hh:mm:ss.fffffffff`,
//! and — from the second edition — the fractional form,
//! `hh:mm:ss.<numerator>S<denominator>`.
//!
//! Both are read. The decimal form is written, and the reasoning is worth
//! recording because the obvious argument points the other way.
//!
//! A master set addresses events by sample position, and an encoder that
//! moved object updates by a sample would be wrong in a way nobody would ever
//! find. 1/48000 s is 20833.333… ns, which no decimal fraction states exactly,
//! so the fractional form looks like the only safe choice. It is not, because
//! the precision is ours to pick: at nanoseconds the rounding error is at most
//! 0.5 ns against a half-sample margin of 10.4 µs at 48 kHz and 2.6 µs at
//! 192 kHz. Reading a nanosecond timestamp back recovers the exact sample at
//! any rate below about a gigahertz. What is *not* exact is the five
//! fractional digits real documents habitually carry — that margin runs out
//! at 192 kHz — and the fix for that is to write nine digits, not to change
//! form.
//!
//! Meanwhile the fractional form costs interoperability: it is a second-
//! edition feature, and the EBU's reference renderer — the most widely
//! deployed ADM implementation there is — rejects the file outright rather
//! than reading it. That was found by handing it a file this crate wrote, not
//! by reasoning about it.

use hz_core::{Error, Result};
use std::fmt;
use std::path::Path;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3600;

/// A point in time, in whichever form the document used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Time {
    /// `hh:mm:ss.fffffffff` — resolution only, never exactness.
    Decimal { nanos: u64 },
    /// `hh:mm:ss.<numerator>S<denominator>` — exact for any sample rate whose
    /// period divides the denominator.
    Fractional {
        whole_seconds: u64,
        numerator: u64,
        denominator: u64,
    },
}

/// A wide intermediate brought back to the width the type carries.
///
/// Saturating rather than wrapping: a time past what 64 bits hold is a time
/// no file means, and the largest one it can name is a better answer than a
/// small one.
fn narrow(wide: u128) -> u64 {
    u64::try_from(wide).unwrap_or(u64::MAX)
}

impl Time {
    /// A time at an exact sample position.
    pub fn at_sample(sample: u64, sample_rate: u32) -> Self {
        let rate = sample_rate as u64;
        Self::Fractional {
            whole_seconds: sample / rate,
            numerator: sample % rate,
            denominator: rate,
        }
    }

    /// The sample position this time names.
    ///
    /// Exact for the fractional form. For the decimal form it is the nearest
    /// sample, because that is the best the spelling can offer — the loss
    /// happened when the file was written, not here.
    pub fn to_samples(self, sample_rate: u32) -> u64 {
        let rate = sample_rate as u64;
        match self {
            Self::Fractional {
                whole_seconds,
                numerator,
                denominator,
            } => {
                let fraction = if denominator == 0 {
                    0
                } else {
                    // Round to nearest so a denominator that is not the sample
                    // rate still lands where it should. In 128 bits, because
                    // the numerator comes out of a file: a numerator near the
                    // top of its range times a sample rate is past what 64
                    // bits hold, and wrapping there names a sample somewhere
                    // else entirely.
                    let wide =
                        u128::from(numerator) * u128::from(rate) + u128::from(denominator) / 2;
                    wide / u128::from(denominator)
                };
                narrow(u128::from(whole_seconds) * u128::from(rate) + fraction)
            }
            Self::Decimal { nanos } => narrow(
                (u128::from(nanos) * u128::from(rate) + u128::from(NANOS_PER_SECOND) / 2)
                    / u128::from(NANOS_PER_SECOND),
            ),
        }
    }

    /// This time in the decimal form, which is what gets written.
    ///
    /// Nanoseconds, so the result names the same sample it went in as.
    pub fn to_decimal(self) -> Self {
        Self::Decimal {
            nanos: self.to_nanos(),
        }
    }

    /// Nanoseconds since zero, rounded to nearest.
    pub fn to_nanos(self) -> u64 {
        match self {
            Self::Decimal { nanos } => nanos,
            Self::Fractional {
                whole_seconds,
                numerator,
                denominator,
            } => {
                let fraction = if denominator == 0 {
                    0
                } else {
                    let wide = u128::from(numerator) * u128::from(NANOS_PER_SECOND)
                        + u128::from(denominator) / 2;
                    wide / u128::from(denominator)
                };
                narrow(u128::from(whole_seconds) * u128::from(NANOS_PER_SECOND) + fraction)
            }
        }
    }

    /// This time as a number of seconds, for the attributes that carry one.
    pub fn to_seconds(self) -> f64 {
        self.to_nanos() as f64 / NANOS_PER_SECOND as f64
    }

    pub fn parse(path: &Path, text: &str) -> Result<Self> {
        let bad = |what: &str| Error::malformed(path, format!("{what} in ADM time `{text}`"));

        let mut parts = text.split(':');
        let (Some(hours), Some(minutes), Some(rest), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad("expected hh:mm:ss"));
        };

        let hours: u64 = hours.trim().parse().map_err(|_| bad("hours"))?;
        let minutes: u64 = minutes.trim().parse().map_err(|_| bad("minutes"))?;

        let (seconds, fraction) = match rest.split_once('.') {
            Some((seconds, fraction)) => (seconds, Some(fraction)),
            None => (rest, None),
        };
        let seconds: u64 = seconds.trim().parse().map_err(|_| bad("seconds"))?;
        let whole_seconds = hours * SECONDS_PER_HOUR + minutes * SECONDS_PER_MINUTE + seconds;

        match fraction {
            None => Ok(Self::Fractional {
                whole_seconds,
                numerator: 0,
                denominator: 1,
            }),
            Some(fraction) => match fraction.split_once('S') {
                Some((numerator, denominator)) => Ok(Self::Fractional {
                    whole_seconds,
                    numerator: numerator.parse().map_err(|_| bad("numerator"))?,
                    denominator: denominator.parse().map_err(|_| bad("denominator"))?,
                }),
                None => {
                    // A decimal fraction, padded or truncated to nanoseconds.
                    let digits: String = fraction.chars().take(9).collect();
                    if !digits.chars().all(|c| c.is_ascii_digit()) {
                        return Err(bad("fraction"));
                    }
                    let scale = 10u64.pow(9 - digits.len() as u32);
                    let nanos = digits.parse::<u64>().map_err(|_| bad("fraction"))? * scale;
                    Ok(Self::Decimal {
                        nanos: whole_seconds * NANOS_PER_SECOND + nanos,
                    })
                }
            },
        }
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, tail) = match *self {
            Self::Fractional {
                whole_seconds,
                numerator,
                denominator,
            } => (whole_seconds, format!("{numerator}S{denominator}")),
            Self::Decimal { nanos } => (
                nanos / NANOS_PER_SECOND,
                format!("{:09}", nanos % NANOS_PER_SECOND),
            ),
        };
        write!(
            f,
            "{:02}:{:02}:{:02}.{tail}",
            whole / SECONDS_PER_HOUR,
            (whole % SECONDS_PER_HOUR) / SECONDS_PER_MINUTE,
            whole % SECONDS_PER_MINUTE,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Time {
        Time::parse(Path::new("t.wav"), text).unwrap()
    }

    #[test]
    fn a_sample_position_survives_being_written_and_read() {
        for sample in [0u64, 1, 47_999, 48_000, 9_269_760, 345_600_000] {
            let time = Time::at_sample(sample, 48_000);
            let text = time.to_string();
            assert_eq!(parse(&text).to_samples(48_000), sample, "{text}");
        }
    }

    /// Five fractional digits — what documents habitually carry — is coarser
    /// than half a sample at 192 kHz, so sample 1 comes back as sample 2. Nine
    /// digits is not, which is why the fix is precision and not form.
    #[test]
    fn nanoseconds_are_exact_where_five_digits_are_not() {
        assert_eq!(parse("00:00:00.00001").to_samples(192_000), 2);

        for rate in [48_000, 96_000, 192_000] {
            for sample in [0u64, 1, 2, rate as u64 - 1, 9_269_760, 345_600_000] {
                let written = Time::at_sample(sample, rate).to_decimal().to_string();
                assert_eq!(
                    parse(&written).to_samples(rate),
                    sample,
                    "{written} @{rate}"
                );
            }
        }
    }

    /// The decimal form is what gets written, because the EBU's reference
    /// renderer implements the first edition and rejects the other one.
    #[test]
    fn the_written_form_is_decimal() {
        let written = Time::at_sample(1152, 48_000).to_decimal().to_string();
        assert_eq!(written, "00:00:00.024000000");
        assert!(!written.contains('S'));
    }

    /// A ramp length is written as a number of seconds, and has to survive it.
    #[test]
    fn a_length_in_seconds_still_names_its_sample_count() {
        for samples in [0u64, 1, 1151, 1152, 48_000] {
            let seconds = Time::at_sample(samples, 48_000).to_seconds();
            let back = (seconds * 48_000.0).round() as u64;
            assert_eq!(back, samples, "{seconds}");
        }
    }

    #[test]
    fn both_written_forms_are_read() {
        assert_eq!(
            parse("00:00:01.5S48000"),
            Time::Fractional {
                whole_seconds: 1,
                numerator: 5,
                denominator: 48_000
            }
        );
        assert_eq!(
            parse("00:00:01.500000000"),
            Time::Decimal {
                nanos: 1_500_000_000
            }
        );
    }

    #[test]
    fn hours_and_minutes_fold_into_seconds() {
        assert_eq!(parse("01:02:03.0S1").to_samples(1), 3723);
    }

    /// A short decimal fraction is padded, not read as though it were nanos.
    #[test]
    fn a_short_fraction_is_scaled_not_misread() {
        assert_eq!(parse("00:00:00.5"), Time::Decimal { nanos: 500_000_000 });
    }

    #[test]
    fn a_time_with_no_fraction_is_whole_seconds() {
        assert_eq!(parse("00:00:07").to_samples(48_000), 7 * 48_000);
    }

    #[test]
    fn a_denominator_that_is_not_the_sample_rate_still_lands_right() {
        // Half a second written against a 100 Hz grid, read at 48 kHz.
        assert_eq!(parse("00:00:00.50S100").to_samples(48_000), 24_000);
    }

    #[test]
    fn nonsense_is_refused() {
        for text in ["", "00:00", "aa:00:00.0S1", "00:00:00.xS1", "00:00:00.0Sx"] {
            assert!(
                Time::parse(Path::new("t.wav"), text).is_err(),
                "accepted `{text}`"
            );
        }
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    /// A time out of a file is not bounded by anything, and the arithmetic
    /// that turns it into a sample position multiplies it by a sample rate.
    /// In 64 bits that wraps, and a wrapped time names a sample somewhere
    /// else — silently, in release.
    #[test]
    fn an_absurd_time_saturates_rather_than_wrapping() {
        let huge = Time::Fractional {
            whole_seconds: u64::MAX / 2,
            numerator: u64::MAX - 1,
            denominator: u64::MAX,
        };
        assert_eq!(huge.to_samples(48_000), u64::MAX);
        assert_eq!(huge.to_nanos(), u64::MAX);

        // The decimal form cannot overflow on the way out — the largest
        // number of nanoseconds there is comes to 3.5e15 samples at 192 kHz —
        // but it does overflow on the way *in* to the multiplication, which is
        // what the wide intermediate is for. The answer is the exact one.
        let nanos = Time::Decimal { nanos: u64::MAX };
        assert_eq!(nanos.to_samples(192_000), 3_541_774_862_152_234);
    }

    /// And an ordinary time is unaffected, to the sample.
    #[test]
    fn an_ordinary_time_is_exact() {
        for rate in [44_100u32, 48_000, 96_000, 192_000] {
            for sample in [0u64, 1, 12_345, 3 * 3600 * 192_000] {
                let time = Time::at_sample(sample, rate);
                assert_eq!(time.to_samples(rate), sample, "{rate} Hz, sample {sample}");
                assert_eq!(
                    time.to_decimal().to_samples(rate),
                    sample,
                    "{rate} Hz, sample {sample}, through nanoseconds"
                );
            }
        }
    }
}
