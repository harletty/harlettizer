//! A speech gate, for measuring the level of dialogue rather than of a whole
//! programme.
//!
//! # Why a gate at all
//!
//! `dialnorm` tells a decoder how loud the *dialogue* in a programme is, so
//! that playback can normalise to it. Measuring the whole programme instead
//! answers a different question: a programme whose dialogue sits at −31 LKFS
//! and whose action sits at −18 measures around −20 overall, and a decoder
//! told −20 will play the dialogue too quietly. So the loudness that feeds
//! `dialnorm` is measured over the parts of the programme that contain speech,
//! and everything else is left out of the average.
//!
//! # What this is, and what it is not
//!
//! The reference encoder's speech gate is proprietary and undocumented. This
//! is not a reimplementation of it — it cannot be, nothing about it is
//! published — it is an open substitute, built from features that the
//! speech/music discrimination literature has used for thirty years, whose
//! disagreement with the reference is a number to be **measured and
//! published** rather than assumed. See `docs/speech.md`.
//!
//! It is a *dialogue presence* detector, not a speech/music classifier. Its
//! job is to find the frames where a voice is speaking, on a signal that is
//! usually a centre channel and therefore already mostly dialogue. Sung vocals
//! and rhythmic music are its known failure mode and it does not pretend
//! otherwise.
//!
//! # How it decides
//!
//! Every 10 ms it looks at the last 80 ms and asks four questions:
//!
//! - **Is anything there?** The frame level has to stand above an adaptive
//!   noise floor, so that room tone and dither do not count as speech.
//! - **Is the energy where speech lives?** The fraction of energy between
//!   300 Hz and 3.4 kHz — the band a telephone keeps because it is the band
//!   speech needs.
//! - **Is it voiced?** Cepstral peak prominence: how far the cepstrum's peak
//!   at a plausible pitch period stands above the trend around it. This is the
//!   standard measure of voice periodicity, and it is what separates a voice
//!   from noise that happens to be band-limited.
//! - **Does it move like speech?** The energy envelope of speech is modulated
//!   at the syllabic rate, around 4 Hz. A held note or a steady noise is not.
//!
//! A frame that passes all four is *voice-like*. Voice-like frames are then
//! smoothed in time — a run has to start before the gate opens, and the gate
//! stays open across the gaps between words — and the result is reduced to one
//! flag per 100 ms, which is the grid BS.1770 already accumulates on.

use crate::fft::Fft;
use crate::loudness::subblock_frames;

/// Frames per 100 ms sub-block. Ten gives a 10 ms hop at any sample rate and
/// keeps the two grids aligned by construction rather than by arithmetic.
const FRAMES_PER_SUBBLOCK: usize = 10;
/// Analysis window, in hops. Eight is 80 ms, which holds nearly five periods
/// of the lowest pitch this looks for — [`F0_MIN_HZ`], whose period is 16.7 ms
/// — and the cepstral peak needs several of them to stand above the trend.
const HOPS_PER_WINDOW: usize = 8;

/// The lowest and highest pitch treated as a voice, in hertz.
const F0_MIN_HZ: f64 = 60.0;
const F0_MAX_HZ: f64 = 400.0;

/// The band speech energy is measured in.
const SPEECH_LOW_HZ: f64 = 300.0;
const SPEECH_HIGH_HZ: f64 = 3400.0;
/// The band it is measured *against*. Not the whole spectrum: rumble below
/// 60 Hz and content above 8 kHz say nothing about whether a voice is present,
/// and including them would make the ratio a property of the mix.
const FULL_LOW_HZ: f64 = 60.0;
const FULL_HIGH_HZ: f64 = 8000.0;

/// Syllabic rate: the envelope modulation frequency speech is built around.
const SYLLABIC_HZ: f64 = 4.0;
const SYLLABIC_Q: f64 = 1.5;

/// Below this the frame is silence as far as any of this is concerned.
const ABSOLUTE_FLOOR_DB: f64 = -80.0;

/// The noise floor is read off a histogram of frame levels, one bin per
/// decibel from [`FLOOR_LOW_DB`] up to 0 dBFS.
const FLOOR_LOW_DB: f64 = -120.0;
const FLOOR_BINS: usize = 121;
/// Which percentile of the window's levels the floor is taken to be.
const FLOOR_PERCENTILE: f64 = 0.10;

/// The cepstrum is taken over a fixed grid of this many points spanning
/// 0 Hz to [`CEPSTRUM_TOP_HZ`], whatever the sample rate.
///
/// Two reasons, and the first is the one that matters. **A voice has harmonic
/// structure only in the bottom few kilohertz.** Taking the cepstrum over the
/// whole spectrum of a 48 kHz signal spreads the harmonic ripple, which lives
/// in a fifth of the bins, across all of them, and the peak all but vanishes:
/// measured on real speech that way, the cepstral peak came out at 2 dB where
/// a synthetic vowel gives 13. The second reason is that a fixed grid makes
/// the measure — and so its threshold — independent of the sample rate.
const CEPSTRUM_BINS: usize = 512;
const CEPSTRUM_TOP_HZ: f64 = 5000.0;

/// How far below a frame's loudest bin the log spectrum is allowed to go
/// before it is floored, in dB.
///
/// This is not cosmetic. The cepstrum is a transform *of the log spectrum*, so
/// an empty bin taken literally — a power of 1e-30, which is −300 dB — is a
/// enormous excursion that the transform spreads across every quefrency and
/// which has nothing to do with the signal. Flooring the log spectrum at a
/// realistic dynamic range is what makes the cepstral peak a measurement of
/// voicing rather than of the zero-padding.
const SPECTRUM_FLOOR_DB: f64 = 120.0;

