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
/// Samples the meter's reconstruction runs behind what is pushed. The
/// prototype's centre is 5,875 samples back, so a push measures four points
/// between 5,875 and 5,125 samples before the newest: once this many more
/// have come, every point up to the next sample is measured, the eighth of a
/// sample on either side of a sample's own instant among them. A stretch
/// measured on its own ([`TruePeak::take`]) has to run this far into the
/// next before it closes; a sample short, a transient on its last sample
/// leaves half its peak to the next.
pub const LATENCY: usize = TAPS_PER_PHASE / 2;

/// Samples of a channel a sample's reconstruction reads from before it.
const KEEP: usize = TAPS_PER_PHASE - 1;

/// Samples of a channel reconstructed at once, a lane each.
const LANES: usize = 8;

/// Tracks the true peak of an interleaved signal, per channel.
#[derive(Debug, Clone)]
pub struct TruePeak {
    /// Phase-major and **reversed** within a phase, so that a phase's taps
    /// line up with a window of the signal oldest first.
    taps: [f64; PHASES * TAPS_PER_PHASE],
    /// Per channel, its last [`KEEP`] samples, oldest first: all a sample
    /// still to come needs of what came before it. Noughts before the first.
    tails: Vec<f64>,
    channels: usize,
    peaks: Vec<f64>,
    /// One channel of a push, after its tail: kept, so that a push allocates
    /// nothing once one as long has been made.
    line: Vec<f64>,
}

impl TruePeak {
    pub fn new(channels: usize) -> Self {
        let designed = design();
        let mut taps = [0.0f64; PHASES * TAPS_PER_PHASE];
        for phase in 0..PHASES {
            for tap in 0..TAPS_PER_PHASE {
                // Reversed: a window is oldest first and the taps are stated
                // newest first.
                taps[phase * TAPS_PER_PHASE + TAPS_PER_PHASE - 1 - tap] =
                    designed[phase * TAPS_PER_PHASE + tap];
            }
        }
        Self {
            taps,
            tails: vec![0.0; channels * KEEP],
            channels,
            peaks: vec![0.0; channels],
            line: Vec::new(),
        }
    }

    /// Feed interleaved samples, normalised so full scale is ±1. A frame cut
    /// short at the end is not measured.
    ///
    /// Each point is the sum, over the twelve samples up to the one it
    /// follows, oldest first, of sample times tap, the products added in that
    /// order to what [`Iterator::sum`] starts from. A channel's samples are
    /// reconstructed [`LANES`] at a time, each its own sum in that order, so
    /// every point is what reconstructing them one at a time makes of it, to
    /// the bit; and the largest magnitude of them is the largest whichever
    /// order they are compared in. NaN is never one, as before.
    pub fn push(&mut self, interleaved: &[f32]) {
        if self.channels == 0 {
            return;
        }
        let frames = interleaved.len() / self.channels;
        if frames == 0 {
            return;
        }
        self.line.resize(KEEP + frames, 0.0);
        let line = &mut self.line[..KEEP + frames];
        let tails = self.tails.chunks_exact_mut(KEEP);
        for (channel, (tail, peak)) in tails.zip(&mut self.peaks).enumerate() {
            line[..KEEP].copy_from_slice(tail);
            for (x, frame) in line[KEEP..]
                .iter_mut()
                .zip(interleaved.chunks_exact(self.channels))
            {
                *x = f64::from(frame[channel]);
            }
            *peak = loudest(&self.taps, line, *peak);
            tail.copy_from_slice(&line[frames..]);
        }
    }

