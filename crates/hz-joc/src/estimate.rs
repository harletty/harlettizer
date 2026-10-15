//! Estimating the parameters: from the downmix and the objects, the matrices
//! that rebuild the objects from the downmix.
//!
//! In the shape every reference stream codes its objects in: twelve parameter
//! bands, fine quantisation, dense coefficients, one data point a frame, the
//! slope smooth. For each object and band the decoder applies one real gain
//! per downmix channel to every QMF subband of the band, and slides from the
//! last frame's gains to this frame's across the frame's 24 slots — at slot
//! `t` it holds `(1 − w)·p + w·c`, `w = (t + 1)/24`, reaching `c` on the last.
//! Three ways to choose `c`, by [`Fit`]:
//!
//! - [`Fit::Ramp`] — the least-squares fit of the frame's subbands *through the
//!   ramp*, the previous gains fixed at what the decoder holds:
//!
//!   ```text
//!   minimise Σ_t Σ_k |O_t[k] − ((1 − w_t)·p + w_t·c)ᵀ X_t[k]|²   over real c
//!   ⇒ (Σ w² Re XXᴴ + λI)·c = Σ w Re(X·conj O) − (Σ w(1 − w) Re XXᴴ)·p
//!   ```
//!
//!   The least error inside each frame, at a price: to make up for the early
//!   slots, still near the old gains, it overshoots the new ones, and the next
//!   frame corrects the other way — on a steady mix about ×1.47, ×0.78,
//!   ×1.10, … — a modulation that decays but is there.
//! - [`Fit::Centred`] — the gains the object needs where the decoder reaches
//!   them: the plain least-squares fit over a frame's worth of slots centred
//!   on the frame's end, half of it from the next frame. No overshoot; a frame
//!   of latency.
//! - [`Fit::Synthesis`], the default — the gains that make the *decoder's
//!   output* closest to the object: each downmix channel's share of each band
//!   is synthesised through the same bank the decoder runs, and all of an
//!   object's gains are fitted together, in time, under a frame-long Hann
//!   window centred on where the decoder's arrival at them is heard. A
//!   band-by-band fit in the QMF domain cannot see that the decoder's
//!   synthesis lets each subband leak into its neighbours, which in the narrow
//!   low bands is a large share of the band; this one fits what the decoder
//!   plays, leakage and all. A frame of latency, like the centred fit, and
//!   about three times its work.
//!
//! In the QMF domain the normal equations' matrices depend on the downmix
//! alone and are formed once a frame and band; only the right-hand side is the
//! object's. By synthesis it is the same, over all the bands at once.
//! The least-squares fit is the minimum-mean-square-error reconstruction the
//! joint object coding literature describes (Purnhagen et al., AES 2016); the
//! ramp, the held gains and the quantisation are what make it this decoder's.
//! Which fit to use is measured, not argued: see `docs/joc.md`.

use crate::payload::{Data, Joc, Object};
use crate::qmf::{Analysis, DELAY, SUBBANDS, Slot, Synthesis};

/// Samples, and slots, in a frame of six blocks.
const FRAME: usize = 1536;
pub const SLOTS: usize = FRAME / SUBBANDS;
/// Slots the steep decision weighs: the frame and half the next.
const SPAN: usize = SLOTS + SLOTS / 2;
/// The share of the ramp's error a step must leave to be taken.
const STEEP_MARGIN: f32 = 0.7;
/// The share of the object's energy over the span a step must buy back.
const STEEP_FLOOR: f64 = 1e-4;
/// The parameter bands every reference stream uses: index 5, twelve bands.
pub const BANDS_INDEX: u8 = 5;
pub const BANDS: usize = 12;
/// The first subband of each of the twelve bands (TS 103 420 table 54); the
/// last runs to the top.
const BAND_START: [usize; BANDS] = [0, 1, 2, 3, 4, 6, 8, 11, 14, 18, 23, 35];
/// The fine quantiser's step (clause 6.6.4): `820 / (4096 · 2)`.
pub const STEP: f32 = 820.0 / 8192.0;
/// Its codes run from −96 to 95 steps; the dense chain counts them modulo
/// 192.
const LEVELS: i32 = 192;
const MAX_CHANNELS: usize = 7;

fn band_of(subband: usize) -> usize {
    BAND_START
        .iter()
        .rposition(|start| *start <= subband)
        .expect("band 0 starts at 0")
}

/// Per downmix channel and band, a gain: one object's matrix.
pub type Matrix = [[f32; BANDS]; MAX_CHANNELS];

/// How a frame's gains are chosen; see the module notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    Ramp,
    Centred,
    #[default]
    Synthesis,
}

/// The state an encoder keeps across frames: the analysis banks, two frames
/// of subbands, and what the decoder holds for every object — the last gains
/// it was given.
pub struct Estimator {
    channels: usize,
    downmix_config: u8,
    core: Vec<Analysis>,
    objects: Vec<Analysis>,
    held: Vec<Matrix>,
    /// The last two frames' slots: the earlier frame in the first half.
    core_slots: Vec<[Slot; 2 * SLOTS]>,
    object_slots: Vec<[Slot; 2 * SLOTS]>,
    /// Which objects the earlier of the two frames carries, once there is one.
    pending: Option<Vec<bool>>,
    /// For [`Fit::Synthesis`]: a synthesis bank per downmix channel and band,
    /// the last two frames of what each makes of its share of the downmix,
    /// the objects on the decoder's clock over the same two frames, and the
    /// last frame of them as they came, to delay the next by.
    basis: Vec<Synthesis>,
    basis_out: Vec<Vec<f32>>,
    target: Vec<Vec<f32>>,
    raw: Vec<Vec<f32>>,
    /// The square root of the frame-long Hann window [`Fit::Synthesis`]
    /// weighs the output by, and the fit's scratch: the basis and the target
    /// under it, the normal equations, their factor, a right-hand side and its
    /// solution, and the smaller system left when gains are held at the
    /// quantiser's bounds — kept, so that a frame allocates nothing for them.
    window_root: Vec<f64>,
    weighted: Vec<Vec<f64>>,
    weighted_target: Vec<f64>,
    gram: Vec<f64>,
    factor: Vec<f64>,
    solution: Vec<f64>,
    rhs: Vec<f64>,
    bounded: Vec<bool>,
    free: Vec<usize>,
    reduced: Vec<f64>,
    reduced_factor: Vec<f64>,
    /// For the steep decision: each slot's downmix matrices, per band, over
    /// the frame and half the next, and an object's side of them; and the
    /// fit by synthesis on part of the window, apart from the frame's.
    slot_gram: Vec<[[f64; MAX_CHANNELS]; MAX_CHANNELS]>,
    slot_rhs: Vec<[f64; MAX_CHANNELS]>,
    slot_energy: Vec<f64>,
    steep_gram: Vec<f64>,
    steep_factor: Vec<f64>,
    /// The gains the steep decision found for the step it chose.
    step_gains: Matrix,
    sequence: u16,
    pub fit: Fit,
    /// The ridge on the normal equations, relative to their mean diagonal:
    /// it keeps a band the downmix barely fills from asking for gains the
    /// quantiser cannot hold. 10⁻⁵ unless set: on the reference streams it
    /// put more gains on the reference's step than 10⁻³ did, and 10⁻¹ cost
    /// 13 dB.
    pub ridge: f32,
    /// With [`Fit::Synthesis`]: step an object's gains at a slot, rather
    /// than ramp them across the frame, where that leaves less than this
    /// share of the ramp's error in the frame. Nought never steps.
    pub steep_margin: f32,
}

