//! True peak, ITU-R BS.1770-4 Annex 2.
//!
//! The largest sample in a signal is not the largest value the signal reaches.
//! Between two samples the reconstructed waveform can go higher, and a
//! converter reconstructing it will — which is why a file that never touches
//! full scale sample-for-sample can still clip on playback. Measuring that
//! means reconstructing between the samples: oversample by four, take the
//! largest magnitude anywhere in the result.
//!
//! The recommendation specifies at least 4× for a 48 kHz signal and gives one
//! interpolator that satisfies it. It does not require *that* interpolator,
//! and no widely used implementation carries it — FFmpeg, for one, resamples
//! to 192 kHz with its general resampler. So the filter here is designed
//! rather than tabulated: a Blackman-Harris windowed sinc, 12 taps per phase.
//! Two implementations of "true peak" therefore agree to about a tenth of a
//! decibel rather than exactly, and that is a property of the measurement, not
//! of either one.
//!
//! Annex 2 also attenuates by 12.04 dB before interpolating and restores it
//! afterwards, so an interpolator working in fixed point cannot overflow. This
//! one works in `f64` and has nothing to overflow, so it does not bother.

/// Oversampling factor. Four is the recommendation's floor at 48 kHz.
const PHASES: usize = 4;
/// Taps per phase. Twelve puts the stop-band far enough down that the
/// measurement is limited by the window rather than by the length.
const TAPS_PER_PHASE: usize = 12;

/// Tracks the true peak of an interleaved signal, per channel.
#[derive(Debug, Clone)]
pub struct TruePeak {
    /// Phase-major and **reversed** within a phase, so that a phase's taps
    /// line up with the history below in the order it holds them.
    taps: [f64; PHASES * TAPS_PER_PHASE],
    /// Per channel, the last `TAPS_PER_PHASE` samples, **twice**.
    ///
    /// Every sample is written at `at` and again at `at + TAPS_PER_PHASE`, so
    /// the window of the last twelve is always one contiguous slice however
    /// far the ring has wrapped. The alternative is a modulo per tap per phase
    /// per channel per sample — forty-eight of them a sample — and this is the
    /// measurement that runs over every sample of a programme.
    history: Vec<f64>,
    channels: usize,
    at: usize,
    peaks: Vec<f64>,
}

impl TruePeak {
    pub fn new(channels: usize) -> Self {
        let designed = design();
        let mut taps = [0.0f64; PHASES * TAPS_PER_PHASE];
        for phase in 0..PHASES {
            for tap in 0..TAPS_PER_PHASE {
                // Reversed: the history holds its window oldest first and the
                // taps are stated newest first.
                taps[phase * TAPS_PER_PHASE + TAPS_PER_PHASE - 1 - tap] =
                    designed[phase * TAPS_PER_PHASE + tap];
            }
        }
        Self {
            taps,
            history: vec![0.0; channels * 2 * TAPS_PER_PHASE],
            channels,
            at: 0,
            peaks: vec![0.0; channels],
        }
    }

    /// Feed interleaved samples, normalised so full scale is ±1.
    pub fn push(&mut self, interleaved: &[f32]) {
        if self.channels == 0 {
            return;
        }
        for frame in interleaved.chunks_exact(self.channels) {
            for (channel, &sample) in frame.iter().enumerate() {
                let base = channel * 2 * TAPS_PER_PHASE;
                let value = f64::from(sample);
                self.history[base + self.at] = value;
                self.history[base + self.at + TAPS_PER_PHASE] = value;
            }
            self.at = (self.at + 1) % TAPS_PER_PHASE;

            for channel in 0..self.channels {
                let base = channel * 2 * TAPS_PER_PHASE;
                // `at` has advanced past the newest sample, so the twelve most
                // recent are `at` onwards, oldest first — contiguous because
                // every sample is in the buffer twice.
                let window = &self.history[base + self.at..base + self.at + TAPS_PER_PHASE];
                for phase in self.taps.chunks_exact(TAPS_PER_PHASE) {
                    let sum: f64 = window.iter().zip(phase).map(|(a, b)| a * b).sum();
                    let magnitude = sum.abs();
                    if magnitude > self.peaks[channel] {
                        self.peaks[channel] = magnitude;
                    }
                }
            }
        }
    }

    /// The largest true peak on any channel, as a linear magnitude.
    pub fn peak(&self) -> f64 {
        self.peaks.iter().copied().fold(0.0, f64::max)
    }

