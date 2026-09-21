//! Loudness to ITU-R BS.1770-4, and loudness range to EBU Tech 3342.
//!
//! # What the numbers are
//!
//! A block's loudness is `-0.691 + 10·log₁₀(Σ Gᵢ·zᵢ)`, where `zᵢ` is the mean
//! square of channel `i` after K-weighting and `Gᵢ` is its weight: one for the
//! front and centre channels, 1.41 for the surrounds, and zero for an LFE,
//! which is not measured at all.
//!
//! *Momentary* is that over 400 ms, *short-term* over 3 s, and *integrated* is
//! the mean energy of the 400 ms blocks that survive two gates — an absolute
//! one at −70 LUFS, and a relative one 10 LU below the mean of what the
//! absolute gate left. The relative gate is why integrated loudness cannot be
//! computed as it goes: it needs every block before it can decide which blocks
//! count.
//!
//! # What it measures, and what it does not
//!
//! BS.1770 measures a **channel-based presentation**. Pointing it at an
//! object-based master and weighting every object by one is not the
//! recommendation and is not done here: measuring a master means rendering a
//! presentation first and measuring that. This module is the second half.
//!
//! Checked against FFmpeg's `ebur128` on tones, pink noise, a 5.1 layout with
//! its LFE excluded and its surrounds weighted, and material that swings by
//! 20 LU: integrated loudness agrees to the printed resolution in every case.
//! See `docs/loudness.md`.

use crate::kweighting::{KWeighting, State};

/// Sub-blocks per second. The 400 ms block and its 75 % overlap both fall out
/// of a 100 ms grid, so everything is accumulated on that grid once and summed
/// into windows afterwards rather than filtered repeatedly.
const SUBBLOCKS_PER_SECOND: usize = 10;
const MOMENTARY_SUBBLOCKS: usize = 4;
const SHORT_TERM_SUBBLOCKS: usize = 30;

/// The offset that makes a reference signal read its nominal level.
const OFFSET_DB: f64 = -0.691;
/// BS.1770-4's absolute gate.
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
/// How far below the ungated mean the relative gate sits.
const RELATIVE_GATE_LU: f64 = -10.0;
/// Tech 3342's relative gate, which is looser than the loudness one.
const RANGE_GATE_LU: f64 = -20.0;

/// Silence, as a loudness.
///
/// Negative infinity, which is what silence *is* on a logarithmic scale. It
/// was briefly a large finite number so that a caller printing a column could
/// align it; that made "no measurement yet" and "very quiet" the same value,
/// and a caller that wants a column can format an infinity however it likes.
pub const SILENCE_LUFS: f64 = f64::NEG_INFINITY;

/// Samples in one 100 ms sub-block.
///
/// Public because anything that wants to say something *about* a sub-block —
/// the speech gate does — has to land on the same grid, and deriving it twice
/// is how two grids drift apart.
pub fn subblock_frames(sample_rate: u32) -> usize {
    (sample_rate as usize).div_ceil(SUBBLOCKS_PER_SECOND)
}

/// A channel's weight in the sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelWeight {
    /// Front and centre channels, and anything treated like one.
    Unity,
    /// Surrounds, weighted 1.41 — about +1.5 dB.
    Surround,
    /// An LFE. Not measured, and not merely quiet: excluded.
    Excluded,
}

impl ChannelWeight {
    fn factor(self) -> f64 {
        match self {
            Self::Unity => 1.0,
            Self::Surround => 1.41,
            Self::Excluded => 0.0,
        }
    }

    /// The weight BS.1770 gives a channel with this speaker label.
    ///
    /// Takes the ADM common-definition labels, since that is what both the
    /// master format and ADM resolve to here.
    pub fn for_speaker_label(label: &str) -> Self {
        match label {
            "LFE" | "LFE1" | "LFE2" | "LFEL" | "LFER" => Self::Excluded,
            // BS.1770 names the weight for a 5.1's left and right surround and
            // says nothing about the layouts that came after it. Two of those
            // labels have to be decided rather than looked up, and both are
            // decided the same way: by which side of the listener the speaker
            // is on, which is what the weight is *for* — it compensates for a
            // sound arriving from behind being heard as quieter than the same
            // sound in front.
            //
            // `M+090` (a 7.1 side surround) and `M±135` (its rear) are
            // therefore surrounds, like the `M±110` the recommendation names.
            "M+090" | "M-090" | "M+110" | "M-110" | "M+135" | "M-135" => Self::Surround,
            // And `M±060` — a wide front channel — is not. It is forward of
            // the listener and inside the front stage, so it is weighted at
            // unity like the rest of it. Stated rather than left to the
            // fallback below, because it is the one front label that looks
            // like it might not be.
            "M+060" | "M-060" => Self::Unity,
            // Everything else: the front stage, the heights, and any label
            // this does not know. A weight of one is the recommendation's own
            // default and the only safe guess — over-weighting a channel
            // reports a mix as louder than it is.
            _ => Self::Unity,
        }
    }
}