impl Estimator {
    /// For `objects` objects, rebuilt from the downmix `downmix_config`
    /// names — nought, a 5.1 downmix, is what this project writes.
    pub fn new(objects: usize, downmix_config: u8) -> Self {
        let channels = match downmix_config {
            0 | 3 => 5,
            _ => 7,
        };
        Self {
            channels,
            downmix_config,
            core: vec![Analysis::new(); channels],
            objects: vec![Analysis::new(); objects],
            held: vec![[[0.0; BANDS]; MAX_CHANNELS]; objects],
            core_slots: vec![[Slot::default(); 2 * SLOTS]; channels],
            object_slots: vec![[Slot::default(); 2 * SLOTS]; objects],
            pending: None,
            basis: vec![Synthesis::new(); channels * BANDS],
            basis_out: vec![vec![0.0; 2 * FRAME]; channels * BANDS],
            target: vec![vec![0.0; 2 * FRAME]; objects],
            raw: vec![vec![0.0; FRAME]; objects],
            window_root: (0..FRAME)
                .map(|t| (std::f64::consts::PI * (t as f64 + 0.5) / FRAME as f64).sin())
                .collect(),
            weighted: vec![vec![0.0; FRAME]; channels * BANDS],
            weighted_target: vec![0.0; FRAME],
            gram: vec![0.0; (channels * BANDS).pow(2)],
            factor: vec![0.0; (channels * BANDS).pow(2)],
            solution: vec![0.0; channels * BANDS],
            rhs: vec![0.0; channels * BANDS],
            bounded: vec![false; channels * BANDS],
            free: Vec::with_capacity(channels * BANDS),
            reduced: vec![0.0; (channels * BANDS).pow(2)],
            reduced_factor: vec![0.0; (channels * BANDS).pow(2)],
            slot_gram: vec![[[0.0; MAX_CHANNELS]; MAX_CHANNELS]; SPAN * BANDS],
            slot_rhs: vec![[0.0; MAX_CHANNELS]; SPAN * BANDS],
            slot_energy: vec![0.0; SPAN * BANDS],
            steep_gram: vec![0.0; (channels * BANDS).pow(2)],
            steep_factor: vec![0.0; (channels * BANDS).pow(2)],
            step_gains: [[0.0; BANDS]; MAX_CHANNELS],
            sequence: 0,
            fit: Fit::default(),
            ridge: 1e-5,
            steep_margin: STEEP_MARGIN,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// What the decoder holds for an object after the last frame emitted.
    pub fn held(&self, object: usize) -> &Matrix {
        &self.held[object]
    }

    /// Number the next frame emitted `sequence`, to follow a stream's own
    /// count. A new stream starts at nought, its first frame.
    pub fn set_sequence(&mut self, sequence: u16) {
        self.sequence = sequence & 1023;
    }

    /// Take one frame: 1536 samples of each downmix channel, in the order the
    /// configuration names them (L, R, C, Ls, Rs, then the pair a
    /// seven-channel downmix adds), and of each object, on the downmix's
    /// clock — what an ideal reconstruction would hand back. `present` says
    /// which objects the frame carries; an absent one keeps what the decoder
    /// holds, as the decoder does.
    ///
    /// Returns the payload of the frame *before*, a frame late — the centred
    /// fit looks into this one — and nothing for the first.
    pub fn push(&mut self, core: &[&[f32]], objects: &[&[f32]], present: &[bool]) -> Option<Joc> {
        assert_eq!(core.len(), self.channels);
        assert_eq!(objects.len(), self.objects.len());
        assert_eq!(present.len(), self.objects.len());
        for (slots, (bank, signal)) in self
            .core_slots
            .iter_mut()
            .zip(self.core.iter_mut().zip(core))
            .chain(
                self.object_slots
                    .iter_mut()
                    .zip(self.objects.iter_mut().zip(objects)),
            )
        {
            slots.copy_within(SLOTS.., 0);
            for t in 0..SLOTS {
                bank.slot(
                    &signal[t * SUBBANDS..(t + 1) * SUBBANDS],
                    &mut slots[SLOTS + t],
                );
            }
        }
        if self.fit == Fit::Synthesis {
            self.synthesise(objects);
        }
        let earlier = self.pending.replace(present.to_vec());
        earlier.map(|present| self.estimate(&present))
    }

    /// Run the new frame through the synthesis basis, and put the objects on
    /// the decoder's clock beside it.
    fn synthesise(&mut self, objects: &[&[f32]]) {
        for out in &mut self.basis_out {
            out.copy_within(FRAME.., 0);
        }
        for ch in 0..self.channels {
            for t in 0..SLOTS {
                let slot = self.core_slots[ch][SLOTS + t];
                for band in 0..BANDS {
                    let mut share = Slot::default();
                    for k in band_range(band) {
                        share.real[k] = slot.real[k];
                        share.imaginary[k] = slot.imaginary[k];
                    }
                    let i = ch * BANDS + band;
                    let at = FRAME + t * SUBBANDS;
                    self.basis[i].slot(&share, &mut self.basis_out[i][at..at + SUBBANDS]);
                }
            }
        }
        for ((target, raw), object) in self.target.iter_mut().zip(&mut self.raw).zip(objects) {
            target.copy_within(FRAME.., 0);
            for n in 0..FRAME {
                target[FRAME + n] = if n >= DELAY {
                    object[n - DELAY]
                } else {
                    raw[FRAME + n - DELAY]
                };
            }
            raw.copy_from_slice(object);
        }
    }

    /// The payload of the earlier of the two frames held, by synthesis.
    fn estimate_by_synthesis(&mut self, present: &[bool]) -> Joc {
        let n = self.channels;
        // The earlier frame ends at FRAME on the core's clock. The decoder
        // reaches its gains at that slot, and a slot's synthesis is spread
        // over the prototype's length behind it, so they are heard most
        // around FRAME + DELAY / 2 — not FRAME + DELAY, where that sample of
        // the core comes out. Fitted there under a frame-long Hann window:
        // the decoder ramps towards each frame's gains from the last's, and
        // the taper weighs the output the way the ramps share it out, where a
        // flat window of the same length fits each frame's gains to its
        // neighbours' mixes too, and a short flat one chases transients.
        let start = FRAME + DELAY / 2 - FRAME / 2;
        for (weighted, basis) in self.weighted.iter_mut().zip(&self.basis_out) {
            for ((w, &root), &value) in weighted
                .iter_mut()
                .zip(&self.window_root)
                .zip(&basis[start..start + FRAME])
            {
                *w = root * f64::from(value);
            }
        }
        self.synthesis_system(0);
        let stepping = self.steep_margin > 0.0;
        if stepping {
            self.slot_matrices();
        }

        let mut out_objects = Vec::with_capacity(self.objects.len());
        for (o, &here) in present.iter().enumerate() {
            if !here {
                out_objects.push(None);
                continue;
            }
            for ((w, &root), &value) in self
                .weighted_target
                .iter_mut()
                .zip(&self.window_root)
                .zip(&self.target[o][start..start + FRAME])
            {
                *w = root * f64::from(value);
            }
            self.synthesis_solve(0);
            let step = if stepping { self.steep_split(o) } else { None };
            let split = step.map(|step| step.split);
            if let Some(split) = split {
                // Stepped at `split`: refitted on the output only the slots
                // from there on make — a slot's synthesis reaches nine slots
                // past it — or on all of the window if that starts later.
                let from = (split * SUBBANDS + DELAY - 1).saturating_sub(start);
                if from > 0 {
                    std::mem::swap(&mut self.gram, &mut self.steep_gram);
                    std::mem::swap(&mut self.factor, &mut self.steep_factor);
                    self.synthesis_system(from);
                    self.synthesis_solve(from);
                    std::mem::swap(&mut self.gram, &mut self.steep_gram);
                    std::mem::swap(&mut self.factor, &mut self.steep_factor);
                }
            }
            let x = &self.solution;
            let (mut codes, mut gains) = quantise(n, |ch, band| x[ch * BANDS + band]);
            if let Some(step) = step {
                // The gains sent are held to the test the step passed: the
                // refit on the output after it can find nothing there to fit
                // — a transient over before its window begins — and come
                // back nought. Then the decision's own gains are sent, fitted
                // on every slot from the step on.
                let error = (0..BANDS)
                    .map(|band| self.stepped_error(band, step.split, &self.held[o], &gains))
                    .sum();
                if !step.passes(error, self.steep_margin) {
                    let found = self.step_gains;
                    (codes, gains) = quantise(n, |ch, band| f64::from(found[ch][band]));
                }
            }
            self.held[o] = gains;
            out_objects.push(Some(Object {
                bands_index: BANDS_INDEX,
                fine: true,
                steep: split.is_some(),
                // The decoder switches at the slot one past the code.
                offsets: split.map(|s| vec![(s - 1) as u8]).unwrap_or_default(),
                data: Data::Dense(vec![codes]),
            }));
        }
        self.joc(out_objects)
    }

    /// The fit by synthesis's normal equations on the window from `from`
    /// samples into it, ridged, into `gram`, and their factor into `factor`.
    fn synthesis_system(&mut self, from: usize) {
        let (n, m) = (self.channels, self.channels * BANDS);
        let gram = &mut self.gram;
        for i in 0..m {
            for j in i..m {
                let v = dot(&self.weighted[i][from..], &self.weighted[j][from..]);
                gram[i * m + j] = v;
                gram[j * m + i] = v;
            }
        }
        // The ridge is one per band, shared by its channels: the bands span
        // tens of decibels, and one ridge across all sixty flattens the
        // quiet ones to nought; but within a band it must weigh every
        // channel's gain alike, so that of two channels carrying one signal
        // the fit leans on the louder — one per signal, against that signal's
        // own energy, asks a channel at −40 dB for a gain of fifty.
        for band in 0..BANDS {
            let mean = (0..n)
                .map(|ch| gram[(ch * BANDS + band) * (m + 1)])
                .sum::<f64>()
                / n as f64;
            let lambda = f64::from(self.ridge) * mean + 1e-12;
            for ch in 0..n {
                gram[(ch * BANDS + band) * (m + 1)] += lambda;
            }
        }
        cholesky(gram, m, &mut self.factor);
    }

    /// The gains for the weighted target on the window from `from` samples
    /// into it, under the quantiser's bounds, into `solution`.
    fn synthesis_solve(&mut self, from: usize) {
        let m = self.channels * BANDS;
        for (b, weighted) in self.rhs.iter_mut().zip(&self.weighted) {
            *b = dot(&weighted[from..], &self.weighted_target[from..]);
        }
        self.solution.copy_from_slice(&self.rhs);
        cholesky_solve(&self.factor, m, &mut self.solution);
        self.hold_to_bounds(m);
    }

    /// Each slot's downmix matrices per band, over the frame and half the
    /// next: what the steep decision weighs every object against.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn slot_matrices(&mut self) {
        let n = self.channels;
        self.slot_gram.fill([[0.0; MAX_CHANNELS]; MAX_CHANNELS]);
        for t in 0..SPAN {
            for k in 0..SUBBANDS {
                let cell = &mut self.slot_gram[t * BANDS + band_of(k)];
                for i in 0..n {
                    let xi = &self.core_slots[i][t];
                    let (ri, ii) = (f64::from(xi.real[k]), f64::from(xi.imaginary[k]));
                    for j in i..n {
                        let xj = &self.core_slots[j][t];
                        cell[i][j] += ri * f64::from(xj.real[k]) + ii * f64::from(xj.imaginary[k]);
                    }
                }
            }
        }
        for cell in &mut self.slot_gram {
            for i in 0..n {
                for j in 0..i {
                    cell[i][j] = cell[j][i];
                }
            }
        }
    }

    /// Whether object `o`'s gains are better stepped than ramped this frame,
    /// and at which slot: the decoder holds the last frame's gains before it
    /// and this frame's from it on. Weighed in the QMF domain — the decoder's
    /// own model of the frame, before synthesis — over the frame and half
    /// the next, against the ramp: each the least-squares gains for its
    /// shape, the ramp from the gains held, the step after the held ones.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn steep_split(&mut self, o: usize) -> Option<Step> {
        let n = self.channels;
        for (rhs, energy) in self.slot_rhs.iter_mut().zip(&mut self.slot_energy) {
            *rhs = [0.0; MAX_CHANNELS];
            *energy = 0.0;
        }
        for t in 0..SPAN {
            let target = &self.object_slots[o][t];
            for k in 0..SUBBANDS {
                let cell = t * BANDS + band_of(k);
                let (or, oi) = (f64::from(target.real[k]), f64::from(target.imaginary[k]));
                self.slot_energy[cell] += or * or + oi * oi;
                for (i, slots) in self.core_slots.iter().enumerate() {
                    let x = &slots[t];
                    self.slot_rhs[cell][i] +=
                        f64::from(x.real[k]) * or + f64::from(x.imaginary[k]) * oi;
                }
            }
        }
        let held = self.held[o];
        let ridge = f64::from(self.ridge);
        let (mut ramped, mut stepped) = (0f64, [0f64; SLOTS + 1]);
        for band in 0..BANDS {
            let h: [f64; MAX_CHANNELS] = std::array::from_fn(|ch| f64::from(held[ch][band]));
            let cell = |t: usize| t * BANDS + band;
            // The ramp: (1 − w)·h + w·c at slot t, w = (t + 1)/24, then c.
            let weight = |t: usize| ((t + 1) as f64 / SLOTS as f64).min(1.0);
            let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
            let mut rhs = [0f64; MAX_CHANNELS];
            for t in 0..SPAN {
                let (w, a, g) = (weight(t), &self.slot_gram[cell(t)], &self.slot_rhs[cell(t)]);
                for i in 0..n {
                    rhs[i] += w * (g[i] - (1.0 - w) * (0..n).map(|j| a[i][j] * h[j]).sum::<f64>());
                    for j in 0..n {
                        system[i][j] += w * w * a[i][j];
                    }
                }
            }
            let c = snap(ridged_solve(system, rhs, n, ridge));
            for t in 0..SPAN {
                let w = weight(t);
                let mix: [f64; MAX_CHANNELS] =
                    std::array::from_fn(|ch| (1.0 - w) * h[ch] + w * c[ch]);
                ramped += error(
                    &self.slot_gram[cell(t)],
                    &self.slot_rhs[cell(t)],
                    self.slot_energy[cell(t)],
                    &mix,
                    n,
                );
            }
            // The steps: the held gains on the slots before the split, the
            // least-squares gains on the rest, the split from late to early so
            // that the rest's sums build up.
            let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
            let (mut rhs, mut energy) = ([0f64; MAX_CHANNELS], 0f64);
            for t in SLOTS..SPAN {
                accumulate(
                    &mut system,
                    &mut rhs,
                    &mut energy,
                    &self.slot_gram[cell(t)],
                    &self.slot_rhs[cell(t)],
                    self.slot_energy[cell(t)],
                    n,
                );
            }
            let mut before = [0f64; SLOTS + 1];
            for t in 0..SLOTS {
                before[t + 1] = before[t]
                    + error(
                        &self.slot_gram[cell(t)],
                        &self.slot_rhs[cell(t)],
                        self.slot_energy[cell(t)],
                        &h,
                        n,
                    );
            }
            for split in (1..=SLOTS).rev() {
                let t = split;
                if t < SLOTS {
                    accumulate(
                        &mut system,
                        &mut rhs,
                        &mut energy,
                        &self.slot_gram[cell(t)],
                        &self.slot_rhs[cell(t)],
                        self.slot_energy[cell(t)],
                        n,
                    );
                }
                let c = ridged_solve(system, rhs, n, ridge);
                stepped[split] += before[split] + error(&system, &rhs, energy, &c, n);
            }
        }
        let (split, _) = (1..=SLOTS)
            .map(|split| (split, stepped[split]))
            .min_by(|a, b| a.1.total_cmp(&b.1))?;
        // The best step weighed again with its gains rounded to the
        // quantiser's steps, as the decoder will have them: a gain between
        // two steps looks better reached at once than ramped to, until both
        // round to the step already held.
        let mut best = 0.0;
        for band in 0..BANDS {
            let (error, gains) = self.step_error(band, split, &held, ridge);
            best += error;
            for (ch, gain) in gains.iter().enumerate().take(n) {
                self.step_gains[ch][band] = *gain as f32;
            }
        }
        // And it must buy back a share of the object's own energy: where
        // both errors are down at the quantiser's, their ratio is noise.
        let step = Step {
            split,
            ramped,
            signal: self.slot_energy.iter().sum(),
        };
        step.passes(best, self.steep_margin).then_some(step)
    }