/// What a frame looked like. Public because tuning a gate means looking at
/// these, not at its verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Features {
    /// Frame level, dBFS.
    pub level_db: f64,
    /// The adaptive noise floor the level is judged against, dBFS.
    pub floor_db: f64,
    /// Energy in 300–3400 Hz over energy in 60–8000 Hz.
    pub speech_band_ratio: f64,
    /// Spectral flatness in the speech band: 1 is noise, 0 is a pure tone.
    pub flatness: f64,
    /// Cepstral peak prominence, dB. High means periodic — voiced.
    pub cpp: f64,
    /// RMS of the envelope's 4 Hz component, dB. High means syllabic.
    pub modulation: f64,
    /// Positive spectral flux in the speech band, normalised. High means the
    /// spectrum is moving — which speech does and a held note does not.
    pub flux: f64,
    /// Whether all four tests passed, before any smoothing in time.
    pub voice_like: bool,
}

/// The decision thresholds.
///
/// **These defaults are ours.** They were set by measuring the features on
/// labelled material — speech against noise, tones, sustained and moving
/// harmonic music, and applause — not taken from a specification, because none
/// specifies them. Each one sits in the middle of a plateau rather than at the
/// edge of a cliff: `cargo xtask speech` produced the sweeps, and the numbers
/// they gave are in `docs/speech.md` so the next person can disagree with them
/// from the same data rather than from taste.
///
/// They are a struct rather than constants so that a job can carry different
/// ones and so that the harness can sweep them without a rebuild.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// How far a frame must stand above the noise floor, dB.
    pub snr_db: f64,
    /// Minimum fraction of energy in the speech band.
    pub speech_band_ratio: f64,
    /// Minimum cepstral peak prominence, dB.
    pub cpp_db: f64,
    /// Minimum 4 Hz envelope modulation, dB. `NEG_INFINITY` disables the test.
    pub modulation_db: f64,
    /// Minimum spectral flux. `NEG_INFINITY` disables the test.
    pub flux: f64,
    /// Consecutive voice-like frames needed to open the gate.
    pub attack_frames: usize,
    /// Consecutive frames without a voice before it closes again. Long enough
    /// to hold across the gap between two words, short enough not to swallow
    /// the silence between two lines.
    pub hangover_frames: usize,
    /// Fraction of a 100 ms sub-block that must be inside an open gate for the
    /// sub-block to count as speech.
    pub subblock_coverage: f64,
    /// Fraction of a 400 ms block's frames that must be voice-like in their
    /// own right for the block to set the dialogue level.
    ///
    /// This is what keeps the hangover from carrying loud material into the
    /// measurement. A block at the end of a line is fully inside the open
    /// gate and holds one syllable of actual voice; a block in the middle of a
    /// line holds twenty.
    pub block_voice_fraction: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            snr_db: 8.0,
            speech_band_ratio: 0.50,
            cpp_db: 1.0,
            modulation_db: 2.0,
            flux: 0.02,
            attack_frames: 3,
            hangover_frames: 25,
            subblock_coverage: 1.0,
            block_voice_fraction: 0.25,
        }
    }
}

/// A streaming speech gate over a mono analysis signal.
#[derive(Debug, Clone)]
pub struct SpeechGate {
    thresholds: Thresholds,
    fft: Fft,

    hop: usize,
    window: usize,
    /// Hann window, precomputed.
    taper: Vec<f64>,
    /// The last `window` samples, oldest first, as a ring.
    ring: Vec<f64>,
    ring_at: usize,
    /// Samples accumulated since the last frame was analysed.
    since_frame: usize,

    /// Scratch, so analysing a frame allocates nothing.
    /// The transform the cepstrum is taken with, over the fixed grid.
    cepstral_fft: Fft,

    frame: Vec<f64>,
    power: Vec<f64>,
    /// The log spectrum, resampled onto the fixed 0–5 kHz grid.
    log_power: Vec<f64>,
    cepstrum: Vec<f64>,
    /// The previous frame's magnitude spectrum, for the flux.
    previous: Vec<f64>,
    has_previous: bool,

    /// Source-transform bins per step of the fixed cepstral grid.
    cepstrum_bin_step: f64,
    /// Bin ranges, resolved once from the sample rate.
    speech_bins: (usize, usize),
    full_bins: (usize, usize),
    quefrencies: (usize, usize),

    /// Noise floor, as a low percentile of the level over the last window.
    floor_db: f64,
    /// One counter per decibel of level, for the percentile.
    floor_histogram: Vec<u32>,
    floor_age: usize,
    floor_span: usize,

    /// Envelope state for the syllabic test.
    envelope_mean: f64,
    envelope_seen: usize,
    modulation: Biquad,
    modulation_state: (f64, f64),
    modulation_power: f64,

    /// One entry per analysed frame.
    frames: Vec<bool>,
    /// The most recent frame's features, for a caller that is tuning.
    last: Features,
    /// Every frame's features, kept only when asked: a two-hour programme is
    /// 720 000 frames, and nothing but calibration wants them.
    trace: Option<Vec<Features>>,
}

impl SpeechGate {
    pub fn new(sample_rate: u32) -> Self {
        Self::with_thresholds(sample_rate, Thresholds::default())
    }