/// A streaming BS.1770 meter.
///
/// Everything it needs is allocated when it is built or grows once per block,
/// so pushing samples costs no allocation: it runs over whole programmes.
#[derive(Debug, Clone)]
pub struct Meter {
    sample_rate: f64,
    weights: Vec<f64>,
    filter: KWeighting,
    states: Vec<(State, State)>,

    /// Samples per 100 ms sub-block.
    subblock_frames: usize,
    /// Frames accumulated into the sub-block being filled.
    frames_in_subblock: usize,
    /// Running sum of squares per channel for the sub-block being filled.
    sums: Vec<f64>,

    /// Per-channel sub-block energies, most recent last, kept as a ring big
    /// enough for the longest window.
    history: Vec<f64>,
    history_len: usize,
    history_at: usize,
    /// Sub-blocks completed. A window is only measured once this many have
    /// gone by — averaging over a ring that is not full yet reads low, and
    /// the first blocks of a programme are exactly the ones a gate keeps.
    subblocks_seen: usize,

    /// Weighted energy of every completed 400 ms block, for the gates.
    blocks: Vec<f64>,
    /// Weighted energy of every completed 3 s window, for the range.
    windows: Vec<f64>,

    momentary: f64,
    short_term: f64,
}

impl Meter {
    /// Build a meter for a layout, one weight per channel in channel order.
    pub fn new(sample_rate: u32, weights: &[ChannelWeight]) -> Self {
        let channels = weights.len().max(1);
        let subblock_frames = subblock_frames(sample_rate);
        let history_len = SHORT_TERM_SUBBLOCKS;

        Self {
            sample_rate: f64::from(sample_rate),
            weights: weights.iter().map(|w| w.factor()).collect(),
            filter: KWeighting::new(f64::from(sample_rate)),
            states: vec![(State::default(), State::default()); channels],
            subblock_frames,
            frames_in_subblock: 0,
            sums: vec![0.0; channels],
            history: vec![0.0; history_len * channels],
            history_len,
            history_at: 0,
            subblocks_seen: 0,
            blocks: Vec::new(),
            windows: Vec::new(),
            momentary: SILENCE_LUFS,
            short_term: SILENCE_LUFS,
        }
    }

    pub fn channels(&self) -> usize {
        self.weights.len()
    }

    /// Feed interleaved samples, normalised so full scale is ±1.
    pub fn push(&mut self, interleaved: &[f32]) {
        let channels = self.weights.len();
        if channels == 0 {
            return;
        }

        for frame in interleaved.chunks_exact(channels) {
            // Zipped rather than indexed: three vectors of the same length
            // walked together, which is what they are, and no bounds check a
            // sample.
            let walk = frame
                .iter()
                .zip(&self.weights)
                .zip(self.states.iter_mut())
                .zip(self.sums.iter_mut());
            for (((sample, weight), state), sum) in walk {
                // An excluded channel is not filtered either: it contributes
                // nothing, and running it would only cost time.
                if *weight == 0.0 {
                    continue;
                }
                let y = self.filter.step(state, f64::from(*sample));
                *sum += y * y;
            }

            self.frames_in_subblock += 1;
            if self.frames_in_subblock == self.subblock_frames {
                self.close_subblock();
            }
        }
    }

    /// Finish the sub-block in progress, so a final partial one is not lost.
    ///
    /// Idempotent, because a caller that flushes and then asks for every
    /// figure should not get a different answer than one that asks twice.
    pub fn flush(&mut self) {
        if self.frames_in_subblock > 0 {
            self.close_subblock();
        }
    }