    /// The error in `band` of the object [`Self::steep_split`] last weighed,
    /// stepped at `split`: the held gains before it, the least-squares gains
    /// from it on, rounded to the quantiser's steps — and those gains.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn step_error(
        &self,
        band: usize,
        split: usize,
        held: &Matrix,
        ridge: f64,
    ) -> (f64, [f64; MAX_CHANNELS]) {
        let n = self.channels;
        let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
        let (mut rhs, mut energy) = ([0f64; MAX_CHANNELS], 0f64);
        for t in split..SPAN {
            let cell = t * BANDS + band;
            accumulate(
                &mut system,
                &mut rhs,
                &mut energy,
                &self.slot_gram[cell],
                &self.slot_rhs[cell],
                self.slot_energy[cell],
                n,
            );
        }
        let c = snap(ridged_solve(system, rhs, n, ridge));
        (
            self.held_error(band, split, held) + error(&system, &rhs, energy, &c, n),
            c,
        )
    }

    /// The error in `band` of the object [`Self::steep_split`] last weighed,
    /// stepped at `split` to `gains`.
    fn stepped_error(&self, band: usize, split: usize, held: &Matrix, gains: &Matrix) -> f64 {
        let n = self.channels;
        let g: [f64; MAX_CHANNELS] = std::array::from_fn(|ch| f64::from(gains[ch][band]));
        let after: f64 = (split..SPAN)
            .map(|t| {
                let cell = t * BANDS + band;
                error(
                    &self.slot_gram[cell],
                    &self.slot_rhs[cell],
                    self.slot_energy[cell],
                    &g,
                    n,
                )
            })
            .sum();
        self.held_error(band, split, held) + after
    }

    /// The error in `band`, on the slots before `split`, of the held gains.
    fn held_error(&self, band: usize, split: usize, held: &Matrix) -> f64 {
        let n = self.channels;
        let h: [f64; MAX_CHANNELS] = std::array::from_fn(|ch| f64::from(held[ch][band]));
        (0..split)
            .map(|t| {
                let cell = t * BANDS + band;
                error(
                    &self.slot_gram[cell],
                    &self.slot_rhs[cell],
                    self.slot_energy[cell],
                    &h,
                    n,
                )
            })
            .sum()
    }

    /// Keep the solution within what the quantiser holds: the least-squares
    /// gains under the bounds, by the primal active-set method for a box. A
    /// gain clamped by itself would leave the others fitted to a partner it
    /// no longer has, and one held for good can be wrong once others move —
    /// so gains are held one at a time, where a step towards the refit
    /// crosses a bound first, and released when the fit would gain by moving
    /// one back inside. It ends where no free gain is past a bound and no held
    /// one wants to leave its own, the optimum over the box. Nothing to do,
    /// and nothing done, when the unbounded solution is inside.
    fn hold_to_bounds(&mut self, m: usize) {
        // The codes −96..=95; a little past them a gain still rounds inside,
        // so only a solution past that margin starts the search.
        let step = f64::from(STEP);
        let (low, high) = (-96.0 * step, 95.0 * step);
        if self.solution[..m]
            .iter()
            .all(|x| (low - step / 2.0..=high + step / 2.0).contains(x))
        {
            return;
        }
        // A feasible start: the solution clamped, the clamped ones held.
        for (x, held) in self.solution.iter_mut().zip(&mut self.bounded).take(m) {
            *held = !(low..=high).contains(x);
            *x = x.clamp(low, high);
        }
        let mut trial = [0f64; MAX_CHANNELS * BANDS];
        // Each round holds or releases one gain; the bound keeps a degenerate
        // case from cycling.
        for _ in 0..4 * m {
            // The free gains' fit with the held ones where they are.
            self.free.clear();
            self.free.extend((0..m).filter(|&i| !self.bounded[i]));
            let k = self.free.len();
            for (a, &i) in self.free.iter().enumerate() {
                for (b, &j) in self.free.iter().enumerate() {
                    self.reduced[a * k + b] = self.gram[i * m + j];
                }
                trial[a] = self.rhs[i]
                    - (0..m)
                        .filter(|&j| self.bounded[j])
                        .map(|j| self.gram[i * m + j] * self.solution[j])
                        .sum::<f64>();
            }
            if k > 0 {
                cholesky(&self.reduced[..k * k], k, &mut self.reduced_factor[..k * k]);
                cholesky_solve(&self.reduced_factor[..k * k], k, &mut trial[..k]);
            }
            // Step towards it as far as the bounds let; the first gain to
            // reach one is held there.
            let mut reach = 1.0;
            let mut blocking = None;
            for (a, &i) in self.free.iter().enumerate() {
                let (from, to) = (self.solution[i], trial[a]);
                let bound = if to > high {
                    high
                } else if to < low {
                    low
                } else {
                    continue;
                };
                let t = ((bound - from) / (to - from)).clamp(0.0, 1.0);
                if t < reach {
                    reach = t;
                    blocking = Some((i, bound));
                }
            }
            for (a, &i) in self.free.iter().enumerate() {
                self.solution[i] += reach * (trial[a] - self.solution[i]);
            }
            if let Some((i, bound)) = blocking {
                self.solution[i] = bound;
                self.bounded[i] = true;
                continue;
            }
            // At the free gains' optimum: release the held gain whose bound
            // costs most, if any — one the error falls by moving inside.
            let mut release = None;
            let mut worst = 0.0;
            for i in (0..m).filter(|&i| self.bounded[i]) {
                let gradient =
                    dot(&self.gram[i * m..(i + 1) * m], &self.solution[..m]) - self.rhs[i];
                // At the upper bound the error falls inward if it rises with
                // the gain; at the lower, if it falls with it.
                let pull = if self.solution[i] >= high {
                    gradient
                } else {
                    -gradient
                };
                let tolerance = 1e-9 * self.gram[i * m + i] * step;
                if pull > tolerance && pull > worst {
                    worst = pull;
                    release = Some(i);
                }
            }
            match release {
                Some(i) => self.bounded[i] = false,
                None => return,
            }
        }
    }

    /// The frame's payload around its objects; the sequence count steps on.
    fn joc(&mut self, objects: Vec<Option<Object>>) -> Joc {
        let joc = Joc {
            downmix_config: self.downmix_config,
            clip_gain: (0, 0),
            sequence: self.sequence,
            objects,
            unflagged_tail: 0,
            padding: 0,
            trailing: vec![],
        };
        self.sequence = next_sequence(self.sequence);
        joc
    }

    /// The last frame's payload, the frame after it taken as silence.
    pub fn finish(&mut self) -> Option<Joc> {
        let silence = vec![0f32; 1536];
        let core: Vec<&[f32]> = vec![&silence; self.channels];
        let objects: Vec<&[f32]> = vec![&silence; self.objects.len()];
        let present = self.pending.clone()?;
        let joc = self.push(&core, &objects, &present);
        self.pending = None;
        joc
    }

    /// The payload of the earlier of the two frames held.
    #[allow(clippy::needless_range_loop)] // an object's index reaches four arrays
    fn estimate(&mut self, present: &[bool]) -> Joc {
        if self.fit == Fit::Synthesis {
            return self.estimate_by_synthesis(present);
        }
        // Which slots, and how each weighs on the downmix's two matrices and
        // on the object's side.
        let window: Vec<(usize, f64, f64, f64)> = match self.fit {
            Fit::Ramp => (0..SLOTS)
                .map(|t| {
                    let w = (t + 1) as f64 / SLOTS as f64;
                    (t, w * w, w * (1.0 - w), w)
                })
                .collect(),
            Fit::Centred | Fit::Synthesis => (SLOTS / 2..SLOTS + SLOTS / 2)
                .map(|t| (t, 1.0, 0.0, 1.0))
                .collect(),
        };

        let n = self.channels;
        let mut a = [[[0f64; MAX_CHANNELS]; MAX_CHANNELS]; BANDS];
        let mut b = [[[0f64; MAX_CHANNELS]; MAX_CHANNELS]; BANDS];
        for &(t, wa, wb, _) in &window {
            for k in 0..SUBBANDS {
                let band = band_of(k);
                for i in 0..n {
                    let (ri, ii) = (
                        f64::from(self.core_slots[i][t].real[k]),
                        f64::from(self.core_slots[i][t].imaginary[k]),
                    );
                    for j in i..n {
                        let x = ri * f64::from(self.core_slots[j][t].real[k])
                            + ii * f64::from(self.core_slots[j][t].imaginary[k]);
                        a[band][i][j] += wa * x;
                        b[band][i][j] += wb * x;
                    }
                }
            }
        }
        for band in 0..BANDS {
            for i in 0..n {
                for j in 0..i {
                    a[band][i][j] = a[band][j][i];
                    b[band][i][j] = b[band][j][i];
                }
            }
        }

        let mut out_objects = Vec::with_capacity(self.objects.len());
        for o in 0..self.objects.len() {
            if !present[o] {
                out_objects.push(None);
                continue;
            }
            let mut g = [[0f64; MAX_CHANNELS]; BANDS];
            for &(t, _, _, wg) in &window {
                let target = &self.object_slots[o][t];
                for k in 0..SUBBANDS {
                    let band = band_of(k);
                    let (or, oi) = (f64::from(target.real[k]), f64::from(target.imaginary[k]));
                    for (i, slots) in self.core_slots.iter().enumerate() {
                        let x = &slots[t];
                        g[band][i] +=
                            wg * (f64::from(x.real[k]) * or + f64::from(x.imaginary[k]) * oi);
                    }
                }
            }

            let held = self.held[o];
            let mut solutions = [[0f64; BANDS]; MAX_CHANNELS];
            for band in 0..BANDS {
                let mut system = a[band];
                let mut rhs = [0f64; MAX_CHANNELS];
                for i in 0..n {
                    rhs[i] = g[band][i]
                        - (0..n)
                            .map(|j| b[band][i][j] * f64::from(held[j][band]))
                            .sum::<f64>();
                }
                let mean_diagonal = (0..n).map(|i| system[i][i]).sum::<f64>() / n as f64;
                let lambda = f64::from(self.ridge) * mean_diagonal + 1e-12;
                for (i, row) in system.iter_mut().enumerate().take(n) {
                    row[i] += lambda;
                }
                let solution = solve(&mut system, &mut rhs, n);
                for ch in 0..n {
                    solutions[ch][band] = solution[ch];
                }
            }
            let (codes, gains) = quantise(n, |ch, band| solutions[ch][band]);
            self.held[o] = gains;
            out_objects.push(Some(Object {
                bands_index: BANDS_INDEX,
                fine: true,
                steep: false,
                offsets: vec![],
                data: Data::Dense(vec![codes]),
            }));
        }

        self.joc(out_objects)
    }
}