    pub fn with_thresholds(sample_rate: u32, thresholds: Thresholds) -> Self {
        let rate = f64::from(sample_rate);
        let hop = (subblock_frames(sample_rate) / FRAMES_PER_SUBBLOCK).max(1);
        let window = hop * HOPS_PER_WINDOW;
        let size = window.next_power_of_two();

        let taper = (0..window)
            .map(|n| {
                let phase = 2.0 * std::f64::consts::PI * n as f64 / window as f64;
                0.5 - 0.5 * phase.cos()
            })
            .collect();

        let bin_of = |hz: f64| ((hz * size as f64 / rate).round() as usize).clamp(1, size / 2);
        // On the fixed grid the quefrency of a pitch is `2·F_top/F0`, and it
        // no longer has anything to do with the sample rate.
        let lag_of =
            |hz: f64| ((2.0 * CEPSTRUM_TOP_HZ / hz).round() as usize).clamp(1, CEPSTRUM_BINS);

        // The frame rate the envelope is tracked at.
        let frame_rate = rate / hop as f64;

        Self {
            thresholds,
            fft: Fft::new(size),
            cepstral_fft: Fft::new(CEPSTRUM_BINS * 2),
            hop,
            window,
            taper,
            ring: vec![0.0; window],
            ring_at: 0,
            since_frame: 0,
            frame: vec![0.0; window],
            power: vec![0.0; size / 2 + 1],
            log_power: vec![0.0; CEPSTRUM_BINS + 1],
            cepstrum: vec![0.0; CEPSTRUM_BINS * 2],
            previous: vec![0.0; size / 2 + 1],
            has_previous: false,
            cepstrum_bin_step: (CEPSTRUM_TOP_HZ * size as f64 / rate) / CEPSTRUM_BINS as f64,
            speech_bins: (bin_of(SPEECH_LOW_HZ), bin_of(SPEECH_HIGH_HZ)),
            full_bins: (bin_of(FULL_LOW_HZ), bin_of(FULL_HIGH_HZ)),
            quefrencies: (lag_of(F0_MAX_HZ), lag_of(F0_MIN_HZ)),
            floor_db: ABSOLUTE_FLOOR_DB,
            floor_histogram: vec![0; FLOOR_BINS],
            floor_age: 0,
            // 1.5 s per half-window: long enough to span a spoken phrase, so
            // the floor is the room rather than the quietest syllable.
            floor_span: (frame_rate * 1.5) as usize,
            envelope_mean: 0.0,
            envelope_seen: 0,
            modulation: Biquad::band_pass(SYLLABIC_HZ, SYLLABIC_Q, frame_rate),
            modulation_state: (0.0, 0.0),
            modulation_power: 0.0,
            frames: Vec::new(),
            trace: None,
            last: Features {
                level_db: ABSOLUTE_FLOOR_DB,
                floor_db: ABSOLUTE_FLOOR_DB,
                speech_band_ratio: 0.0,
                flatness: 1.0,
                cpp: 0.0,
                modulation: 0.0,
                flux: 0.0,
                voice_like: false,
            },
        }
    }

    /// Feed mono samples, normalised so full scale is ±1.
    pub fn push(&mut self, mono: &[f32]) {
        for &sample in mono {
            self.ring[self.ring_at] = f64::from(sample);
            self.ring_at = (self.ring_at + 1) % self.window;
            self.since_frame += 1;
            if self.since_frame == self.hop {
                self.since_frame = 0;
                self.analyse();
            }
        }
    }

    /// Analyse a final partial hop, so the tail of a programme is not lost.
    pub fn flush(&mut self) {
        if self.since_frame > 0 {
            self.since_frame = 0;
            self.analyse();
        }
    }

    /// One flag per completed 100 ms sub-block, aligned with [`crate::Meter`]'s
    /// sub-blocks: entry `i` describes the same 100 ms the meter's `i`th
    /// sub-block covers.
    pub fn subblocks(&self) -> Vec<bool> {
        let open = self.smoothed();
        open.chunks(FRAMES_PER_SUBBLOCK)
            .filter(|chunk| chunk.len() == FRAMES_PER_SUBBLOCK)
            .map(|chunk| {
                let voiced = chunk.iter().filter(|open| **open).count();
                voiced as f64 / chunk.len() as f64 >= self.thresholds.subblock_coverage
            })
            .collect()
    }

    /// Which 400 ms blocks are dialogue, given [`crate::Meter`]'s convention
    /// that block `i` covers sub-blocks `i..i+4`.
    ///
    /// Two conditions, and the second was put there by a measurement.
    ///
    /// **Every sub-block in the block has to be speech.** Measuring the level
    /// of dialogue means measuring blocks that are dialogue all the way
    /// through; a block that is one part speech and three parts explosion
    /// measures the explosion.
    ///
    /// **And a quarter of its frames have to be voice-like in their own
    /// right.** The hangover holds the gate open across the gaps inside
    /// speech, which is what it is for — but a block at the end of a line is
    /// mostly hangover, and if what follows the line is louder than the line,
    /// such blocks carry the loud material into the dialogue figure. Measured
    /// on a deliberate mixture — speech at −31 LUFS alternating with material
    /// at −12 — five such blocks out of a hundred and eighteen moved the
    /// answer by 4 LU, because a nineteen-decibel gap needs very few of them.
    /// The first and last sub-block of a block must each hold a voice-like
    /// frame as well, which bounds what a block can be contaminated by to one
    /// sub-block at either edge.
    pub fn blocks(&self) -> Vec<bool> {
        let subblocks = self.subblocks();
        if subblocks.len() < 4 {
            return Vec::new();
        }
        let per_block = FRAMES_PER_SUBBLOCK * 4;
        subblocks
            .windows(4)
            .enumerate()
            .map(|(index, window)| {
                if !window.iter().all(|speech| *speech) {
                    return false;
                }
                let from = index * FRAMES_PER_SUBBLOCK;
                let to = (from + per_block).min(self.frames.len());
                if to <= from {
                    return false;
                }
                // Voice at both ends, so whatever surrounds the block is kept
                // out of it: at most the width of one sub-block can be
                // something else, at either edge.
                let head = from..(from + FRAMES_PER_SUBBLOCK).min(to);
                let tail = to.saturating_sub(FRAMES_PER_SUBBLOCK).max(from)..to;
                if !self.frames[head].iter().any(|v| *v) || !self.frames[tail].iter().any(|v| *v) {
                    return false;
                }
                let voiced = self.frames[from..to].iter().filter(|v| **v).count();
                voiced as f64 / (to - from) as f64 >= self.thresholds.block_voice_fraction
            })
            .collect()
    }