    /// The largest true peak on any channel since the last take, as a linear
    /// magnitude, and the peaks started again — the interpolator's history
    /// kept, so that the next stretch's first samples are reconstructed as
    /// they would have been.
    pub fn take(&mut self) -> f64 {
        let peak = self.peak();
        self.peaks.fill(0.0);
        peak
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

/// The largest of `peak` and the magnitude of every phase's point after
/// each sample of `line` but its first [`KEEP`], which are the ones before.
fn loudest(taps: &[f64; PHASES * TAPS_PER_PHASE], line: &[f64], peak: f64) -> f64 {
    let points = line.len() - KEEP;
    let nought: f64 = std::iter::empty::<f64>().sum();
    let whole = points - points % LANES;
    // Each lane's largest, the peak so far among them.
    let mut lanes = [peak; LANES];
    for n in (0..whole).step_by(LANES) {
        for phase in taps.chunks_exact(TAPS_PER_PHASE) {
            let mut sums = [nought; LANES];
            for (k, &tap) in phase.iter().enumerate() {
                let samples: &[f64; LANES] = line[n + k..n + k + LANES]
                    .try_into()
                    .expect("a lane a sample");
                for (sum, &sample) in sums.iter_mut().zip(samples) {
                    *sum += sample * tap;
                }
            }
            for (largest, sum) in lanes.iter_mut().zip(sums) {
                let magnitude = sum.abs();
                if magnitude > *largest {
                    *largest = magnitude;
                }
            }
        }
    }
    let mut peak = lanes
        .into_iter()
        .fold(peak, |peak, lane| if lane > peak { lane } else { peak });
    // The samples a whole set of lanes leaves, one at a time.
    for n in whole..points {
        let window = &line[n..n + TAPS_PER_PHASE];
        for phase in taps.chunks_exact(TAPS_PER_PHASE) {
            let sum: f64 = window.iter().zip(phase).map(|(a, b)| a * b).sum();
            let magnitude = sum.abs();
            if magnitude > peak {
                peak = magnitude;
            }
        }
    }
    peak
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

    /// A sample's reconstruction is measured whole [`LATENCY`] samples after
    /// it, and what is measured after that is only the ringing: an impulse,
    /// [`LATENCY`] samples after it, a take, and the next stretch well under
    /// it. A sample sooner, the next stretch has the other side of its peak.
    #[test]
    fn a_sample_is_measured_whole_latency_samples_after_it() {
        let measure = |after: usize| {
            let mut meter = TruePeak::new(1);
            let at = 20;
            for n in 0..=at + after {
                meter.push(&[if n == at { 1.0 } else { 0.0 }]);
            }
            let peak = meter.take();
            for _ in 0..2 * TAPS_PER_PHASE {
                meter.push(&[0.0]);
            }
            (peak, meter.peak())
        };
        let (peak, rest) = measure(LATENCY);
        assert!(peak > 0.9, "its own stretch: {peak}");
        assert!(rest < 0.2, "the next: {rest}");
        let (_, rest) = measure(LATENCY - 1);
        assert!(rest > 0.9, "a sample sooner, the next: {rest}");
    }

    /// A take hands back the peak of the stretch since the last, and starts
    /// again: a loud stretch, then a quiet one, each measured on its own.
    #[test]
    fn a_take_measures_the_stretch_since_the_last() {
        let tone = |amplitude: f32| -> Vec<f32> {
            // Faded in and out, so that no edge rings past the tone.
            (0..4800)
                .map(|n| {
                    let fade = (std::f32::consts::PI * n as f32 / 4800.0).sin().powi(2);
                    amplitude * fade * (n as f32 * 0.05).sin()
                })
                .collect()
        };
        let mut meter = TruePeak::new(1);
        meter.push(&tone(0.9));
        let loud = meter.take();
        meter.push(&tone(0.1));
        let quiet = meter.take();
        assert!((loud - 0.9).abs() < 0.018, "{loud}");
        assert!((quiet - 0.1).abs() < 0.002, "{quiet}");
        assert_eq!(meter.peak(), 0.0);
    }

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

#[cfg(test)]
mod before_tests {
    use super::*;

    /// The meter as it was before its samples were reconstructed a set of
    /// lanes at a time, kept word for word: one sample after another, each
    /// channel's last twelve in a doubled ring.
    struct Before {
        taps: [f64; PHASES * TAPS_PER_PHASE],
        history: Vec<f64>,
        channels: usize,
        at: usize,
        peaks: Vec<f64>,
    }

    impl Before {
        fn new(channels: usize) -> Self {
            let designed = design();
            let mut taps = [0.0f64; PHASES * TAPS_PER_PHASE];
            for phase in 0..PHASES {
                for tap in 0..TAPS_PER_PHASE {
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

        fn push(&mut self, interleaved: &[f32]) {
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

        fn take(&mut self) -> f64 {
            let peak = self.peaks.iter().copied().fold(0.0, f64::max);
            self.peaks.fill(0.0);
            peak
        }
    }

    /// A sample of noise, at full scale or far under it, a square or a
    /// tone; and with `odd`, now and then one of the values a meter has to
    /// get through: noughts of either sign, subnormals, the largest and
    /// infinite, NaN.
    fn sample(state: &mut u64, n: usize, odd: bool) -> f32 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        let noise = ((*state >> 40) as i32 as f32) / 8_388_608.0 - 1.0;
        let pick = if odd { (*state >> 20) % 400 } else { 400 };
        match pick {
            0 => -0.0,
            1 => 0.0,
            2 => f32::from_bits(1 + (*state as u32 & 0x7f_ffff)),
            3 => -f32::MIN_POSITIVE / 3.0,
            4 => f32::MAX,
            5 => -f32::MAX,
            6 => f32::INFINITY,
            7 => f32::NEG_INFINITY,
            8 => f32::NAN,
            _ => match n / 997 % 4 {
                0 => noise,
                1 => noise * 1e-30,
                2 => noise.signum(),
                _ => (n as f32 * 0.37).sin() * 0.999,
            },
        }
    }

    /// Reconstructed a set of lanes at a time, the meter reads what it read
    /// one sample after another, to the bit: every channel's peak after
    /// every push, and every take — one channel to fourteen, pushes of no
    /// frame, one, part of a frame, a set of lanes and some, thousands; a
    /// take now and then; and samples that are noise, quiet, square or a
    /// tone, and on every third channel count now and then a value no
    /// signal has. (Once a channel has met an infinity its peak stays there,
    /// so the other counts carry the comparison.)
    #[test]
    fn the_meter_reads_what_it_read_one_sample_after_another_to_the_bit() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for channels in 1..=14 {
            let mut meter = TruePeak::new(channels);
            let mut before = Before::new(channels);
            let mut n = 0;
            for push in 0..60 {
                let frames = match push % 7 {
                    0 => 0,
                    1 => 1,
                    2 => LANES - 1,
                    3 => LANES + 3,
                    4 => KEEP - 2,
                    5 => 4096,
                    _ => (state >> 33) as usize % 2000,
                };
                // Now and then a frame cut short, which neither measures.
                let cut = usize::from(push % 5 == 0 && channels > 1);
                let samples: Vec<f32> = (0..frames * channels + cut)
                    .map(|i| sample(&mut state, n + i / channels, channels % 3 == 0))
                    .collect();
                n += frames;
                meter.push(&samples);
                before.push(&samples);
                for channel in 0..channels {
                    assert_eq!(
                        meter.channel_peak(channel).to_bits(),
                        before.peaks[channel].to_bits(),
                        "{channels} channels, push {push}, channel {channel}"
                    );
                }
                if push % 9 == 4 {
                    assert_eq!(
                        meter.take().to_bits(),
                        before.take().to_bits(),
                        "{channels} channels, take after push {push}"
                    );
                }
            }
        }
    }
}