/// The subbands of a band.
fn band_range(band: usize) -> std::ops::Range<usize> {
    BAND_START[band]..BAND_START.get(band + 1).copied().unwrap_or(SUBBANDS)
}

/// Gains to the decoder's codes: quantise each to the fine step, and chain
/// each channel's bands — the first against the quantiser's zero, every other
/// against the band before, modulo 192. Returns the codes and the gains the
/// decoder will dequantise them to.
fn quantise(channels: usize, gain: impl Fn(usize, usize) -> f64) -> (Vec<Vec<u16>>, Matrix) {
    let mut codes = vec![vec![0u16; BANDS]; channels];
    let mut gains = [[0f32; BANDS]; MAX_CHANNELS];
    for ch in 0..channels {
        let mut previous_level = 0i32;
        for band in 0..BANDS {
            let q = (gain(ch, band) / f64::from(STEP))
                .round()
                .clamp(-96.0, 95.0) as i32;
            gains[ch][band] = q as f32 * STEP;
            let level = q + 96;
            let code = if band == 0 { q } else { level - previous_level };
            codes[ch][band] = code.rem_euclid(LEVELS) as u16;
            previous_level = level;
        }
    }
    (codes, gains)
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The lower-triangular factor of a symmetric positive-definite `m × m`
/// matrix, row-major, into `l`.
fn cholesky(a: &[f64], m: usize, l: &mut [f64]) {
    for i in 0..m {
        for j in 0..=i {
            let mut sum = a[i * m + j];
            for k in 0..j {
                sum -= l[i * m + k] * l[j * m + k];
            }
            l[i * m + j] = if i == j {
                sum.max(1e-30).sqrt()
            } else {
                sum / l[j * m + j]
            };
        }
    }
}

/// Solve `L Lᵀ x = b` in place, for the factor [`cholesky`] made: `x` holds
/// `b` on the way in and the solution on the way out.
fn cholesky_solve(l: &[f64], m: usize, x: &mut [f64]) {
    for i in 0..m {
        let s: f64 = (0..i).map(|k| l[i * m + k] * x[k]).sum();
        x[i] = (x[i] - s) / l[i * m + i];
    }
    for i in (0..m).rev() {
        let s: f64 = (i + 1..m).map(|k| l[k * m + i] * x[k]).sum();
        x[i] = (x[i] - s) / l[i * m + i];
    }
}

/// The step [`Estimator::steep_split`] chose for an object, and what any
/// gains sent for it must beat: the ramp's error, and the object's energy.
#[derive(Clone, Copy)]
struct Step {
    split: usize,
    ramped: f64,
    signal: f64,
}

impl Step {
    /// Whether a step leaving `error` is worth taking: less than `margin` of
    /// the ramp's error, and a share of the object's energy bought back.
    fn passes(&self, error: f64, margin: f32) -> bool {
        error < f64::from(margin) * self.ramped && self.ramped - error > STEEP_FLOOR * self.signal
    }
}

/// `Σ|o − mixᵀx|²` from the sums `a = Σ Re xxᴴ`, `g = Σ Re x·conj o` and
/// `e = Σ|o|²`.
fn error(
    a: &[[f64; MAX_CHANNELS]; MAX_CHANNELS],
    g: &[f64; MAX_CHANNELS],
    e: f64,
    mix: &[f64; MAX_CHANNELS],
    n: usize,
) -> f64 {
    let mut quadratic = 0.0;
    for i in 0..n {
        quadratic += mix[i] * (0..n).map(|j| a[i][j] * mix[j]).sum::<f64>();
    }
    (e - 2.0 * (0..n).map(|i| mix[i] * g[i]).sum::<f64>() + quadratic).max(0.0)
}

/// Add one slot's sums to running ones.
fn accumulate(
    system: &mut [[f64; MAX_CHANNELS]; MAX_CHANNELS],
    rhs: &mut [f64; MAX_CHANNELS],
    energy: &mut f64,
    a: &[[f64; MAX_CHANNELS]; MAX_CHANNELS],
    g: &[f64; MAX_CHANNELS],
    e: f64,
    n: usize,
) {
    for i in 0..n {
        rhs[i] += g[i];
        for j in 0..n {
            system[i][j] += a[i][j];
        }
    }
    *energy += e;
}

/// Gains as the decoder will have them: on the quantiser's steps, within
/// its codes.
fn snap(gains: [f64; MAX_CHANNELS]) -> [f64; MAX_CHANNELS] {
    let step = f64::from(STEP);
    gains.map(|g| (g / step).round().clamp(-96.0, 95.0) * step)
}

/// [`solve`] with a ridge of `ridge` times the mean diagonal.
fn ridged_solve(
    mut system: [[f64; MAX_CHANNELS]; MAX_CHANNELS],
    mut rhs: [f64; MAX_CHANNELS],
    n: usize,
    ridge: f64,
) -> [f64; MAX_CHANNELS] {
    let lambda = ridge * (0..n).map(|i| system[i][i]).sum::<f64>() / n as f64 + 1e-12;
    for (i, row) in system.iter_mut().enumerate().take(n) {
        row[i] += lambda;
    }
    solve(&mut system, &mut rhs, n)
}

/// The sequence count after `sequence`. Clause 6.3.3.3: nought marks the
/// first frame or the first after a splice, so the count runs to 1023 and
/// then restarts at *one* — a decoder that saw it pass to nought would take
/// the stream for spliced there and drop everything it holds.
pub fn next_sequence(sequence: u16) -> u16 {
    if sequence >= 1023 { 1 } else { sequence + 1 }
}

/// Solve `system · x = rhs` for the leading `n` unknowns, by Gaussian
/// elimination with partial pivoting; the system is overwritten.
#[allow(clippy::needless_range_loop)] // a textbook elimination, indexed as one
fn solve(
    system: &mut [[f64; MAX_CHANNELS]; MAX_CHANNELS],
    rhs: &mut [f64; MAX_CHANNELS],
    n: usize,
) -> [f64; MAX_CHANNELS] {
    for col in 0..n {
        let pivot = (col..n)
            .max_by(|a, b| system[*a][col].abs().total_cmp(&system[*b][col].abs()))
            .expect("rows");
        system.swap(col, pivot);
        rhs.swap(col, pivot);
        let d = system[col][col];
        if d.abs() < 1e-30 {
            continue;
        }
        for row in col + 1..n {
            let f = system[row][col] / d;
            for k in col..n {
                system[row][k] -= f * system[col][k];
            }
            rhs[row] -= f * rhs[col];
        }
    }
    let mut x = [0f64; MAX_CHANNELS];
    for row in (0..n).rev() {
        let d = system[row][row];
        if d.abs() < 1e-30 {
            continue;
        }
        let s: f64 = (row + 1..n).map(|k| system[row][k] * x[k]).sum();
        x[row] = (rhs[row] - s) / d;
    }
    x
}

/// The gains a dense, single-point object's codes stand for, as the decoder
/// dequantises them: per channel and band.
pub fn dequantise(object: &Object, channels: usize) -> Option<Matrix> {
    let Data::Dense(points) = &object.data else {
        return None;
    };
    let point = points.last()?;
    let step = 820.0 / (4096.0 * if object.fine { 2.0 } else { 1.0 });
    let levels = if object.fine { 192 } else { 96 };
    let centre = levels / 2;
    let mut out = [[0f32; BANDS]; MAX_CHANNELS];
    for ch in 0..channels {
        let mut level = centre;
        for (gain, code) in out[ch].iter_mut().zip(&point[ch]) {
            level = (level + i32::from(*code)).rem_euclid(levels);
            *gain = (level - centre) as f32 * step;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    }

    /// A frame of downmix channels and of objects.
    type Frame = (Vec<Vec<f32>>, Vec<Vec<f32>>);

    fn frames(mixes: &[[f32; 5]], count: usize, seed: &mut u32) -> Vec<Frame> {
        (0..count)
            .map(|_| {
                let core: Vec<Vec<f32>> = (0..5)
                    .map(|_| (0..1536).map(|_| noise(seed)).collect())
                    .collect();
                let objects = mixes
                    .iter()
                    .map(|mix| {
                        (0..1536)
                            .map(|n| (0..5).map(|ch| mix[ch] * core[ch][n]).sum())
                            .collect()
                    })
                    .collect();
                (core, objects)
            })
            .collect()
    }

    fn run(estimator: &mut Estimator, frames: &[Frame]) -> Vec<Joc> {
        let mut out = Vec::new();
        for (core, objects) in frames {
            let core_refs: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
            let object_refs: Vec<&[f32]> = objects.iter().map(Vec::as_slice).collect();
            let present = vec![true; objects.len()];
            out.extend(estimator.push(&core_refs, &object_refs, &present));
        }
        out.extend(estimator.finish());
        out
    }

    const MIXES: [[f32; 5]; 3] = [
        [1.0, 0.0, 0.5, 0.0, 0.0],
        [0.0, 0.7, 0.0, -0.3, 0.2],
        [0.25, 0.25, 0.25, 0.25, 0.25],
    ];

    /// Objects that are fixed mixes of the downmix come back as those mixes,
    /// from the first frame, through the payload's bytes and the decoder's
    /// dequantisation — to within half a step.
    #[test]
    fn the_centred_fit_recovers_a_fixed_mix_at_once() {
        let mut seed = 3u32;
        let input = frames(&MIXES, 4, &mut seed);
        let mut estimator = Estimator::new(3, 0);
        estimator.fit = Fit::Centred;
        let payloads = run(&mut estimator, &input);
        assert_eq!(payloads.len(), 4, "one payload a frame, the last at finish");
        for joc in &payloads[..3] {
            let joc = crate::payload::read(&joc.to_bytes()).unwrap();
            for (o, mix) in MIXES.iter().enumerate() {
                let gains = dequantise(joc.objects[o].as_ref().unwrap(), 5).unwrap();
                for (ch, (row, want)) in gains.iter().zip(mix).enumerate() {
                    for (band, gain) in row.iter().enumerate() {
                        assert!(
                            (gain - want).abs() <= STEP / 2.0 + 2e-3,
                            "object {o} channel {ch} band {band}: {gain} for {want}"
                        );
                    }
                }
            }
        }
    }

    /// So does the fit by synthesis, through the decoder's own bank.
    #[test]
    fn the_synthesis_fit_recovers_a_fixed_mix_at_once() {
        let mut seed = 6u32;
        let input = frames(&MIXES, 4, &mut seed);
        let mut estimator = Estimator::new(3, 0);
        assert_eq!(estimator.fit, Fit::Synthesis, "the default");
        let payloads = run(&mut estimator, &input);
        assert_eq!(payloads.len(), 4);
        for (k, joc) in payloads[..3].iter().enumerate() {
            for (o, mix) in MIXES.iter().enumerate() {
                let object = joc.objects[o].as_ref().unwrap();
                // From nothing held the first frames may step at once, rather
                // than ramp up from nought or from gains fitted while the
                // banks filled; after them a steady mix never steps.
                if k < 2 {
                    assert!(
                        !object.steep || object.offsets == [0],
                        "frame {k} object {o}"
                    );
                } else {
                    assert!(!object.steep, "frame {k} object {o}");
                }
                let gains = dequantise(object, 5).unwrap();
                for (ch, (row, want)) in gains.iter().zip(mix).enumerate() {
                    for (band, gain) in row.iter().enumerate() {
                        assert!(
                            (gain - want).abs() <= STEP / 2.0 + 2e-3,
                            "object {o} channel {ch} band {band}: {gain} for {want}"
                        );
                    }
                }
            }
        }
    }

    /// Two downmix channels carrying one signal at very different levels:
    /// the object is the loud one, and every fit must rebuild it at its level.
    /// A ridge that weighs each channel's share by its own energy asks the
    /// quiet one for a gain a hundred times the loud one's, past what the
    /// quantiser holds; clamped there, it rebuilt the object 4.5 dB low.
    #[test]
    fn every_fit_rebuilds_an_object_carried_by_correlated_channels_at_its_level() {
        const QUIET: f32 = 0.01;
        for fit in [Fit::Centred, Fit::Synthesis] {
            let mut seed = 11u32;
            let input: Vec<Frame> = (0..5)
                .map(|_| {
                    let loud: Vec<f32> = (0..1536).map(|_| noise(&mut seed)).collect();
                    let mut core = vec![loud.clone(), loud.iter().map(|v| QUIET * v).collect()];
                    core.extend((0..3).map(|_| (0..1536).map(|_| noise(&mut seed)).collect()));
                    (core, vec![loud])
                })
                .collect();
            let mut estimator = Estimator::new(1, 0);
            estimator.fit = fit;
            let payloads = run(&mut estimator, &input);
            for (frame, joc) in payloads[..4].iter().enumerate() {
                let joc = crate::payload::read(&joc.to_bytes()).unwrap();
                let gains = dequantise(joc.objects[0].as_ref().unwrap(), 5).unwrap();
                for (band, (loud, quiet)) in gains[0].iter().zip(&gains[1]).enumerate() {
                    let level = loud + QUIET * quiet;
                    assert!(
                        (level - 1.0).abs() <= STEP / 2.0 + 2e-3,
                        "{fit:?} frame {frame} band {band}: rebuilt at {level} \
                         (gains {loud} and {quiet})"
                    );
                }
            }
        }
    }

    /// An object louder than the quantiser's largest gain, carried by two
    /// channels, one at half the other's level. The fit leans on the louder
    /// past its bound, 10.4 of 13; held there, at 95 steps, the quieter makes
    /// up the rest and the object is rebuilt at its level. Clamping the one
    /// gain alone, and leaving the other where the unbounded fit put it,
    /// rebuilt it at 12.1.
    #[test]
    fn a_gain_past_the_quantisers_bounds_is_held_there_and_the_rest_refitted() {
        const LEVEL: f32 = 13.0;
        let mut seed = 17u32;
        let input: Vec<Frame> = (0..5)
            .map(|_| {
                let signal: Vec<f32> = (0..1536).map(|_| noise(&mut seed)).collect();
                let mut core = vec![signal.clone(), signal.iter().map(|v| 0.5 * v).collect()];
                core.extend((0..3).map(|_| (0..1536).map(|_| noise(&mut seed)).collect()));
                (core, vec![signal.iter().map(|v| LEVEL * v).collect()])
            })
            .collect();
        let mut estimator = Estimator::new(1, 0);
        let payloads = run(&mut estimator, &input);
        for (frame, joc) in payloads[..4].iter().enumerate() {
            let gains = dequantise(joc.objects[0].as_ref().unwrap(), 5).unwrap();
            for (band, (held, refitted)) in gains[0].iter().zip(&gains[1]).enumerate() {
                assert_eq!(*held, 95.0 * STEP, "frame {frame} band {band}");
                let level = held + 0.5 * refitted;
                assert!(
                    (level - LEVEL).abs() <= STEP / 2.0 + 2e-3,
                    "frame {frame} band {band}: rebuilt at {level} (gains {held} and {refitted})"
                );
            }
        }
    }

    /// Two gains past their bounds at once, where holding both is wrong: a
    /// third channel, C = 0.3·(L − R), lets the object 113.5·L − 118.5·R (in
    /// steps) be carried by many mixes, and the unbounded fit picks one with
    /// L at 95.8 and R at −100.8. Holding R at −96 and refitting brings L
    /// back inside, at 91 with C at 75 — exactly what the quantiser holds.
    /// Holding both, C cannot make it up: it moves L's and R's shares
    /// together, and their sum stayed four steps out.
    #[test]
    fn a_held_gain_is_released_when_the_refit_brings_it_back_inside() {
        let mut seed = 23u32;
        let input: Vec<Frame> = (0..5)
            .map(|_| {
                let l: Vec<f32> = (0..1536).map(|_| noise(&mut seed)).collect();
                let r: Vec<f32> = (0..1536).map(|_| noise(&mut seed)).collect();
                let c: Vec<f32> = l.iter().zip(&r).map(|(l, r)| 0.3 * (l - r)).collect();
                let object = (0..1536)
                    .map(|n| STEP * (85.0 * l[n] - 90.0 * r[n] + 95.0 * c[n]))
                    .collect();
                (
                    vec![l, r, c, vec![0.0; 1536], vec![0.0; 1536]],
                    vec![object],
                )
            })
            .collect();
        let mut estimator = Estimator::new(1, 0);
        let payloads = run(&mut estimator, &input);
        for (frame, joc) in payloads[..4].iter().enumerate() {
            let gains = dequantise(joc.objects[0].as_ref().unwrap(), 5).unwrap();
            let mixes = gains[0].iter().zip(&gains[1]).zip(&gains[2]);
            for (band, ((&l, &r), &c)) in mixes.enumerate() {
                // What the mix gives L and R once C is spread back onto them;
                // three gains each within half a step put these within 0.65.
                let (on_l, on_r) = ((l + 0.3 * c) / STEP, (r - 0.3 * c) / STEP);
                assert!(
                    (on_l - 113.5).abs() <= 0.65 && (on_r + 118.5).abs() <= 0.65,
                    "frame {frame} band {band}: L {on_l:.2} and R {on_r:.2} steps for 113.5 \
                     and −118.5 (gains {:.1} {:.1} {:.1} steps)",
                    l / STEP,
                    r / STEP,
                    c / STEP
                );
            }
        }
    }

    /// An object whose mix switches inside a frame: that frame, and only
    /// that one after the first, steps its gains — at the slot the switch reaches the
    /// subbands, the analysis putting a sample's slot four or so after its
    /// own — to the new mix; with the margin at nought it ramps instead.
    #[test]
    fn a_short_transient_keeps_nonzero_gains_after_a_step() {
        // The object switches from L to twice R inside frame 3, and every
        // downmix channel falls silent two slots later: the step's own
        // window, the output only the slots after it make, then begins after
        // the transient has gone, and a refit there alone came back nought.
        let mut seed = 29u32;
        let switch = 3 * FRAME + 10 * SUBBANDS;
        let end = switch + 2 * SUBBANDS;
        let core: Vec<Vec<f32>> = (0..5)
            .map(|_| {
                (0..7 * FRAME)
                    .map(|t| if t < end { noise(&mut seed) } else { 0.0 })
                    .collect()
            })
            .collect();
        let object: Vec<f32> = (0..7 * FRAME)
            .map(|t| {
                if t < switch {
                    core[0][t]
                } else {
                    2.0 * core[1][t]
                }
            })
            .collect();
        let input: Vec<Frame> = (0..7)
            .map(|k| {
                let frame = |x: &Vec<f32>| x[k * FRAME..(k + 1) * FRAME].to_vec();
                (core.iter().map(frame).collect(), vec![frame(&object)])
            })
            .collect();
        let payloads = run(&mut Estimator::new(1, 0), &input);
        let joc = crate::payload::read(&payloads[3].to_bytes()).unwrap();
        let object = joc.objects[0].as_ref().unwrap();
        assert!(object.steep);
        let gains = dequantise(object, 5).unwrap();
        assert!(
            gains.iter().flatten().any(|&g| g != 0.0),
            "the short transient was refitted to an all-zero matrix"
        );
        // What the transient needs: R at twice, L near nought — in the wide
        // bands. A band a subband or two wide has a few slots of transient
        // after the step to fit five gains on, half of it smeared with the
        // mix before, and scatters.
        for (band, (l, r)) in gains[0].iter().zip(&gains[1]).enumerate().skip(5) {
            assert!(
                (1.0..=2.5).contains(r) && l.abs() < 0.75,
                "band {band}: L {l} and R {r} for nought and two"
            );
        }
    }

    #[test]
    fn a_mix_that_switches_inside_a_frame_steps_there() {
        const FRAMES: usize = 5;
        const SWITCH: usize = 2 * 1536 + 6 * SUBBANDS;
        let mut seed = 29u32;
        let core: Vec<Vec<f32>> = (0..5)
            .map(|_| (0..FRAMES * 1536).map(|_| noise(&mut seed)).collect())
            .collect();
        let object: Vec<f32> = (0..FRAMES * 1536)
            .map(|n| {
                let mix = if n < SWITCH { &MIXES[0] } else { &MIXES[1] };
                (0..5).map(|ch| mix[ch] * core[ch][n]).sum()
            })
            .collect();
        let input: Vec<Frame> = (0..FRAMES)
            .map(|k| {
                let frame = |signal: &Vec<f32>| signal[k * 1536..(k + 1) * 1536].to_vec();
                (core.iter().map(frame).collect(), vec![frame(&object)])
            })
            .collect();
        let payloads = run(&mut Estimator::new(1, 0), &input);
        for (k, joc) in payloads[..FRAMES - 1].iter().enumerate() {
            let joc = crate::payload::read(&joc.to_bytes()).unwrap();
            let object = joc.objects[0].as_ref().unwrap();
            match k {
                0 | 1 => assert!(!object.steep || object.offsets == [0], "frame {k}"),
                _ => assert_eq!(object.steep, k == 2, "frame {k}"),
            }
            if k == 2 {
                let split = usize::from(object.offsets[0]) + 1;
                assert!((8..=12).contains(&split), "stepped at slot {split}");
                let gains = dequantise(object, 5).unwrap();
                for (ch, (row, want)) in gains.iter().zip(&MIXES[1]).enumerate() {
                    for (band, gain) in row.iter().enumerate() {
                        assert!(
                            (gain - want).abs() <= STEP / 2.0 + 2e-3,
                            "channel {ch} band {band}: {gain} for {want}"
                        );
                    }
                }
            }
        }
        let mut never = Estimator::new(1, 0);
        never.steep_margin = 0.0;
        assert!(
            run(&mut never, &input)
                .iter()
                .all(|joc| joc.objects[0].as_ref().is_some_and(|o| !o.steep)),
            "a margin of nought never steps"
        );
    }

    /// The ramp fit overshoots a step in the mix and settles over a few
    /// frames, as the module notes say.
    #[test]
    fn the_ramp_fit_overshoots_then_settles() {
        let mut seed = 4u32;
        let input = frames(&MIXES, 8, &mut seed);
        let mut estimator = Estimator::new(3, 0);
        estimator.fit = Fit::Ramp;
        let payloads = run(&mut estimator, &input);
        let first = dequantise(payloads[0].objects[0].as_ref().unwrap(), 5).unwrap();
        assert!(
            first[0][6] > 1.3,
            "the first frame overshoots: {}",
            first[0][6]
        );
        let settled = dequantise(payloads[6].objects[0].as_ref().unwrap(), 5).unwrap();
        assert!((settled[0][6] - 1.0).abs() < 0.06, "{}", settled[0][6]);
    }

    /// The codes are the decoder's chain: what `dequantise` reads back is
    /// what the estimator says the decoder holds.
    #[test]
    fn the_codes_are_what_the_estimator_holds() {
        let mut seed = 9u32;
        let input = frames(&[[3.0, 0.0, 0.0, 0.0, -2.0]], 2, &mut seed);
        let mut estimator = Estimator::new(1, 0);
        let core_refs: Vec<&[f32]> = input[0].0.iter().map(Vec::as_slice).collect();
        let object_refs: Vec<&[f32]> = input[0].1.iter().map(Vec::as_slice).collect();
        assert!(estimator.push(&core_refs, &object_refs, &[true]).is_none());
        let joc = estimator.finish().unwrap();
        let gains = dequantise(joc.objects[0].as_ref().unwrap(), 5).unwrap();
        assert_eq!(&gains, estimator.held(0));
    }

    /// The count never passes back to nought, which would read as a splice.
    #[test]
    fn the_sequence_skips_nought_when_it_wraps() {
        assert_eq!(next_sequence(0), 1);
        assert_eq!(next_sequence(1022), 1023);
        assert_eq!(next_sequence(1023), 1);
        let mut seed = 1u32;
        let input = frames(&MIXES, 3, &mut seed);
        let mut estimator = Estimator::new(3, 0);
        estimator.set_sequence(1022);
        let payloads = run(&mut estimator, &input);
        let counts: Vec<u16> = payloads.iter().map(|j| j.sequence).collect();
        assert_eq!(counts, [1022, 1023, 1]);
    }

    /// An absent object keeps what the decoder holds.
    #[test]
    fn an_absent_object_keeps_its_gains() {
        let mut seed = 5u32;
        let input = frames(&[[0.0, 0.0, 0.5, 0.0, 0.0]], 3, &mut seed);
        let mut estimator = Estimator::new(1, 0);
        let refs = |i: usize| {
            (
                input[i].0.iter().map(Vec::as_slice).collect::<Vec<_>>(),
                input[i].1.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            )
        };
        let (c0, o0) = refs(0);
        let (c1, o1) = refs(1);
        let (c2, o2) = refs(2);
        estimator.push(&c0, &o0, &[true]);
        estimator.push(&c1, &o1, &[false]).unwrap();
        let held = *estimator.held(0);
        let joc = estimator.push(&c2, &o2, &[true]).unwrap();
        assert!(joc.objects[0].is_none());
        assert_eq!(estimator.held(0), &held);
    }
}