    /// The fraction of the programme the gate held open.
    pub fn speech_fraction(&self) -> f64 {
        let open = self.smoothed();
        if open.is_empty() {
            return 0.0;
        }
        open.iter().filter(|o| **o).count() as f64 / open.len() as f64
    }

    /// The last frame's features, for calibration and for reporting.
    pub fn last_features(&self) -> Features {
        self.last
    }

    /// Keep every frame's features, for calibration.
    ///
    /// Off by default and deliberately opt-in: thresholds are set by looking
    /// at distributions, and a distribution over a programme is tens of
    /// megabytes.
    pub fn trace(&mut self) {
        self.trace.get_or_insert_with(Vec::new);
    }

    /// The traced features, empty unless [`Self::trace`] was called.
    pub fn traced(&self) -> &[Features] {
        self.trace.as_deref().unwrap_or(&[])
    }

    /// The raw per-frame verdicts, before smoothing in time.
    pub fn raw_frames(&self) -> &[bool] {
        &self.frames
    }

    /// The per-frame verdicts after the attack and hangover rules.
    pub fn open_frames(&self) -> Vec<bool> {
        self.smoothed()
    }

    pub fn frames_analysed(&self) -> usize {
        self.frames.len()
    }

    /// Voice-like frames, turned into a gate: a run has to establish itself
    /// before the gate opens, and it stays open across the gaps within speech.
    ///
    /// Done in one pass at the end rather than as the frames arrive, because
    /// the attack rule looks forward and doing it streaming would mean either
    /// a delay or a wrong answer at the boundary.
    fn smoothed(&self) -> Vec<bool> {
        let mut open = vec![false; self.frames.len()];
        let mut run = 0usize;
        let mut since_voice = usize::MAX;
        let mut is_open = false;

        for (index, &voice) in self.frames.iter().enumerate() {
            if voice {
                run += 1;
                since_voice = 0;
                if !is_open && run >= self.thresholds.attack_frames {
                    is_open = true;
                    // Backdate the onset: the frames that established the run
                    // are speech too, and dropping them would clip every word.
                    let from = index + 1 - run;
                    open[from..=index].fill(true);
                }
            } else {
                run = 0;
                since_voice = since_voice.saturating_add(1);
                if is_open && since_voice > self.thresholds.hangover_frames {
                    is_open = false;
                }
            }
            if is_open {
                open[index] = true;
            }
        }
        open
    }

    fn analyse(&mut self) {
        // Copy the ring out oldest-first and taper it.
        for (n, slot) in self.frame.iter_mut().enumerate() {
            let index = (self.ring_at + n) % self.window;
            *slot = self.ring[index] * self.taper[n];
        }

        let energy: f64 = self.frame.iter().map(|x| x * x).sum();
        // The taper takes energy out; putting it back keeps `level_db` a level
        // rather than a level minus the window's loss.
        let taper_power: f64 = self.taper.iter().map(|w| w * w).sum();
        let mean_square = if taper_power > 0.0 {
            energy / taper_power
        } else {
            0.0
        };
        let level_db = 10.0 * (mean_square + 1e-30).log10();

        self.fft.power_spectrum(&self.frame, &mut self.power);

        let (speech_low, speech_high) = self.speech_bins;
        let (full_low, full_high) = self.full_bins;
        let speech_energy: f64 = self.power[speech_low..=speech_high].iter().sum();
        let full_energy: f64 = self.power[full_low..=full_high].iter().sum();
        let speech_band_ratio = if full_energy > 0.0 {
            speech_energy / full_energy
        } else {
            0.0
        };

        let bins = (speech_high - speech_low + 1) as f64;
        let log_mean = self.power[speech_low..=speech_high]
            .iter()
            .map(|p| (p + 1e-30).ln())
            .sum::<f64>()
            / bins;
        let flatness = if speech_energy > 0.0 {
            (log_mean.exp() / (speech_energy / bins)).clamp(0.0, 1.0)
        } else {
            1.0
        };

        let cpp = self.cepstral_peak_prominence();
        let flux = self.spectral_flux();

        self.track_floor(level_db);
        let modulation = self.track_modulation(level_db);

        let voice_like = level_db > ABSOLUTE_FLOOR_DB
            && level_db > self.floor_db + self.thresholds.snr_db
            && speech_band_ratio >= self.thresholds.speech_band_ratio
            && cpp >= self.thresholds.cpp_db
            && flux >= self.thresholds.flux
            // The modulation tracker needs about a second before it means
            // anything; until then the test abstains rather than rejecting
            // the opening line of a programme.
            && (self.envelope_seen < self.floor_span / 2
                || modulation >= self.thresholds.modulation_db);

        self.last = Features {
            level_db,
            floor_db: self.floor_db,
            speech_band_ratio,
            flatness,
            cpp,
            modulation,
            flux,
            voice_like,
        };
        self.frames.push(voice_like);
        if let Some(trace) = self.trace.as_mut() {
            trace.push(self.last);
        }
    }