    fn close_subblock(&mut self) {
        let channels = self.weights.len();
        let frames = self.frames_in_subblock.max(1) as f64;

        for channel in 0..channels {
            self.history[self.history_at * channels + channel] = self.sums[channel] / frames;
            self.sums[channel] = 0.0;
        }
        self.history_at = (self.history_at + 1) % self.history_len;
        self.frames_in_subblock = 0;
        self.subblocks_seen += 1;

        if self.subblocks_seen >= MOMENTARY_SUBBLOCKS {
            let energy = self.window_energy(MOMENTARY_SUBBLOCKS);
            self.momentary = loudness_of(energy);
            self.blocks.push(energy);
        }
        if self.subblocks_seen >= SHORT_TERM_SUBBLOCKS {
            let energy = self.window_energy(SHORT_TERM_SUBBLOCKS);
            self.short_term = loudness_of(energy);
            self.windows.push(energy);
        }
    }

    /// The weighted energy of the last `count` sub-blocks.
    fn window_energy(&self, count: usize) -> f64 {
        let channels = self.weights.len();
        let mut total = 0.0;
        for step in 0..count {
            // `history_at` already points past the newest entry.
            let index = (self.history_at + self.history_len - 1 - step) % self.history_len;
            for channel in 0..channels {
                total += self.weights[channel] * self.history[index * channels + channel];
            }
        }
        total / count as f64
    }

    /// Loudness over the last 400 ms.
    pub fn momentary(&self) -> f64 {
        self.momentary
    }

    /// Loudness over the last 3 s.
    pub fn short_term(&self) -> f64 {
        self.short_term
    }

    /// The weighted energy of every completed 400 ms block, oldest first.
    ///
    /// Block `i` covers sub-blocks `i..i+4`, so it starts at sample
    /// `i * subblock_frames` and runs for four of them. That mapping is the
    /// contract a gate needs in order to say which blocks it kept.
    pub fn blocks(&self) -> &[f64] {
        &self.blocks
    }

    /// Gated loudness over everything pushed so far.
    ///
    /// Needs every block, because the relative gate is defined against their
    /// mean — which is why this is not a running figure.
    pub fn integrated(&self) -> f64 {
        self.integrated_over(|_| true)
    }

    /// Gated loudness over a chosen subset of the blocks.
    ///
    /// `keep` is asked about each block by index into [`Self::blocks`]. Both
    /// BS.1770 gates still apply, and the relative one is computed over the
    /// subset — a dialogue measurement's relative gate is relative to the
    /// dialogue, not to the programme around it.
    pub fn integrated_over(&self, keep: impl Fn(usize) -> bool) -> f64 {
        let above_absolute = |(index, energy): &(usize, &f64)| {
            keep(*index) && loudness_of(**energy) > ABSOLUTE_GATE_LUFS
        };

        let ungated: Vec<(usize, &f64)> = self
            .blocks
            .iter()
            .enumerate()
            .filter(above_absolute)
            .collect();
        if ungated.is_empty() {
            return SILENCE_LUFS;
        }

        let mean = ungated.iter().map(|(_, e)| **e).sum::<f64>() / ungated.len() as f64;
        let relative = loudness_of(mean) + RELATIVE_GATE_LU;

        let (sum, count) = ungated
            .into_iter()
            .filter(|(_, energy)| loudness_of(**energy) > relative)
            .fold((0.0, 0usize), |(sum, count), (_, energy)| {
                (sum + energy, count + 1)
            });

        if count == 0 {
            SILENCE_LUFS
        } else {
            loudness_of(sum / count as f64)
        }
    }

