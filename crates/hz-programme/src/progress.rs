//! How far along a run is, for a caller driving a bar.
//!
//! The same reporter whatever the run writes: a TrueHD stream and an IAMF
//! sequence report the same lines, so a process host parses one format.

/// How far along a run is, on standard error, for a caller driving a bar.
///
/// # Why it is a flag and why it is stderr
///
/// The summary this command prints is its *answer* and goes to standard
/// output; progress is scaffolding that is meaningless once the thing has
/// finished, so it goes to standard error and only when asked for. A caller
/// reading both streams — which is what a process host does — can then tell
/// the two apart by their shape rather than by guessing.
///
/// # One line per whole percent
///
/// Not one per access unit: a two-hour programme is 216 000 of them and a bar
/// cannot show more than a hundred positions anyway. So a line goes out only
/// when the whole percent changes, which caps the output at a hundred lines
/// whatever the length, and the format is fixed and dull —
/// `progress <n>%` — because something is going to parse it with a regular
/// expression.
///
/// The denominator is the input's own frame count, which both containers state
/// in their header. So this is a real fraction of the work and not an estimate
/// from how much has been written, which would depend on how well the material
/// compresses.
pub struct Progress {
    total: u64,
    last: u64,
}

impl Progress {
    /// `None` when the flag is off, or when the input does not say how long it
    /// is — a percentage of an unknown total is a number made up.
    pub fn new(wanted: bool, total: u64) -> Option<Self> {
        (wanted && total > 0).then_some(Self {
            total,
            last: u64::MAX,
        })
    }

    /// The percent to report for `at` frames done, or `None` if the whole
    /// percent has not moved since the last one.
    ///
    /// Separated from printing it so that a test can watch the sequence: what
    /// matters about this is that it starts at nothing, ends at everything,
    /// never goes backwards and never repeats itself, and none of that is
    /// visible from the outside once it has gone to standard error.
    fn stepped(&mut self, at: u64) -> Option<u64> {
        let percent = at.min(self.total) * 100 / self.total;
        (percent != self.last).then(|| {
            self.last = percent;
            percent
        })
    }

    /// Report `at` frames of `total` done, if that has moved the whole percent.
    pub fn at(&mut self, at: u64) {
        if let Some(percent) = self.stepped(at) {
            eprintln!("progress {percent}%");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off, or with nothing to be a fraction of, there is no reporter — a
    /// percentage of an unknown total is a number made up.
    #[test]
    fn there_is_nothing_to_report_without_a_total_or_a_flag() {
        assert!(Progress::new(false, 48_000).is_none());
        assert!(Progress::new(true, 0).is_none());
        assert!(Progress::new(true, 1).is_some());
    }

    /// A whole programme's worth of units yields at most a hundred and one
    /// lines, in order, without repeating — which is the whole reason it is a
    /// whole percent and not a frame count.
    #[test]
    fn a_run_reports_each_whole_percent_once_and_in_order() {
        // Two hours at 48 kHz, in forty-sample access units: 216 000 calls.
        let total = 48_000u64 * 3600 * 2;
        let mut progress = Progress::new(true, total).expect("a reporter");
        let mut seen = Vec::new();
        let mut at = 0u64;
        if let Some(percent) = progress.stepped(at) {
            seen.push(percent);
        }
        while at < total {
            at = (at + 40).min(total);
            if let Some(percent) = progress.stepped(at) {
                seen.push(percent);
            }
        }

        assert_eq!(seen.first(), Some(&0), "it starts at nothing");
        assert_eq!(seen.last(), Some(&100), "and ends at everything");
        assert_eq!(seen.len(), 101, "one line a percent, and no more");
        assert!(
            seen.windows(2).all(|pair| pair[1] > pair[0]),
            "never backwards, never twice"
        );
    }

    /// A short run reports fewer lines rather than the same hundred: eight
    /// frames cannot be twelve and a half per cent done.
    #[test]
    fn a_run_shorter_than_a_hundred_frames_reports_what_it_has() {
        let mut progress = Progress::new(true, 8).expect("a reporter");
        let seen: Vec<u64> = (0..=8).filter_map(|at| progress.stepped(at)).collect();
        assert_eq!(seen, vec![0, 12, 25, 37, 50, 62, 75, 87, 100]);
    }

    /// And a caller that overshoots its own total — a final unit padded past
    /// the end — does not get a hundred and four per cent.
    #[test]
    fn overshooting_the_total_still_ends_at_a_hundred() {
        let mut progress = Progress::new(true, 100).expect("a reporter");
        assert_eq!(progress.stepped(100), Some(100));
        assert_eq!(progress.stepped(140), None);
    }
}