    /// How far the cepstrum's peak at a plausible pitch period stands above
    /// the trend across the search range.
    ///
    /// Prominence rather than height, because the cepstrum has a slope of its
    /// own that says nothing about voicing: subtracting a least-squares line
    /// through the search range is what makes the number comparable between a
    /// bright voice and a dull one.
    fn cepstral_peak_prominence(&mut self) -> f64 {
        let peak = self.power.iter().copied().fold(0.0f64, f64::max);
        if peak <= 0.0 {
            return 0.0;
        }
        let floor = peak * 10f64.powf(-SPECTRUM_FLOOR_DB / 10.0);

        // Resample the log spectrum onto the fixed grid. Linear between the
        // two neighbouring bins: the grid is finer than the transform's own
        // spacing at every rate this runs at, so this loses no ripple.
        let step = self.cepstrum_bin_step;
        for (index, slot) in self.log_power.iter_mut().enumerate() {
            let at = index as f64 * step;
            let low = at.floor() as usize;
            let high = (low + 1).min(self.power.len() - 1);
            let fraction = at - low as f64;
            let a = 10.0 * self.power[low.min(self.power.len() - 1)].max(floor).log10();
            let b = 10.0 * self.power[high].max(floor).log10();
            *slot = a + (b - a) * fraction;
        }
        self.cepstral_fft
            .real_even(&self.log_power, &mut self.cepstrum);

        let (low, high) = self.quefrencies;
        let high = high.min(self.cepstrum.len() - 1);
        if low >= high {
            return 0.0;
        }

        let count = (high - low + 1) as f64;
        let mut sum_q = 0.0;
        let mut sum_c = 0.0;
        let mut sum_qq = 0.0;
        let mut sum_qc = 0.0;
        let mut peak = f64::NEG_INFINITY;
        let mut peak_at = low;
        for q in low..=high {
            let c = self.cepstrum[q];
            let qf = q as f64;
            sum_q += qf;
            sum_c += c;
            sum_qq += qf * qf;
            sum_qc += qf * c;
            if c > peak {
                peak = c;
                peak_at = q;
            }
        }

        let denominator = count * sum_qq - sum_q * sum_q;
        let (slope, intercept) = if denominator.abs() > f64::EPSILON {
            let slope = (count * sum_qc - sum_q * sum_c) / denominator;
            (slope, (sum_c - slope * sum_q) / count)
        } else {
            (0.0, sum_c / count)
        };

        peak - (slope * peak_at as f64 + intercept)
    }

    /// How much of the speech band's magnitude appeared since the last frame.
    ///
    /// Positive flux only — what grew, not what decayed — normalised by the
    /// band's own magnitude so that it measures change rather than level. A
    /// held note scores near zero however loud it is; speech, which is one
    /// spectral shape replacing another every few tens of milliseconds, does
    /// not. This is what tells a sustained instrument from a voice, and it is
    /// the reason a 1 kHz tone — which is band-limited *and* perfectly
    /// periodic, and passes every other test here — is rejected.
    fn spectral_flux(&mut self) -> f64 {
        let (low, high) = self.speech_bins;
        let mut grown = 0.0;
        let mut total = 0.0;
        for k in low..=high {
            let magnitude = self.power[k].sqrt();
            grown += (magnitude - self.previous[k]).max(0.0);
            total += magnitude;
            self.previous[k] = magnitude;
        }
        let was_first = !self.has_previous;
        self.has_previous = true;
        if was_first || total <= 0.0 {
            // Nothing to compare against: abstain rather than report a step.
            return f64::INFINITY;
        }
        grown / total
    }

    /// The noise floor: a low percentile of the frame level over the last
    /// second and a half.
    ///
    /// A percentile and not a minimum, and that is not a refinement. A
    /// minimum is set by the single quietest frame in the window, so anything
    /// with gaps in it — music with a note attack every bar, an effect with a
    /// tail — drags the floor to nearly nothing and leaves the level test
    /// permanently satisfied. Measured on a mixture of speech and a moving
    /// harmonic line, that alone put two thirds of the music into the
    /// dialogue measurement.
    ///
    /// A one-decibel histogram rather than a ring of levels: constant space,
    /// one increment per frame, and one pass over a hundred counters per
    /// window.
    fn track_floor(&mut self, level_db: f64) {
        let bin = ((level_db - FLOOR_LOW_DB).round() as isize).clamp(0, FLOOR_BINS as isize - 1);
        self.floor_histogram[bin as usize] += 1;
        self.floor_age += 1;

        if self.floor_age >= self.floor_span {
            let total: u32 = self.floor_histogram.iter().sum();
            let wanted = ((f64::from(total) * FLOOR_PERCENTILE) as u32).max(1);
            let mut seen = 0;
            let mut at = 0;
            for (index, count) in self.floor_histogram.iter().enumerate() {
                seen += count;
                if seen >= wanted {
                    at = index;
                    break;
                }
            }
            self.floor_db = (FLOOR_LOW_DB + at as f64).max(ABSOLUTE_FLOOR_DB);
            self.floor_histogram.fill(0);
            self.floor_age = 0;
        }
    }