    /// Loudness range, EBU Tech 3342: the spread between the 10th and 95th
    /// percentiles of the gated short-term loudness.
    pub fn loudness_range(&self) -> f64 {
        let mut values: Vec<f64> = self
            .windows
            .iter()
            .map(|energy| loudness_of(*energy))
            .filter(|l| *l > ABSOLUTE_GATE_LUFS)
            .collect();
        if values.is_empty() {
            return 0.0;
        }

        let mean_energy = self
            .windows
            .iter()
            .filter(|energy| loudness_of(**energy) > ABSOLUTE_GATE_LUFS)
            .sum::<f64>()
            / values.len() as f64;
        let gate = loudness_of(mean_energy) + RANGE_GATE_LU;

        values.retain(|l| *l > gate);
        if values.is_empty() {
            return 0.0;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        percentile(&values, 0.10)
            .map(|low| percentile(&values, 0.95).unwrap_or(low) - low)
            .unwrap_or(0.0)
            .max(0.0)
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}

/// A block's loudness from its weighted energy.
fn loudness_of(energy: f64) -> f64 {
    if energy <= 0.0 {
        SILENCE_LUFS
    } else {
        OFFSET_DB + 10.0 * energy.log10()
    }
}

/// Tech 3342's percentile: the value at that position in the sorted list.
fn percentile(sorted: &[f64], fraction: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, seconds: f64, amplitude: f64, channels: usize) -> Vec<f32> {
        let frames = (f64::from(rate) * seconds) as usize;
        let mut out = Vec::with_capacity(frames * channels);
        for n in 0..frames {
            let t = n as f64 / f64::from(rate);
            let value = (std::f64::consts::TAU * hz * t).sin() * amplitude;
            for _ in 0..channels {
                out.push(value as f32);
            }
        }
        out
    }

    fn measure(rate: u32, weights: &[ChannelWeight], samples: &[f32]) -> Meter {
        let mut meter = Meter::new(rate, weights);
        meter.push(samples);
        meter.flush();
        meter
    }

    /// The calibration the whole scale rests on. For a steady tone the answer
    /// is a closed form — `-0.691 + 10log₁₀(Σ G·k²·A²/2)` — so this checks the
    /// gating and summing against the filter rather than against a memory of
    /// what the number should be.
    #[test]
    fn a_steady_tone_reads_its_closed_form() {
        let rate = 48_000;
        let amplitude = 0.5;
        let weights = [ChannelWeight::Unity, ChannelWeight::Unity];
        let meter = measure(rate, &weights, &sine(rate, 1000.0, 10.0, amplitude, 2));

        let k = KWeighting::new(f64::from(rate)).magnitude(1000.0, f64::from(rate));
        let per_channel = k * k * amplitude * amplitude / 2.0;
        let expected = OFFSET_DB + 10.0 * (2.0 * per_channel).log10();

        // Tight on purpose: anything looser would hide a window that averages
        // over sub-blocks it has not seen yet, which reads low by a few dB on
        // exactly the blocks a gate keeps.
        assert!(
            (meter.integrated() - expected).abs() < 0.01,
            "integrated {} vs {expected}",
            meter.integrated()
        );
        assert!((meter.momentary() - expected).abs() < 0.01);
        assert!((meter.short_term() - expected).abs() < 0.01);
    }

    /// Doubling amplitude is +6.02 dB, whatever else the meter does.
    #[test]
    fn level_moves_the_reading_by_exactly_what_it_should() {
        let rate = 48_000;
        let weights = [ChannelWeight::Unity, ChannelWeight::Unity];
        let quiet = measure(rate, &weights, &sine(rate, 1000.0, 6.0, 0.25, 2));
        let loud = measure(rate, &weights, &sine(rate, 1000.0, 6.0, 0.5, 2));
        assert!(
            (loud.integrated() - quiet.integrated() - 6.0206).abs() < 0.01,
            "{} vs {}",
            loud.integrated(),
            quiet.integrated()
        );
    }

    /// An LFE is excluded, not merely quiet: adding one at full tilt must not
    /// move the reading at all.
    #[test]
    fn an_lfe_does_not_count() {
        let rate = 48_000;
        let stereo = measure(
            rate,
            &[ChannelWeight::Unity, ChannelWeight::Unity],
            &sine(rate, 1000.0, 6.0, 0.5, 2),
        );

        let with_lfe = measure(
            rate,
            &[
                ChannelWeight::Unity,
                ChannelWeight::Unity,
                ChannelWeight::Excluded,
            ],
            &sine(rate, 1000.0, 6.0, 0.5, 3),
        );

        assert!(
            (with_lfe.integrated() - stereo.integrated()).abs() < 1e-6,
            "{} vs {}",
            with_lfe.integrated(),
            stereo.integrated()
        );
    }

    /// A surround is weighted 1.41, which is +1.49 dB on its own contribution.
    #[test]
    fn a_surround_carries_its_extra_weight() {
        let rate = 48_000;
        let front = measure(
            rate,
            &[ChannelWeight::Unity],
            &sine(rate, 1000.0, 6.0, 0.5, 1),
        );
        let surround = measure(
            rate,
            &[ChannelWeight::Surround],
            &sine(rate, 1000.0, 6.0, 0.5, 1),
        );
        let expected = 10.0 * 1.41f64.log10();
        assert!(
            (surround.integrated() - front.integrated() - expected).abs() < 0.01,
            "{} vs {}",
            surround.integrated(),
            front.integrated()
        );
    }

    /// The relative gate is the whole reason integrated loudness is not a
    /// running average: twenty seconds of silence after ten of tone must not
    /// drag the answer down.
    #[test]
    fn silence_is_gated_out_rather_than_averaged_in() {
        let rate = 48_000;
        let weights = [ChannelWeight::Unity, ChannelWeight::Unity];
        let tone = sine(rate, 1000.0, 10.0, 0.5, 2);
        let mut with_silence = tone.clone();
        with_silence.extend(std::iter::repeat_n(0.0f32, rate as usize * 20 * 2));

        let alone = measure(rate, &weights, &tone);
        let padded = measure(rate, &weights, &with_silence);

        assert!(
            (padded.integrated() - alone.integrated()).abs() < 0.1,
            "{} vs {}",
            padded.integrated(),
            alone.integrated()
        );
    }

    #[test]
    fn silence_alone_is_silence_and_not_a_number() {
        let rate = 48_000;
        let meter = measure(
            rate,
            &[ChannelWeight::Unity],
            &vec![0.0f32; rate as usize * 2],
        );
        assert!(meter.integrated().is_infinite());
        assert_eq!(meter.loudness_range(), 0.0);
    }

    /// A steady tone has no range. Anything else means the short-term windows
    /// are drifting when they should not.
    #[test]
    fn a_steady_tone_has_no_range() {
        let rate = 48_000;
        let meter = measure(
            rate,
            &[ChannelWeight::Unity, ChannelWeight::Unity],
            &sine(rate, 1000.0, 20.0, 0.5, 2),
        );
        assert!(meter.loudness_range() < 0.2, "{}", meter.loudness_range());
    }

    /// Flushing twice must not change anything, or a caller that asks for two
    /// figures gets two different answers.
    #[test]
    fn flushing_is_idempotent() {
        let rate = 48_000;
        let mut meter = Meter::new(rate, &[ChannelWeight::Unity]);
        meter.push(&sine(rate, 1000.0, 5.0, 0.5, 1));
        meter.flush();
        let once = meter.integrated();
        meter.flush();
        assert_eq!(meter.integrated(), once);
    }

    #[test]
    fn speaker_labels_map_to_the_weights_the_recommendation_gives() {
        assert_eq!(
            ChannelWeight::for_speaker_label("LFE1"),
            ChannelWeight::Excluded
        );
        assert_eq!(
            ChannelWeight::for_speaker_label("M+110"),
            ChannelWeight::Surround
        );
        assert_eq!(
            ChannelWeight::for_speaker_label("M+030"),
            ChannelWeight::Unity
        );
        assert_eq!(
            ChannelWeight::for_speaker_label("U+030"),
            ChannelWeight::Unity
        );
    }
}

#[cfg(test)]
mod weight_tests {
    use super::*;

    /// The two labels the recommendation does not name are decided here, so
    /// they are asserted here: a wide front channel is front, and a 7.1's side
    /// and rear surrounds are surrounds.
    #[test]
    fn the_labels_bs_1770_does_not_name_are_decided_rather_than_defaulted() {
        for label in ["M+060", "M-060", "M+030", "M-030", "M+000", "U+030"] {
            assert_eq!(
                ChannelWeight::for_speaker_label(label).factor(),
                1.0,
                "{label} is forward of the listener and weighs one"
            );
        }
        for label in ["M+090", "M-090", "M+110", "M-110", "M+135", "M-135"] {
            assert_eq!(
                ChannelWeight::for_speaker_label(label).factor(),
                1.41,
                "{label} is behind the listener and carries the surround weight"
            );
        }
        for label in ["LFE", "LFE1", "LFE2", "LFEL", "LFER"] {
            assert_eq!(
                ChannelWeight::for_speaker_label(label).factor(),
                0.0,
                "{label} is excluded from the sum"
            );
        }
    }
}