    /// The largest true peak on any channel, in dBTP.
    pub fn peak_db(&self) -> f64 {
        let peak = self.peak();
        if peak <= 0.0 {
            f64::NEG_INFINITY
        } else {
            20.0 * peak.log10()
        }
    }

    /// One channel's true peak, as a linear magnitude.
    pub fn channel_peak(&self, channel: usize) -> f64 {
        self.peaks.get(channel).copied().unwrap_or(0.0)
    }
}

/// A 4-phase interpolating low-pass: a sinc at the original Nyquist,
/// windowed, then split into phases.
///
/// The split is the part that is easy to get wrong. A polyphase decomposition
/// takes **every fourth** coefficient for each phase, not four contiguous
/// blocks of twelve; slicing it into blocks builds four unrelated filters that
/// still look like filters, and the only visible symptom is that a constant
/// signal no longer reads its own level.
fn design() -> [f64; PHASES * TAPS_PER_PHASE] {
    const LENGTH: usize = PHASES * TAPS_PER_PHASE;
    let mut prototype = [0.0; LENGTH];
    let centre = (LENGTH - 1) as f64 / 2.0;

    for (n, tap) in prototype.iter_mut().enumerate() {
        let x = (n as f64 - centre) / PHASES as f64;
        let sinc = if x.abs() < 1e-12 {
            1.0
        } else {
            let pix = std::f64::consts::PI * x;
            pix.sin() / pix
        };
        *tap = sinc * blackman_harris(n, LENGTH);
    }

    let mut taps = [0.0; LENGTH];
    for phase in 0..PHASES {
        for tap in 0..TAPS_PER_PHASE {
            taps[phase * TAPS_PER_PHASE + tap] = prototype[phase + tap * PHASES];
        }
        // Unity gain at DC per phase, so a constant reads its own value
        // instead of whatever the window happens to sum to.
        let sum: f64 = (0..TAPS_PER_PHASE)
            .map(|tap| taps[phase * TAPS_PER_PHASE + tap])
            .sum();
        if sum.abs() > 1e-12 {
            for tap in 0..TAPS_PER_PHASE {
                taps[phase * TAPS_PER_PHASE + tap] /= sum;
            }
        }
    }
    taps
}