    /// The 4 Hz component of the level envelope, as an RMS in dB.
    fn track_modulation(&mut self, level_db: f64) -> f64 {
        // A long silence would otherwise drag the mean down by tens of dB and
        // make the return of speech look like enormous modulation.
        let level = level_db.max(self.floor_db - 10.0);

        // 1.5 s: slower than a syllable, faster than a scene.
        let alpha = 1.0 - (-1.0f64 / (self.floor_span as f64)).exp();
        if self.envelope_seen == 0 {
            self.envelope_mean = level;
        } else {
            self.envelope_mean += (level - self.envelope_mean) * alpha;
        }
        self.envelope_seen += 1;

        let ac = (level - self.envelope_mean).clamp(-30.0, 30.0);
        let filtered = self.modulation.step(&mut self.modulation_state, ac);
        self.modulation_power += (filtered * filtered - self.modulation_power) * alpha;
        self.modulation_power.max(0.0).sqrt()
    }
}

/// A transposed direct-form-II biquad, private to the envelope filter.
///
/// [`crate::kweighting::Biquad`] is the same shape but is the K-weighting's,
/// and coupling a speech feature to a loudness filter's type would mean a
/// change to one showing up in the other.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Biquad {
    /// Constant-peak-gain band-pass, from the audio EQ cookbook.
    fn band_pass(centre_hz: f64, q: f64, sample_rate: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * centre_hz / sample_rate;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: alpha / a0,
            b1: 0.0,
            b2: -alpha / a0,
            a1: -2.0 * w0.cos() / a0,
            a2: (1.0 - alpha) / a0,
        }
    }

    fn step(&self, state: &mut (f64, f64), x: f64) -> f64 {
        let y = self.b0 * x + state.0;
        state.0 = self.b1 * x - self.a1 * y + state.1;
        state.1 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Which part of a presentation the gate listens to.
///
/// Not the whole mix: dialogue lives in the centre channel, and a gate given
/// the centre alone has most of its discrimination done for it before it
/// starts. Only when there is no centre does it fall back to the mid of the
/// front pair, which is where a stereo mix puts a voice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Analysis {
    /// One channel, by index into the interleaved frame.
    Channel(usize),
    /// The mean of several.
    Mean(Vec<usize>),
}

impl Analysis {
    /// The centre if the layout has one, otherwise the mid of the front pair.
    ///
    /// Labels are the ADM common-definition ones, which is what both the
    /// master format and ADM resolve to here.
    pub fn for_labels(labels: &[&str]) -> Self {
        if let Some(centre) = labels.iter().position(|l| matches!(*l, "M+000" | "C")) {
            return Self::Channel(centre);
        }
        let front: Vec<usize> = labels
            .iter()
            .enumerate()
            .filter(|(_, l)| matches!(**l, "M+030" | "M-030" | "L" | "R"))
            .map(|(index, _)| index)
            .collect();
        if front.is_empty() {
            Self::Channel(0)
        } else {
            Self::Mean(front)
        }
    }

    fn sample(&self, frame: &[f32]) -> f32 {
        match self {
            Self::Channel(index) => frame.get(*index).copied().unwrap_or(0.0),
            Self::Mean(indices) => {
                if indices.is_empty() {
                    return 0.0;
                }
                let sum: f32 = indices
                    .iter()
                    .map(|index| frame.get(*index).copied().unwrap_or(0.0))
                    .sum();
                sum / indices.len() as f32
            }
        }
    }
}

/// A loudness meter that also reports the loudness of the dialogue in what it
/// measured.
///
/// Both figures come from the same blocks: the programme figure integrates all
/// of them, the dialogue figure integrates the ones the gate held open. So the
/// difference between the two is a property of the gate and of nothing else,
/// which is what makes it comparable against another engine's.
#[derive(Debug, Clone)]
pub struct DialogueMeter {
    meter: crate::Meter,
    gate: SpeechGate,
    analysis: Analysis,
    /// Scratch for the analysis signal, so pushing allocates nothing.
    mono: Vec<f32>,
}

impl DialogueMeter {
    pub fn new(sample_rate: u32, weights: &[crate::ChannelWeight], analysis: Analysis) -> Self {
        Self {
            meter: crate::Meter::new(sample_rate, weights),
            gate: SpeechGate::new(sample_rate),
            analysis,
            mono: Vec::new(),
        }
    }

    pub fn with_thresholds(
        sample_rate: u32,
        weights: &[crate::ChannelWeight],
        analysis: Analysis,
        thresholds: Thresholds,
    ) -> Self {
        Self {
            meter: crate::Meter::new(sample_rate, weights),
            gate: SpeechGate::with_thresholds(sample_rate, thresholds),
            analysis,
            mono: Vec::new(),
        }
    }

    /// Feed interleaved samples, normalised so full scale is ±1.
    pub fn push(&mut self, interleaved: &[f32]) {
        let channels = self.meter.channels();
        if channels == 0 {
            return;
        }
        self.mono.clear();
        self.mono.reserve(interleaved.len() / channels);
        for frame in interleaved.chunks_exact(channels) {
            self.mono.push(self.analysis.sample(frame));
        }
        self.meter.push(interleaved);
        self.gate.push(&self.mono);
    }

    pub fn flush(&mut self) {
        self.meter.flush();
        self.gate.flush();
    }

    /// Loudness of the whole programme, BS.1770-4.
    pub fn integrated(&self) -> f64 {
        self.meter.integrated()
    }

    /// Loudness of the blocks the gate called dialogue.
    ///
    /// [`crate::SILENCE_LUFS`] when the gate never opened, which is a real
    /// answer — a programme with no speech in it has no dialogue level — and
    /// has to be handled rather than rounded into a `dialnorm`.
    pub fn dialogue(&self) -> f64 {
        let speech = self.gate.blocks();
        if !speech.iter().any(|s| *s) {
            return crate::SILENCE_LUFS;
        }
        self.meter
            .integrated_over(|index| speech.get(index).copied().unwrap_or(false))
    }

    /// The fraction of 400 ms blocks the gate called dialogue.
    ///
    /// Worth printing next to any dialogue figure: a gate that kept 2 % of a
    /// programme measured two minutes of it, and the reader should know.
    pub fn dialogue_fraction(&self) -> f64 {
        let speech = self.gate.blocks();
        if speech.is_empty() {
            return 0.0;
        }
        speech.iter().filter(|s| **s).count() as f64 / speech.len() as f64
    }

    pub fn loudness_range(&self) -> f64 {
        self.meter.loudness_range()
    }

    pub fn meter(&self) -> &crate::Meter {
        &self.meter
    }