fn blackman_harris(n: usize, length: usize) -> f64 {
    use std::f64::consts::TAU;
    let x = n as f64 / (length - 1) as f64;
    0.35875 - 0.48829 * (TAU * x).cos() + 0.14128 * (2.0 * TAU * x).cos()
        - 0.01168 * (3.0 * TAU * x).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measure(channels: usize, samples: &[f32]) -> TruePeak {
        let mut peak = TruePeak::new(channels);
        peak.push(samples);
        peak
    }

    /// A constant, once it has faded in, must read itself. If the phases are
    /// not normalised to unity at DC this is off by whatever the window sums
    /// to, and every later number inherits the error.
    #[test]
    fn a_constant_reads_its_own_level() {
        // A raised-cosine fade, because a linear one still has a corner
        // where it meets the constant, and a corner overshoots too.
        let mut samples: Vec<f32> = (0..480)
            .map(|n| {
                let x = n as f64 / 480.0;
                (0.5 * 0.5 * (1.0 - (std::f64::consts::PI * x).cos())) as f32
            })
            .collect();
        samples.extend(std::iter::repeat_n(0.5f32, 1000));

        let peak = measure(1, &samples);
        assert!((peak.peak() - 0.5).abs() < 1e-6, "{}", peak.peak());
    }

    /// A signal that starts at full level *is* a step, and a band-limited step
    /// overshoots — so the true peak of a file that opens loud is genuinely
    /// above its largest sample. Worth pinning down, because it looks like a
    /// bug the first time it is seen.
    #[test]
    fn a_step_at_the_start_overshoots_and_that_is_correct() {
        let peak = measure(1, &vec![0.5f32; 1000]);
        assert!(
            peak.peak() > 0.5,
            "a step should overshoot: {}",
            peak.peak()
        );
        assert!(peak.peak() < 0.6, "but not by this much: {}", peak.peak());
    }

    /// The point of the whole exercise. A sine at a quarter of the sample rate
    /// sampled at its zero crossings never shows more than 0.707 of its
    /// amplitude, and reconstruction reaches all of it.
    #[test]
    fn the_peak_between_samples_is_found() {
        let rate = 48_000.0;
        let hz = 12_000.0;
        let mut samples = Vec::new();
        for n in 0..4800 {
            let t = f64::from(n) / rate;
            // An eighth-cycle offset puts every sample off the crest.
            let phase = std::f64::consts::PI / 4.0;
            samples.push(((std::f64::consts::TAU * hz * t + phase).sin()) as f32);
        }

        // Sampled an eighth of a cycle off the crest, the largest sample is
        // cos(π/4) of the amplitude and no more.
        let sample_peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let off_crest = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (sample_peak - off_crest).abs() < 0.01,
            "sample peak {sample_peak}"
        );

        let peak = measure(1, &samples);
        assert!(
            peak.peak() > 0.97,
            "true peak {} should reach the crest",
            peak.peak()
        );
        assert!(peak.peak() < 1.05, "true peak {} overshoots", peak.peak());
    }

    /// Full scale sample-for-sample is at least full scale between samples.
    #[test]
    fn a_full_scale_signal_never_reads_below_full_scale() {
        let rate = 48_000.0;
        for hz in [100.0, 1000.0, 7000.0, 15_000.0] {
            let samples: Vec<f32> = (0..9600)
                .map(|n| {
                    let t = f64::from(n) / rate;
                    (std::f64::consts::TAU * hz * t).sin() as f32
                })
                .collect();
            let peak = measure(1, &samples);
            assert!(peak.peak() > 0.98, "{hz} Hz: {}", peak.peak());
        }
    }

    #[test]
    fn channels_are_measured_apart() {
        let samples: Vec<f32> = (0..2000)
            .flat_map(|n| {
                let loud = if n == 1000 { 0.9 } else { 0.1 };
                [loud, 0.05f32]
            })
            .collect();
        let peak = measure(2, &samples);
        assert!(peak.channel_peak(0) > 0.5);
        assert!(peak.channel_peak(1) < 0.2);
        assert!((peak.peak() - peak.channel_peak(0)).abs() < 1e-12);
    }

    #[test]
    fn silence_has_no_peak() {
        let peak = measure(2, &vec![0.0f32; 4000]);
        assert_eq!(peak.peak(), 0.0);
        assert!(peak.peak_db().is_infinite());
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    /// The doubled history and the reversed taps have to compute what the
    /// obvious loop computes.
    ///
    /// The obvious loop is written out here rather than referred to: it is a
    /// modulo per tap per phase per channel per sample, which is what the
    /// buffer exists to avoid, and the only way to know the avoidance is
    /// faithful is to run both. The sums are in opposite orders, so they agree
    /// to within rounding rather than exactly.
    #[test]
    fn the_contiguous_window_agrees_with_the_obvious_loop() {
        const CHANNELS: usize = 3;
        let designed = design();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut samples = Vec::new();
        for _ in 0..CHANNELS * 500 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            samples.push(((state >> 40) as i32 as f32) / 8_388_608.0);
        }

        let mut fast = TruePeak::new(CHANNELS);
        fast.push(&samples);

        // The same measurement, spelled the slow way.
        let mut history = vec![0.0f64; CHANNELS * TAPS_PER_PHASE];
        let mut peaks = [0.0f64; CHANNELS];
        let mut at = 0usize;
        for frame in samples.chunks_exact(CHANNELS) {
            for (channel, &sample) in frame.iter().enumerate() {
                history[channel * TAPS_PER_PHASE + at] = f64::from(sample);
            }
            at = (at + 1) % TAPS_PER_PHASE;
            for (channel, peak) in peaks.iter_mut().enumerate() {
                let base = channel * TAPS_PER_PHASE;
                for phase in 0..PHASES {
                    let mut sum = 0.0;
                    for tap in 0..TAPS_PER_PHASE {
                        let index = (at + TAPS_PER_PHASE - 1 - tap) % TAPS_PER_PHASE;
                        sum += history[base + index] * designed[phase * TAPS_PER_PHASE + tap];
                    }
                    *peak = peak.max(sum.abs());
                }
            }
        }

        for (channel, peak) in peaks.iter().enumerate() {
            assert!(
                (fast.channel_peak(channel) - peak).abs() < 1e-9,
                "channel {channel}: {} against {peak}",
                fast.channel_peak(channel),
            );
        }
    }
}