    pub fn gate(&self) -> &SpeechGate {
        &self.gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelWeight;

    const RATE: u32 = 48_000;

    /// A crude voice: a glottal pulse train through three formant resonators.
    ///
    /// Not a claim to sound like anyone. It is periodic, its energy sits where
    /// a voice's does, and the caller can move its pitch and its formants —
    /// which is exactly what the gate looks at, so it is a fixture that tests
    /// the gate rather than a recording that tests the recording.
    fn vowel(f0: f64, formants: [f64; 3], seconds: f64) -> Vec<f32> {
        let n = (f64::from(RATE) * seconds) as usize;
        let period = (f64::from(RATE) / f0).max(2.0) as usize;
        let mut out = vec![0.0f64; n];
        for (index, &centre) in formants.iter().enumerate() {
            let bandwidth = 80.0 + 40.0 * index as f64;
            let r = (-std::f64::consts::PI * bandwidth / f64::from(RATE)).exp();
            let theta = 2.0 * std::f64::consts::PI * centre / f64::from(RATE);
            let (a1, a2) = (-2.0 * r * theta.cos(), r * r);
            let (mut y1, mut y2) = (0.0, 0.0);
            for (i, slot) in out.iter_mut().enumerate() {
                let x = if i % period == 0 { 1.0 } else { 0.0 };
                let y = x - a1 * y1 - a2 * y2;
                y2 = y1;
                y1 = y;
                *slot += y * 0.3;
            }
        }
        normalised(&out, 0.25)
    }

    /// Vowels with gaps between them: the syllabic rhythm speech has and a
    /// held note does not.
    fn syllables(seconds: f64) -> Vec<f32> {
        const VOWELS: [[f64; 3]; 4] = [
            [730.0, 1090.0, 2440.0],
            [270.0, 2290.0, 3010.0],
            [530.0, 1840.0, 2480.0],
            [570.0, 840.0, 2410.0],
        ];
        let mut out = Vec::new();
        let mut which = 0;
        while out.len() < (f64::from(RATE) * seconds) as usize {
            // Pitch drifts syllable to syllable, as a voice does.
            let f0 = 110.0 + 20.0 * (which as f64 * 0.7).sin();
            out.extend(vowel(f0, VOWELS[which % VOWELS.len()], 0.16));
            out.extend(std::iter::repeat_n(
                0.0f32,
                (f64::from(RATE) * 0.08) as usize,
            ));
            which += 1;
        }
        out.truncate((f64::from(RATE) * seconds) as usize);
        out
    }

    fn tone(hz: f64, seconds: f64) -> Vec<f32> {
        let n = (f64::from(RATE) * seconds) as usize;
        (0..n)
            .map(|i| {
                (0.25 * (std::f64::consts::TAU * hz * i as f64 / f64::from(RATE)).sin()) as f32
            })
            .collect()
    }

    /// Deterministic pink-ish noise: white through a one-pole tilt.
    fn noise(seconds: f64) -> Vec<f32> {
        let n = (f64::from(RATE) * seconds) as usize;
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut low = 0.0f64;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let white = (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            low += 0.05 * (white - low);
            out.push(low + 0.15 * white);
        }
        normalised(&out, 0.25)
    }

    fn normalised(samples: &[f64], to: f64) -> Vec<f32> {
        let peak = samples
            .iter()
            .fold(0.0f64, |m, x| m.max(x.abs()))
            .max(1e-12);
        samples.iter().map(|x| (x / peak * to) as f32).collect()
    }

    fn gate(samples: &[f32]) -> SpeechGate {
        let mut gate = SpeechGate::new(RATE);
        gate.push(samples);
        gate.flush();
        gate
    }

    fn open_rate(gate: &SpeechGate) -> f64 {
        let open = gate.open_frames();
        if open.is_empty() {
            return 0.0;
        }
        open.iter().filter(|o| **o).count() as f64 / open.len() as f64
    }

    /// The measurement the whole cepstral path exists to make. A periodic
    /// voice has a cepstral peak well clear of the trend around it; if this
    /// number collapses, the gate has stopped looking at voicing and is
    /// running on the other features alone.
    #[test]
    fn a_vowel_is_strongly_voiced() {
        let mut gate = SpeechGate::new(RATE);
        gate.trace();
        gate.push(&vowel(120.0, [730.0, 1090.0, 2440.0], 1.0));
        gate.flush();

        let traced = gate.traced();
        let middle = traced[traced.len() / 2];
        assert!(
            middle.cpp > 6.0,
            "a periodic vowel should be plainly voiced, got {:.2} dB",
            middle.cpp
        );
        assert!(
            middle.speech_band_ratio > 0.8,
            "its energy should be in the speech band, got {:.2}",
            middle.speech_band_ratio
        );
    }

    #[test]
    fn syllables_are_speech() {
        let gate = gate(&syllables(6.0));
        assert!(
            open_rate(&gate) > 0.7,
            "the gate should be open through most of a spoken passage, was {:.0}%",
            100.0 * open_rate(&gate)
        );
    }

    #[test]
    fn noise_is_not_speech() {
        let gate = gate(&noise(6.0));
        assert!(
            open_rate(&gate) < 0.01,
            "noise is not speech, but the gate opened for {:.1}% of it",
            100.0 * open_rate(&gate)
        );
    }

    /// A held tone passes two of the four tests — it is band-limited and
    /// perfectly periodic — so this is the case that says whether the other
    /// two are doing anything.
    #[test]
    fn a_held_tone_is_not_speech() {
        let gate = gate(&tone(900.0, 6.0));
        assert!(
            open_rate(&gate) < 0.01,
            "a held tone is not speech, but the gate opened for {:.1}% of it",
            100.0 * open_rate(&gate)
        );
    }

    /// The test the gate exists to pass: quiet dialogue under loud
    /// everything-else, and the dialogue figure has to find the dialogue.
    ///
    /// The programme figure reads the mixture, which is what BS.1770 is for.
    /// A `dialnorm` derived from *that* would tell a decoder the programme is
    /// loud and leave the dialogue too quiet to hear, which is the whole
    /// reason for a speech gate.
    #[test]
    fn the_dialogue_figure_finds_the_quiet_voice() {
        let speech = syllables(6.0);
        let loud = noise(6.0);

        // 18 dB between them, alternating in six-second turns.
        let quiet: Vec<f32> = speech.iter().map(|s| s * 0.05).collect();
        let mut timeline = Vec::new();
        for turn in 0..4 {
            if turn % 2 == 0 {
                timeline.extend_from_slice(&quiet);
            } else {
                timeline.extend_from_slice(&loud);
            }
        }

        let mut meter = DialogueMeter::new(RATE, &[ChannelWeight::Unity], Analysis::Channel(0));
        meter.push(&timeline);
        meter.flush();

        let programme = meter.integrated();
        let dialogue = meter.dialogue();
        assert!(dialogue.is_finite(), "the gate found no dialogue at all");
        assert!(
            dialogue < programme - 10.0,
            "dialogue {dialogue:.1} should sit well below programme {programme:.1}"
        );

        // What the speech measures on its own is the answer the gate should
        // recover. It is allowed to be a little high — it keeps the blocks
        // that hold a voice and drops the gaps — but not by a decibel step.
        let mut alone = crate::Meter::new(RATE, &[ChannelWeight::Unity]);
        alone.push(&quiet);
        alone.flush();
        let truth = alone.integrated();
        assert!(
            (dialogue - truth).abs() < 1.0,
            "dialogue {dialogue:.2} against the speech's own {truth:.2}"
        );
    }

    /// The gate and the meter have to agree about which 400 ms is which, or
    /// the dialogue figure measures the wrong blocks — quietly, and by an
    /// amount that looks like a tuning problem.
    #[test]
    fn the_gate_and_the_meter_share_a_grid() {
        for rate in [44_100u32, 48_000, 96_000] {
            let seconds = 4.0;
            let samples: Vec<f32> = (0..(f64::from(rate) * seconds) as usize)
                .map(|i| (0.2 * (i as f64 * 0.01).sin()) as f32)
                .collect();

            let mut meter = crate::Meter::new(rate, &[ChannelWeight::Unity]);
            meter.push(&samples);
            meter.flush();

            let mut gate = SpeechGate::with_thresholds(rate, Thresholds::default());
            gate.push(&samples);
            gate.flush();

            let difference = meter.blocks().len() as isize - gate.blocks().len() as isize;
            assert!(
                difference.abs() <= 1,
                "at {rate} Hz the meter has {} blocks and the gate {}",
                meter.blocks().len(),
                gate.blocks().len()
            );
        }
    }

    #[test]
    fn the_centre_is_where_it_listens() {
        let five_one = ["M+030", "M-030", "M+000", "LFE1", "M+110", "M-110"];
        assert_eq!(Analysis::for_labels(&five_one), Analysis::Channel(2));

        let stereo = ["M+030", "M-030"];
        assert_eq!(Analysis::for_labels(&stereo), Analysis::Mean(vec![0, 1]));
    }

    /// Selecting no blocks is a real answer, not an error, and it must not
    /// come back as a number a `dialnorm` could be rounded out of.
    #[test]
    fn a_programme_with_no_speech_has_no_dialogue_level() {
        let mut meter = DialogueMeter::new(RATE, &[ChannelWeight::Unity], Analysis::Channel(0));
        meter.push(&tone(440.0, 6.0));
        meter.flush();

        assert!(meter.integrated().is_finite(), "the tone has a loudness");
        assert!(
            !meter.dialogue().is_finite(),
            "but it has no dialogue, and the gate said {:.1}",
            meter.dialogue()
        );
    }
}
