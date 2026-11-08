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

use crate::huffman::{Codebook, Kind};
use crate::payload::{Data, Joc, Object};
use crate::qmf::{Analysis, DELAY, SUBBANDS, Slot, Synthesis};
#[cfg(feature = "parallel")]
use rayon::prelude::*;

// Rayon's iterators with the `parallel` feature, the standard ones without:
// the same loops either way, each step of them the same arithmetic, so the
// same results to the bit.
#[cfg(feature = "parallel")]
macro_rules! each {
    ($v:expr) => {
        $v.par_iter()
    };
}
#[cfg(not(feature = "parallel"))]
macro_rules! each {
    ($v:expr) => {
        $v.iter()
    };
}
#[cfg(feature = "parallel")]
macro_rules! each_mut {
    ($v:expr) => {
        $v.par_iter_mut()
    };
}
#[cfg(not(feature = "parallel"))]
macro_rules! each_mut {
    ($v:expr) => {
        $v.iter_mut()
    };
}
#[cfg(feature = "parallel")]
macro_rules! chunks {
    ($v:expr, $n:expr) => {
        $v.par_chunks($n)
    };
}
#[cfg(not(feature = "parallel"))]
macro_rules! chunks {
    ($v:expr, $n:expr) => {
        $v.chunks($n)
    };
}
#[cfg(feature = "parallel")]
macro_rules! chunks_mut {
    ($v:expr, $n:expr) => {
        $v.par_chunks_mut($n)
    };
}
#[cfg(not(feature = "parallel"))]
macro_rules! chunks_mut {
    ($v:expr, $n:expr) => {
        $v.chunks_mut($n)
    };
}

/// The threads a frame's objects are fitted on.
#[cfg(feature = "parallel")]
fn workers() -> usize {
    rayon::current_num_threads()
}
#[cfg(not(feature = "parallel"))]
fn workers() -> usize {
    1
}

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
/// Sums of the fit's equations taken side by side, each its own: the
/// equations' entries come in runs this long, a row of the basis wide
/// enough to hold whole runs.
const LANES: usize = 8;
/// Rows of the equations worked out at once, sharing what they read of the
/// basis: within a run, so that the rows start in the same one.
const TILE: usize = 2;
const _: () = assert!(LANES.is_multiple_of(TILE) && BANDS.is_multiple_of(TILE));

fn band_of(subband: usize) -> usize {
    BAND_START
        .iter()
        .rposition(|start| *start <= subband)
        .expect("band 0 starts at 0")
}

/// Per downmix channel and band, a gain: one object's matrix.
pub type Matrix = [[f32; BANDS]; MAX_CHANNELS];
/// Per channel and band, unquantised.
type Gains = [[f64; BANDS]; MAX_CHANNELS];

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
    /// weighs the output by, and what the frame's objects share of the fit:
    /// the basis under it, the normal equations and their factor — kept, so
    /// that a frame allocates nothing for them.
    window_root: Vec<f64>,
    /// The basis under the window a sample to a row, [`wide`] long, the
    /// columns past the basis nought.
    weighted: Vec<f64>,
    gram: Vec<f64>,
    factor: Vec<f64>,
    /// For the steep decision: each slot's downmix matrices, per band, over
    /// the frame and half the next.
    slot_gram: Vec<[[f64; MAX_CHANNELS]; MAX_CHANNELS]>,
    /// And each band's systems, from them: the same for every object;
    /// none until a frame weighs a step.
    steep: Vec<SteepBand>,
    /// The scratch each object's fit works in, one per thread the objects
    /// are fitted on at once.
    fitters: Vec<Fitter>,
    /// For the budget: the gains each object's codes were quantised from,
    /// and how much an error in each weighs — the fit's own normal
    /// equations' diagonal.
    wanted: Vec<Gains>,
    weights: Vec<Gains>,
    /// Each object's channels laid out for the budget's search, a frame at
    /// a time, `channels` an object; which of them are present objects'; and
    /// the codes each trades for at the λ tried last.
    trellises: Vec<Trellis>,
    live: Vec<bool>,
    codes: Vec<[u16; BANDS]>,
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
    /// The most bytes the payload the next [`Self::push`] or
    /// [`Self::finish`] returns may take — what a skip field leaves it beside
    /// the frame's other payloads. A payload over it has its gains quantised
    /// again, trading error for bits, until it fits: or, if even gains all
    /// nought do not, as small as a payload of its objects can be. Unlimited
    /// unless set.
    pub budget: usize,
    /// Payloads [`Self::budget`] has made smaller so far.
    squeezed: usize,
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
            weighted: vec![0.0; FRAME * wide(channels * BANDS)],
            gram: vec![0.0; (channels * BANDS).pow(2)],
            factor: vec![0.0; (channels * BANDS).pow(2)],
            slot_gram: vec![[[0.0; MAX_CHANNELS]; MAX_CHANNELS]; SPAN * BANDS],
            steep: Vec::new(),
            fitters: vec![Fitter::new(channels * BANDS)],
            wanted: vec![[[0.0; BANDS]; MAX_CHANNELS]; objects],
            weights: vec![[[0.0; BANDS]; MAX_CHANNELS]; objects],
            trellises: vec![Trellis::default(); objects * channels],
            live: vec![false; objects * channels],
            codes: vec![[0; BANDS]; objects * channels],
            sequence: 0,
            fit: Fit::default(),
            ridge: 1e-5,
            steep_margin: STEEP_MARGIN,
            budget: usize::MAX,
            squeezed: 0,
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
        // Every channel and object through its own bank.
        each_mut!(self.core_slots)
            .zip(each_mut!(self.core).zip(each!(core)))
            .chain(each_mut!(self.object_slots).zip(each_mut!(self.objects).zip(each!(objects))))
            .for_each(|(slots, (bank, signal))| {
                slots.copy_within(SLOTS.., 0);
                for t in 0..SLOTS {
                    bank.slot(
                        &signal[t * SUBBANDS..(t + 1) * SUBBANDS],
                        &mut slots[SLOTS + t],
                    );
                }
            });
        if self.fit == Fit::Synthesis {
            self.synthesise(objects);
        }
        let earlier = self.pending.replace(present.to_vec());
        earlier.map(|present| self.estimate(&present))
    }

    /// Run the new frame through the synthesis basis, and put the objects on
    /// the decoder's clock beside it.
    fn synthesise(&mut self, objects: &[&[f32]]) {
        // Each channel's share of each band through its own bank, `i` being
        // `channel · BANDS + band`.
        let core_slots = &self.core_slots;
        each_mut!(self.basis)
            .zip(each_mut!(self.basis_out))
            .enumerate()
            .for_each(|(i, (bank, out))| {
                let (ch, band) = (i / BANDS, i % BANDS);
                out.copy_within(FRAME.., 0);
                for t in 0..SLOTS {
                    let slot = core_slots[ch][SLOTS + t];
                    let mut share = Slot::default();
                    for k in band_range(band) {
                        share.real[k] = slot.real[k];
                        share.imaginary[k] = slot.imaginary[k];
                    }
                    let at = FRAME + t * SUBBANDS;
                    bank.slot(&share, &mut out[at..at + SUBBANDS]);
                }
            });
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
        let (window_root, basis_out) = (&self.window_root, &self.basis_out);
        chunks_mut!(self.weighted, wide(n * BANDS))
            .zip(each!(window_root))
            .enumerate()
            .for_each(|(t, (row, &root))| {
                for (w, basis) in row.iter_mut().zip(basis_out) {
                    *w = root * f64::from(basis[start + t]);
                }
            });
        synthesis_system(
            &self.weighted,
            0,
            n,
            self.ridge,
            &mut self.gram,
            &mut self.factor,
        );
        let stepping = self.steep_margin > 0.0 && present.contains(&true);
        if stepping {
            self.slot_matrices();
            self.steep_systems();
        }

        // Each object is fitted from what the decoder holds for it, against
        // the downmix's side of the equations the frame's objects share: so
        // apart, a run of them to each thread, each run in its own scratch.
        let objects = present.len();
        let runs = workers().clamp(1, objects.max(1));
        if self.fitters.len() < runs {
            self.fitters.resize_with(runs, || Fitter::new(n * BANDS));
        }
        let run = objects.div_ceil(runs).max(1);
        let frame = Frame {
            channels: n,
            ridge: self.ridge,
            steep_margin: self.steep_margin,
            stepping,
            start,
            weighted: &self.weighted,
            gram: &self.gram,
            factor: &self.factor,
            window_root: &self.window_root,
            slot_gram: &self.slot_gram,
            steep: &self.steep,
            core_slots: &self.core_slots,
        };
        let mut out_objects: Vec<Option<Object>> = (0..objects).map(|_| None).collect();
        chunks_mut!(out_objects, run)
            .zip(chunks!(present, run))
            .zip(chunks_mut!(self.held, run))
            .zip(chunks_mut!(self.wanted, run))
            .zip(chunks_mut!(self.weights, run))
            .zip(chunks!(self.target, run))
            .zip(chunks!(self.object_slots, run))
            .zip(each_mut!(self.fitters))
            .for_each(
                |(((((((out, present), held), wanted), weights), target), slots), fitter)| {
                    for (o, &here) in present.iter().enumerate() {
                        if here {
                            out[o] = Some(fitter.fit(
                                &frame,
                                &target[o],
                                &slots[o],
                                &mut held[o],
                                &mut wanted[o],
                                &mut weights[o],
                            ));
                        }
                    }
                },
            );
        self.joc(out_objects)
    }

    /// Each slot's downmix matrices per band, over the frame and half the
    /// next: what the steep decision weighs every object against.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn slot_matrices(&mut self) {
        let n = self.channels;
        let core_slots = &self.core_slots;
        chunks_mut!(self.slot_gram, BANDS)
            .enumerate()
            .for_each(|(t, cells)| {
                cells.fill([[0.0; MAX_CHANNELS]; MAX_CHANNELS]);
                for k in 0..SUBBANDS {
                    let cell = &mut cells[band_of(k)];
                    for i in 0..n {
                        let xi = &core_slots[i][t];
                        let (ri, ii) = (f64::from(xi.real[k]), f64::from(xi.imaginary[k]));
                        for j in i..n {
                            let xj = &core_slots[j][t];
                            cell[i][j] +=
                                ri * f64::from(xj.real[k]) + ii * f64::from(xj.imaginary[k]);
                        }
                    }
                }
                for cell in cells {
                    for i in 0..n {
                        for j in 0..i {
                            cell[i][j] = cell[j][i];
                        }
                    }
                }
            });
    }

    /// Each band's systems for the steep decision, from the slots' matrices.
    fn steep_systems(&mut self) {
        self.steep.resize_with(BANDS, SteepBand::default);
        let (n, ridge, slot_gram) = (self.channels, f64::from(self.ridge), &self.slot_gram);
        each_mut!(self.steep)
            .enumerate()
            .for_each(|(band, steep)| steep.lay(slot_gram, band, n, ridge));
    }

    /// The frame's payload around its objects; the sequence count steps on.
    fn joc(&mut self, objects: Vec<Option<Object>>) -> Joc {
        let mut joc = Joc {
            downmix_config: self.downmix_config,
            clip_gain: (0, 0),
            sequence: self.sequence,
            objects,
            unflagged_tail: 0,
            padding: 0,
            trailing: vec![],
        };
        if self.budget < usize::MAX && joc.to_bytes().len() > self.budget {
            self.fit_budget(&mut joc);
        }
        self.sequence = next_sequence(self.sequence);
        joc
    }

    /// Bring a payload over the budget under it: every present object's
    /// codes chosen again, channel by channel, for the least error plus λ
    /// times their bits, one λ for the frame — the least that fits, found by
    /// bisection. Codes are differences between neighbouring bands and a
    /// difference of nought costs one bit, so a larger λ flattens the gains
    /// across bands where that costs least; in the limit every code is
    /// nought, the smallest a payload of these objects can be. Only a frame
    /// over its budget pays for this.
    fn fit_budget(&mut self, joc: &mut Joc) {
        let step = f64::from(STEP);
        // Scaled by the objects quantised again: an absent one's weights are
        // whatever its last frame left, and a loud object gone from a quiet
        // frame put every λ the search tried past the useful ones.
        let scale = self
            .weights
            .iter()
            .zip(&joc.objects)
            .filter(|(_, object)| object.is_some())
            .flat_map(|(weights, _)| weights.iter().flatten())
            .fold(0f64, |m, &w| m.max(w))
            .max(1e-30)
            * step
            * step;
        // What each present object's trade does not owe λ, laid out once for
        // every λ tried: each object's channels apart from every other's.
        let n = self.channels;
        let (objects, wanted, weights) = (&joc.objects, &self.wanted, &self.weights);
        each_mut!(self.trellises)
            .zip(each_mut!(self.live))
            .enumerate()
            .for_each(|(i, (trellis, live))| {
                let (o, ch) = (i / n, i % n);
                *live = objects[o].is_some();
                if *live {
                    trellis.lay(&wanted[o][ch], &weights[o][ch]);
                }
            });
        let mut codes = std::mem::take(&mut self.codes);
        let fits = |estimator: &Self, joc: &mut Joc, codes: &mut [[u16; BANDS]], lambda: f64| {
            estimator.requantise(joc, codes, lambda);
            joc.to_bytes().len() <= estimator.budget
        };
        // A λ that fits and a quarter of it that does not, each way from the
        // scale — at most to where every code is nought, beyond which nothing
        // gets smaller, or to where the least λ tried still fits — then the
        // least that fits between them, halving the ratio.
        let (mut low, mut high) = (scale / 4.0, scale);
        for _ in 0..40 {
            if !fits(self, joc, &mut codes, high) {
                (low, high) = (high, high * 4.0);
            } else if fits(self, joc, &mut codes, low) {
                (low, high) = (low / 4.0, low);
            } else {
                break;
            }
        }
        for _ in 0..24 {
            let middle = (low * high).sqrt();
            if fits(self, joc, &mut codes, middle) {
                high = middle;
            } else {
                low = middle;
            }
        }
        self.requantise(joc, &mut codes, high);
        self.codes = codes;
        self.squeezed += 1;
        for (held, object) in self.held.iter_mut().zip(&joc.objects) {
            if let Some(gains) = object.as_ref().and_then(|o| dequantise(o, n)) {
                *held = gains;
            }
        }
    }

    /// Every present object's codes for the trade of error against bits at
    /// `lambda`, each of its channels traded in `codes` apart from every
    /// other's.
    fn requantise(&self, joc: &mut Joc, codes: &mut [[u16; BANDS]], lambda: f64) {
        let prices = Prices::new(lambda);
        each_mut!(codes)
            .zip(each!(self.trellises).zip(each!(self.live)))
            .for_each(|(codes, (trellis, &live))| {
                if live {
                    *codes = trellis.trade(&prices);
                }
            });
        let n = self.channels;
        for (object, codes) in joc.objects.iter_mut().zip(codes.chunks(n)) {
            let Some(object) = object else {
                continue;
            };
            match &mut object.data {
                // Written over where the last λ's are, which are the codes'
                // own shape.
                Data::Dense(points)
                    if points.len() == 1
                        && points[0].len() == n
                        && points[0].iter().all(|kept| kept.len() == BANDS) =>
                {
                    for (kept, codes) in points[0].iter_mut().zip(codes) {
                        kept.copy_from_slice(codes);
                    }
                }
                data => *data = Data::Dense(vec![codes.iter().map(|c| c.to_vec()).collect()]),
            }
        }
    }

    /// How many payloads [`Self::budget`] has made smaller so far.
    pub fn squeezed(&self) -> usize {
        self.squeezed
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
                let solution = Eliminated::new(system, n).solve(rhs, n);
                for ch in 0..n {
                    solutions[ch][band] = solution[ch];
                }
            }
            let (codes, gains) = quantise(n, |ch, band| solutions[ch][band]);
            for ch in 0..n {
                self.wanted[o][ch] = solutions[ch];
                self.weights[o][ch] = std::array::from_fn(|band| a[band][ch][ch]);
            }
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

/// What a frame's fits by synthesis share: the downmix's side of the normal
/// equations and their factor, the basis they are formed from, and the slots
/// the steep decision weighs.
struct Frame<'a> {
    channels: usize,
    ridge: f32,
    steep_margin: f32,
    stepping: bool,
    /// Where the window starts in the two frames of output.
    start: usize,
    weighted: &'a [f64],
    gram: &'a [f64],
    factor: &'a [f64],
    window_root: &'a [f64],
    slot_gram: &'a [[[f64; MAX_CHANNELS]; MAX_CHANNELS]],
    steep: &'a [SteepBand],
    core_slots: &'a [[Slot; 2 * SLOTS]],
}

/// What the steep decision weighs every object against in one band, the
/// same for each: the ramp's system, eliminated under the ridge, and for
/// each split the step's — the sum of the slots' matrices from it on, and
/// that eliminated under the ridge. Worked out once a frame, where each
/// object worked out its own: only the right-hand sides are the object's.
#[derive(Clone, Default)]
struct SteepBand {
    ramp: Eliminated,
    /// By split, from one; nought is no step.
    steps: [[[f64; MAX_CHANNELS]; MAX_CHANNELS]; SLOTS + 1],
    stepped: [Eliminated; SLOTS + 1],
}

impl SteepBand {
    /// Band `band`'s systems from `slot_gram`, each slot's downmix matrices,
    /// summed slot by slot in the order each object's were: the ramp's from
    /// the first slot, the steps' from the last.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn lay(
        &mut self,
        slot_gram: &[[[f64; MAX_CHANNELS]; MAX_CHANNELS]],
        band: usize,
        n: usize,
        ridge: f64,
    ) {
        let cell = |t: usize| &slot_gram[t * BANDS + band];
        let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
        for t in 0..SPAN {
            let (w, a) = (ramp_weight(t), cell(t));
            for i in 0..n {
                for j in 0..n {
                    system[i][j] += w * w * a[i][j];
                }
            }
        }
        self.ramp = Eliminated::ridged(system, n, ridge);
        let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
        for t in SLOTS..SPAN {
            add(&mut system, cell(t), n);
        }
        for split in (1..=SLOTS).rev() {
            if split < SLOTS {
                add(&mut system, cell(split), n);
            }
            self.steps[split] = system;
            self.stepped[split] = Eliminated::ridged(system, n, ridge);
        }
    }
}

/// The ramp's weight on the frame's gains at slot `t`, `(t + 1)/24`, then
/// one: the decoder's `(1 − w)·held + w·new`.
fn ramp_weight(t: usize) -> f64 {
    ((t + 1) as f64 / SLOTS as f64).min(1.0)
}

/// The fit by synthesis's normal equations on the window from `from`
/// samples into it, ridged, into `gram`, and their factor into `factor`.
fn synthesis_system(
    weighted: &[f64],
    from: usize,
    n: usize,
    ridge: f32,
    gram: &mut [f64],
    factor: &mut [f64],
) {
    let m = n * BANDS;
    let rows = window(weighted, from, m);
    // The upper triangle a few rows at a time, a run of their entries at
    // once from the run the diagonal is in; then the lower from it.
    chunks_mut!(gram[..m * m], TILE * m)
        .enumerate()
        .for_each(|(tile, out)| {
            let i = tile * TILE;
            for run in (i / LANES * LANES..m).step_by(LANES) {
                let sums = sums(rows, wide(m), run, |_, sample| {
                    std::array::from_fn::<_, TILE, _>(|r| sample[i + r])
                });
                for (r, (row, sums)) in out.chunks_exact_mut(m).zip(&sums).enumerate() {
                    let (first, end) = (run.max(i + r), (run + LANES).min(m));
                    row[first..end].copy_from_slice(&sums[first - run..end - run]);
                }
            }
        });
    for i in 0..m {
        for j in i + 1..m {
            gram[j * m + i] = gram[i * m + j];
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
        let lambda = f64::from(ridge) * mean + 1e-12;
        for ch in 0..n {
            gram[(ch * BANDS + band) * (m + 1)] += lambda;
        }
    }
    cholesky(gram, m, factor);
}

/// The scratch one object's fit by synthesis works in — the target under
/// the window, a right-hand side and its solution, the smaller system left
/// when gains are held at the quantiser's bounds, the equations of a refit
/// after a step, and the steep decision's side of the object — kept, so
/// that a frame allocates nothing for them.
struct Fitter {
    weighted_target: Vec<f64>,
    rhs: Vec<f64>,
    solution: Vec<f64>,
    bounded: Vec<bool>,
    free: Vec<usize>,
    reduced: Vec<f64>,
    reduced_factor: Vec<f64>,
    steep_gram: Vec<f64>,
    steep_factor: Vec<f64>,
    slot_rhs: Vec<[f64; MAX_CHANNELS]>,
    slot_energy: Vec<f64>,
    /// The gains the steep decision found for the step it chose.
    step_gains: Matrix,
}

impl Fitter {
    /// For `m` gains: a downmix channel's in each band.
    fn new(m: usize) -> Self {
        Self {
            weighted_target: vec![0.0; FRAME],
            rhs: vec![0.0; m],
            solution: vec![0.0; m],
            bounded: vec![false; m],
            free: Vec::with_capacity(m),
            reduced: vec![0.0; m * m],
            reduced_factor: vec![0.0; m * m],
            steep_gram: vec![0.0; m * m],
            steep_factor: vec![0.0; m * m],
            slot_rhs: vec![[0.0; MAX_CHANNELS]; SPAN * BANDS],
            slot_energy: vec![0.0; SPAN * BANDS],
            step_gains: [[0.0; BANDS]; MAX_CHANNELS],
        }
    }

    /// An object's payload for the frame: its gains fitted to `target`, the
    /// object on the decoder's clock over the two frames, from `held`, what
    /// the decoder holds for it, which they replace; `wanted` and `weights`
    /// take what the budget needs to quantise them again.
    fn fit(
        &mut self,
        frame: &Frame,
        target: &[f32],
        slots: &[Slot; 2 * SLOTS],
        held: &mut Matrix,
        wanted: &mut Gains,
        weights: &mut Gains,
    ) -> Object {
        let (n, start) = (frame.channels, frame.start);
        for ((w, &root), &value) in self
            .weighted_target
            .iter_mut()
            .zip(frame.window_root)
            .zip(&target[start..start + FRAME])
        {
            *w = root * f64::from(value);
        }
        self.solve(frame, frame.gram, frame.factor, 0);
        let step = if frame.stepping {
            self.steep_split(frame, slots, held)
        } else {
            None
        };
        let split = step.map(|step| step.split);
        if let Some(split) = split {
            // Stepped at `split`: refitted on the output only the slots
            // from there on make — a slot's synthesis reaches nine slots
            // past it — or on all of the window if that starts later.
            let from = (split * SUBBANDS + DELAY - 1).saturating_sub(start);
            if from > 0 {
                let (mut gram, mut factor) = (
                    std::mem::take(&mut self.steep_gram),
                    std::mem::take(&mut self.steep_factor),
                );
                synthesis_system(frame.weighted, from, n, frame.ridge, &mut gram, &mut factor);
                self.solve(frame, &gram, &factor, from);
                (self.steep_gram, self.steep_factor) = (gram, factor);
            }
        }
        let x = &self.solution;
        let (mut codes, mut gains) = quantise(n, |ch, band| x[ch * BANDS + band]);
        for (ch, wanted) in wanted.iter_mut().enumerate().take(n) {
            *wanted = std::array::from_fn(|band| x[ch * BANDS + band]);
        }
        if let Some(step) = step {
            // The gains sent are held to the test the step passed: the
            // refit on the output after it can find nothing there to fit
            // — a transient over before its window begins — and come
            // back nought. Then the decision's own gains are sent, fitted
            // on every slot from the step on.
            let error = (0..BANDS)
                .map(|band| self.stepped_error(frame, band, step.split, held, &gains))
                .sum();
            if !step.passes(error, frame.steep_margin) {
                let found = self.step_gains;
                (codes, gains) = quantise(n, |ch, band| f64::from(found[ch][band]));
                for (wanted, found) in wanted.iter_mut().zip(&found) {
                    *wanted = found.map(f64::from);
                }
            }
        }
        let m = n * BANDS;
        for (ch, weights) in weights.iter_mut().enumerate().take(n) {
            *weights = std::array::from_fn(|band| frame.gram[(ch * BANDS + band) * (m + 1)]);
        }
        *held = gains;
        Object {
            bands_index: BANDS_INDEX,
            fine: true,
            steep: split.is_some(),
            // The decoder switches at the slot one past the code.
            offsets: split.map(|s| vec![(s - 1) as u8]).unwrap_or_default(),
            data: Data::Dense(vec![codes]),
        }
    }

    /// The gains for the weighted target on the window from `from` samples
    /// into it, under the quantiser's bounds, into `solution`, by the
    /// normal equations `gram` and their factor `factor`.
    fn solve(&mut self, frame: &Frame, gram: &[f64], factor: &[f64], from: usize) {
        let m = frame.channels * BANDS;
        against(
            frame.weighted,
            from,
            m,
            &self.weighted_target,
            &mut self.rhs,
        );
        self.solution.copy_from_slice(&self.rhs);
        cholesky_solve(factor, m, &mut self.solution);
        self.hold_to_bounds(gram, m);
    }

    /// Whether object `o`'s gains are better stepped than ramped this frame,
    /// and at which slot: the decoder holds the last frame's gains before it
    /// and this frame's from it on. Weighed in the QMF domain — the decoder's
    /// own model of the frame, before synthesis — over the frame and half
    /// the next, against the ramp: each the least-squares gains for its
    /// shape, the ramp from the gains held, the step after the held ones.
    fn steep_split(
        &mut self,
        frame: &Frame,
        slots: &[Slot; 2 * SLOTS],
        held: &Matrix,
    ) -> Option<Step> {
        let n = frame.channels;
        let (ramped, stepped) = self.weigh(frame, slots, held);
        let held = *held;
        let ridge = f64::from(frame.ridge);
        let (split, _) = (1..=SLOTS)
            .map(|split| (split, stepped[split]))
            .min_by(|a, b| a.1.total_cmp(&b.1))?;
        // The best step weighed again with its gains rounded to the
        // quantiser's steps, as the decoder will have them: a gain between
        // two steps looks better reached at once than ramped to, until both
        // round to the step already held.
        let mut best = 0.0;
        for band in 0..BANDS {
            let (error, gains) = self.step_error(frame, band, split, &held, ridge);
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
        step.passes(best, frame.steep_margin).then_some(step)
    }

    /// The object's error over the frame and half the next, ramped, and
    /// stepped at each split from one: its sums against the downmix, slot
    /// by slot, kept for the step chosen.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn weigh(
        &mut self,
        frame: &Frame,
        slots: &[Slot; 2 * SLOTS],
        held: &Matrix,
    ) -> (f64, [f64; SLOTS + 1]) {
        let n = frame.channels;
        for (rhs, energy) in self.slot_rhs.iter_mut().zip(&mut self.slot_energy) {
            *rhs = [0.0; MAX_CHANNELS];
            *energy = 0.0;
        }
        for t in 0..SPAN {
            let target = &slots[t];
            for k in 0..SUBBANDS {
                let cell = t * BANDS + band_of(k);
                let (or, oi) = (f64::from(target.real[k]), f64::from(target.imaginary[k]));
                self.slot_energy[cell] += or * or + oi * oi;
                for (i, core) in frame.core_slots.iter().enumerate() {
                    let x = &core[t];
                    self.slot_rhs[cell][i] +=
                        f64::from(x.real[k]) * or + f64::from(x.imaginary[k]) * oi;
                }
            }
        }
        let (mut ramped, mut stepped) = (0f64, [0f64; SLOTS + 1]);
        for band in 0..BANDS {
            // The band's systems, worked out for the frame: none worked out
            // is a bug, not a frame without steps.
            let steep = &frame.steep[band];
            let h: [f64; MAX_CHANNELS] = std::array::from_fn(|ch| f64::from(held[ch][band]));
            let cell = |t: usize| t * BANDS + band;
            // The ramp: (1 − w)·h + w·c at slot t, w = (t + 1)/24, then c.
            // Its system is the band's; the right-hand side is the object's.
            let mut rhs = [0f64; MAX_CHANNELS];
            for t in 0..SPAN {
                let (w, a, g) = (
                    ramp_weight(t),
                    &frame.slot_gram[cell(t)],
                    &self.slot_rhs[cell(t)],
                );
                for i in 0..n {
                    rhs[i] += w * (g[i] - (1.0 - w) * (0..n).map(|j| a[i][j] * h[j]).sum::<f64>());
                }
            }
            let c = snap(steep.ramp.solve(rhs, n));
            for t in 0..SPAN {
                let w = ramp_weight(t);
                let mix: [f64; MAX_CHANNELS] =
                    std::array::from_fn(|ch| (1.0 - w) * h[ch] + w * c[ch]);
                ramped += error(
                    &frame.slot_gram[cell(t)],
                    &self.slot_rhs[cell(t)],
                    self.slot_energy[cell(t)],
                    &mix,
                    n,
                );
            }
            // The steps: the held gains on the slots before the split, the
            // least-squares gains on the rest, the split from late to early so
            // that the rest's sums build up — the systems the band's, built
            // up the same way.
            let (mut rhs, mut energy) = ([0f64; MAX_CHANNELS], 0f64);
            for t in SLOTS..SPAN {
                take(
                    &mut rhs,
                    &mut energy,
                    &self.slot_rhs[cell(t)],
                    self.slot_energy[cell(t)],
                    n,
                );
            }
            let mut before = [0f64; SLOTS + 1];
            for t in 0..SLOTS {
                before[t + 1] = before[t]
                    + error(
                        &frame.slot_gram[cell(t)],
                        &self.slot_rhs[cell(t)],
                        self.slot_energy[cell(t)],
                        &h,
                        n,
                    );
            }
            let mut rest = [([0f64; MAX_CHANNELS], 0f64); SLOTS + 1];
            for split in (1..=SLOTS).rev() {
                if split < SLOTS {
                    take(
                        &mut rhs,
                        &mut energy,
                        &self.slot_rhs[cell(split)],
                        self.slot_energy[cell(split)],
                        n,
                    );
                }
                rest[split] = (rhs, energy);
            }
            // Each split's gains and error apart from every other's, a few
            // splits side by side: one's solve does not wait on the last's.
            for first in (1..=SLOTS).step_by(SIDE) {
                let gains = Eliminated::solve_side_by_side::<SIDE>(
                    std::array::from_fn(|s| &steep.stepped[first + s]),
                    std::array::from_fn(|s| rest[first + s].0),
                    n,
                );
                for (s, c) in gains.iter().enumerate() {
                    let split = first + s;
                    let (rhs, energy) = &rest[split];
                    stepped[split] +=
                        before[split] + error(&steep.steps[split], rhs, *energy, c, n);
                }
            }
        }
        (ramped, stepped)
    }

    /// The error in `band` of the object [`Self::steep_split`] last weighed,
    /// stepped at `split`: the held gains before it, the least-squares gains
    /// from it on, rounded to the quantiser's steps — and those gains.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn step_error(
        &self,
        frame: &Frame,
        band: usize,
        split: usize,
        held: &Matrix,
        ridge: f64,
    ) -> (f64, [f64; MAX_CHANNELS]) {
        let n = frame.channels;
        let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
        let (mut rhs, mut energy) = ([0f64; MAX_CHANNELS], 0f64);
        for t in split..SPAN {
            let cell = t * BANDS + band;
            accumulate(
                &mut system,
                &mut rhs,
                &mut energy,
                &frame.slot_gram[cell],
                &self.slot_rhs[cell],
                self.slot_energy[cell],
                n,
            );
        }
        let c = snap(ridged_solve(system, rhs, n, ridge));
        (
            self.held_error(frame, band, split, held) + error(&system, &rhs, energy, &c, n),
            c,
        )
    }

    /// The error in `band` of the object [`Self::steep_split`] last weighed,
    /// stepped at `split` to `gains`.
    fn stepped_error(
        &self,
        frame: &Frame,
        band: usize,
        split: usize,
        held: &Matrix,
        gains: &Matrix,
    ) -> f64 {
        let n = frame.channels;
        let g: [f64; MAX_CHANNELS] = std::array::from_fn(|ch| f64::from(gains[ch][band]));
        let after: f64 = (split..SPAN)
            .map(|t| {
                let cell = t * BANDS + band;
                error(
                    &frame.slot_gram[cell],
                    &self.slot_rhs[cell],
                    self.slot_energy[cell],
                    &g,
                    n,
                )
            })
            .sum();
        self.held_error(frame, band, split, held) + after
    }

    /// The error in `band`, on the slots before `split`, of the held gains.
    fn held_error(&self, frame: &Frame, band: usize, split: usize, held: &Matrix) -> f64 {
        let n = frame.channels;
        let h: [f64; MAX_CHANNELS] = std::array::from_fn(|ch| f64::from(held[ch][band]));
        (0..split)
            .map(|t| {
                let cell = t * BANDS + band;
                error(
                    &frame.slot_gram[cell],
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
    fn hold_to_bounds(&mut self, gram: &[f64], m: usize) {
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
                    self.reduced[a * k + b] = gram[i * m + j];
                }
                trial[a] = self.rhs[i]
                    - (0..m)
                        .filter(|&j| self.bounded[j])
                        .map(|j| gram[i * m + j] * self.solution[j])
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
                let gradient = dot(&gram[i * m..(i + 1) * m], &self.solution[..m]) - self.rhs[i];
                // At the upper bound the error falls inward if it rises with
                // the gain; at the lower, if it falls with it.
                let pull = if self.solution[i] >= high {
                    gradient
                } else {
                    -gradient
                };
                let tolerance = 1e-9 * gram[i * m + i] * step;
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
}

/// The subbands of a band.
fn band_range(band: usize) -> std::ops::Range<usize> {
    BAND_START[band]..BAND_START.get(band + 1).copied().unwrap_or(SUBBANDS)
}

/// Levels a band's code may take in the trade: its own, six steps either
/// side, two either side of the gains' weighted mean, and nought.
const NEAR: i32 = 6;
const MOST: usize = 2 * NEAR as usize + 1 + 5 + 1;
/// The most steps two levels can be apart, either way.
const SPREAD: i32 = LEVELS - 1;

/// What each code costs at a λ: λ times its bits in the fine codebook, for
/// every step a band's level can be from the band before's, and the first
/// band's from nought — worked out once for each λ the budget's search
/// tries, not once for each code it weighs.
struct Prices([f64; PRICES]);
/// The prices' room: the `2 · SPREAD + 1` steps, to a power of two, so that
/// a step's index needs no bounds check — masking one in range changes
/// nothing.
const PRICES: usize = 512;
const _: () = assert!(PRICES > 2 * SPREAD as usize);

impl Prices {
    fn new(lambda: f64) -> Self {
        let book = Codebook::get(Kind::Matrix { fine: true });
        let bits = |code: i32| book.length(code.rem_euclid(LEVELS) as u16) as f64;
        Self(std::array::from_fn(|i| match i32::try_from(i) {
            Ok(i) if i <= 2 * SPREAD => lambda * bits(i - SPREAD),
            _ => f64::NAN,
        }))
    }

    fn of(&self, steps: i32) -> f64 {
        debug_assert!((-SPREAD..=SPREAD).contains(&steps));
        self.0[(steps + SPREAD) as usize & (PRICES - 1)]
    }
}

/// One object's gains on one downmix channel, laid out for the trade of
/// error against bits: each band's candidate levels and the weighted error
/// of each. None of it changes with λ, so it is laid out once a frame, and
/// the budget's search tries λ after λ on it.
#[derive(Clone, Default)]
struct Trellis {
    levels: [[i32; MOST]; BANDS],
    counts: [usize; BANDS],
    errors: [[f64; MOST]; BANDS],
}

impl Trellis {
    fn lay(&mut self, wanted: &[f64; BANDS], weight: &[f64; BANDS]) {
        let step = f64::from(STEP);
        let level = |gain: f64| (gain / step).round().clamp(-96.0, 95.0) as i32;
        let total: f64 = weight.iter().sum();
        let mean = if total > 0.0 {
            level(wanted.iter().zip(weight).map(|(g, w)| g * w).sum::<f64>() / total)
        } else {
            0
        };
        for band in 0..BANDS {
            let own = level(wanted[band]);
            let near = (own - NEAR..=own + NEAR)
                .chain(mean - 2..=mean + 2)
                .chain(0..=0);
            let (levels, count) = (&mut self.levels[band], &mut self.counts[band]);
            *count = 0;
            for q in near.filter(|q| (-96..=95).contains(q)) {
                if !levels[..*count].contains(&q) {
                    levels[*count] = q;
                    self.errors[band][*count] =
                        weight[band] * (f64::from(q) * step - wanted[band]).powi(2);
                    *count += 1;
                }
            }
        }
    }

    /// The channel's fine codes for the least `Σ w·(gain − wanted)² + λ·bits`
    /// over its bands, at `prices`' λ: a Viterbi search over each band's
    /// candidate levels — a few either side of its own, the channel's
    /// weighted mean and nought, the last two so that a flat run, or all
    /// nought, is always there to take. Of costs that tie, the level laid
    /// out first is taken.
    fn trade(&self, prices: &Prices) -> [u16; BANDS] {
        let mut cost = [[0f64; MOST]; BANDS];
        let mut from = [[0u8; MOST]; BANDS];
        let first = self.counts[0];
        for ((cost, &error), &level) in cost[0][..first]
            .iter_mut()
            .zip(&self.errors[0][..first])
            .zip(&self.levels[0][..first])
        {
            *cost = error + prices.of(level);
        }
        for band in 1..BANDS {
            let (done, rest) = cost.split_at_mut(band);
            let count = self.counts[band - 1];
            let (before, costs) = (&self.levels[band - 1][..count], &done[band - 1][..count]);
            for s in 0..self.counts[band] {
                let q = self.levels[band][s];
                let mut best = costs[0] + prices.of(q - before[0]);
                let mut at = 0;
                for p in 1..count {
                    let tried = costs[p] + prices.of(q - before[p]);
                    if tried.total_cmp(&best).is_lt() {
                        best = tried;
                        at = p;
                    }
                }
                rest[0][s] = best + self.errors[band][s];
                from[band][s] = at as u8;
            }
        }
        let last = BANDS - 1;
        let mut s = 0;
        for t in 1..self.counts[last] {
            if cost[last][t].total_cmp(&cost[last][s]).is_lt() {
                s = t;
            }
        }
        let mut levels = [0i32; BANDS];
        for band in (0..BANDS).rev() {
            levels[band] = self.levels[band][s];
            s = usize::from(from[band][s]);
        }
        std::array::from_fn(|band| {
            let code = if band == 0 {
                levels[0]
            } else {
                levels[band] - levels[band - 1]
            };
            code.rem_euclid(LEVELS) as u16
        })
    }
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

/// The width of a row of `m` signals a sample, in whole runs of [`LANES`].
fn wide(m: usize) -> usize {
    m.next_multiple_of(LANES)
}

/// The rows of a basis of `m` signals ([`wide`]) from sample `from` of the
/// window on.
fn window(weighted: &[f64], from: usize, m: usize) -> &[f64] {
    &weighted[from * wide(m)..]
}

/// The right-hand side of the fit by synthesis on the window from `from`
/// samples into it: each of the basis's `m` signals against `target`, into
/// `rhs`.
fn against(weighted: &[f64], from: usize, m: usize, target: &[f64], rhs: &mut [f64]) {
    let (rows, target) = (window(weighted, from, m), &target[from..]);
    for run in (0..m).step_by(LANES) {
        let [sums] = sums(rows, wide(m), run, |t, _| [target[t]]);
        let end = (run + LANES).min(m);
        rhs[run..end].copy_from_slice(&sums[..end - run]);
    }
}

/// The [`LANES`] columns of `rows` from `run`, each times each of the `R`
/// values `by` takes of its row — from the row's sample and the row —
/// summed down the rows: each sum its own, its products taken and added in
/// the order [`dot`] takes them, and from where it starts, so that each is
/// the dot of the two to the bit. Side by side, the sums do not wait on each
/// other, and the `R` share each run they read.
#[inline(always)]
fn sums<const R: usize>(
    rows: &[f64],
    wide: usize,
    run: usize,
    by: impl Fn(usize, &[f64]) -> [f64; R],
) -> [[f64; LANES]; R] {
    let mut sums = [[std::iter::empty::<f64>().sum::<f64>(); LANES]; R];
    for (t, row) in rows.chunks_exact(wide).enumerate() {
        let column: &[f64; LANES] = row[run..run + LANES].try_into().expect("whole runs");
        for (sums, a) in sums.iter_mut().zip(by(t, row)) {
            for (sum, &b) in sums.iter_mut().zip(column) {
                *sum += a * b;
            }
        }
    }
    sums
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
    add(system, a, n);
    take(rhs, energy, g, e, n);
}

/// Add one slot's downmix matrix to a running sum.
#[allow(clippy::needless_range_loop)] // matrices indexed as matrices
fn add(
    system: &mut [[f64; MAX_CHANNELS]; MAX_CHANNELS],
    a: &[[f64; MAX_CHANNELS]; MAX_CHANNELS],
    n: usize,
) {
    for i in 0..n {
        for j in 0..n {
            system[i][j] += a[i][j];
        }
    }
}

/// Add one slot's object side to running sums.
fn take(
    rhs: &mut [f64; MAX_CHANNELS],
    energy: &mut f64,
    g: &[f64; MAX_CHANNELS],
    e: f64,
    n: usize,
) {
    for (rhs, g) in rhs.iter_mut().zip(g).take(n) {
        *rhs += g;
    }
    *energy += e;
}

/// Gains as the decoder will have them: on the quantiser's steps, within
/// its codes.
fn snap(gains: [f64; MAX_CHANNELS]) -> [f64; MAX_CHANNELS] {
    let step = f64::from(STEP);
    gains.map(|g| (g / step).round().clamp(-96.0, 95.0) * step)
}

/// `system · x = rhs` for the leading `n` unknowns, under a ridge of
/// `ridge` times the mean diagonal.
fn ridged_solve(
    system: [[f64; MAX_CHANNELS]; MAX_CHANNELS],
    rhs: [f64; MAX_CHANNELS],
    n: usize,
    ridge: f64,
) -> [f64; MAX_CHANNELS] {
    Eliminated::ridged(system, n, ridge).solve(rhs, n)
}

/// The sequence count after `sequence`. Clause 6.3.3.3: nought marks the
/// first frame or the first after a splice, so the count runs to 1023 and
/// then restarts at *one* — a decoder that saw it pass to nought would take
/// the stream for spliced there and drop everything it holds.
pub fn next_sequence(sequence: u16) -> u16 {
    if sequence >= 1023 { 1 } else { sequence + 1 }
}

/// A small system's Gaussian elimination with partial pivoting, worked out
/// once and kept, so that every right-hand side solved against the system
/// is taken through its steps without working them out again: the row each
/// column's pivot came from and, in one matrix, the system left on and
/// above the diagonal, and below it the factor each row was eliminated by —
/// row `r` under column `c` numbered as it was when `c` was eliminated. A
/// right-hand side meets the same steps, in the same order, as it would
/// eliminated beside the system: to the bit what that gives.
#[derive(Clone, Copy, Default)]
struct Eliminated {
    pivots: [u8; MAX_CHANNELS],
    lu: [[f64; MAX_CHANNELS]; MAX_CHANNELS],
}

impl Eliminated {
    /// The leading `n` rows and columns of `system`, eliminated; a column
    /// whose pivot is below 10⁻³⁰ is left as it is.
    #[allow(clippy::needless_range_loop)] // a textbook elimination, indexed as one
    fn new(mut system: [[f64; MAX_CHANNELS]; MAX_CHANNELS], n: usize) -> Self {
        let mut pivots = [0u8; MAX_CHANNELS];
        let mut factors = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
        for col in 0..n {
            let pivot = (col..n)
                .max_by(|a, b| system[*a][col].abs().total_cmp(&system[*b][col].abs()))
                .expect("rows");
            system.swap(col, pivot);
            pivots[col] = pivot as u8;
            let d = system[col][col];
            if d.abs() < 1e-30 {
                continue;
            }
            for row in col + 1..n {
                let f = system[row][col] / d;
                for k in col..n {
                    system[row][k] -= f * system[col][k];
                }
                factors[row][col] = f;
            }
        }
        // Below the diagonal, nothing elimination left is read again: a row
        // is done with once its column is, and the pivots it meets later
        // swap only rows below their own.
        for row in 0..n {
            system[row][..row].copy_from_slice(&factors[row][..row]);
        }
        Self { pivots, lu: system }
    }

    /// [`Self::new`] under a ridge of `ridge` times the mean diagonal.
    fn ridged(mut system: [[f64; MAX_CHANNELS]; MAX_CHANNELS], n: usize, ridge: f64) -> Self {
        let lambda = ridge * (0..n).map(|i| system[i][i]).sum::<f64>() / n as f64 + 1e-12;
        for (i, row) in system.iter_mut().enumerate().take(n) {
            row[i] += lambda;
        }
        Self::new(system, n)
    }

    /// The system's `x` for the leading `n` unknowns of `rhs`: the
    /// elimination's swaps and steps, then back substitution.
    fn solve(&self, rhs: [f64; MAX_CHANNELS], n: usize) -> [f64; MAX_CHANNELS] {
        let [x] = Self::solve_side_by_side([self], [rhs], n);
        x
    }

    /// [`Self::solve`] for each of `K` systems and right-hand sides, taken
    /// a step of each at a time: each system's steps are its own, in its own
    /// order, but one's division need not wait on another's.
    #[allow(clippy::needless_range_loop)] // a textbook elimination, indexed as one
    fn solve_side_by_side<const K: usize>(
        systems: [&Self; K],
        mut rhs: [[f64; MAX_CHANNELS]; K],
        n: usize,
    ) -> [[f64; MAX_CHANNELS]; K] {
        for col in 0..n {
            for (system, rhs) in systems.iter().zip(&mut rhs) {
                rhs.swap(col, usize::from(system.pivots[col]));
                if system.lu[col][col].abs() < 1e-30 {
                    continue;
                }
                for row in col + 1..n {
                    rhs[row] -= system.lu[row][col] * rhs[col];
                }
            }
        }
        let mut x = [[0f64; MAX_CHANNELS]; K];
        for row in (0..n).rev() {
            for ((system, x), rhs) in systems.iter().zip(&mut x).zip(&rhs) {
                let d = system.lu[row][row];
                if d.abs() < 1e-30 {
                    continue;
                }
                let s: f64 = (row + 1..n).map(|k| system.lu[row][k] * x[k]).sum();
                x[row] = (rhs[row] - s) / d;
            }
        }
        x
    }
}

/// How many splits the steep decision solves side by side: six or eight
/// were no faster than four.
const SIDE: usize = 4;
const _: () = assert!(SLOTS.is_multiple_of(SIDE));

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

    /// Objects of their own noise, which no mix of the downmix carries: their
    /// gains scatter across bands, and their payloads run large.
    fn unruly(objects: usize, frames: usize, seed: &mut u32) -> Vec<Frame> {
        (0..frames)
            .map(|_| {
                let core: Vec<Vec<f32>> = (0..5)
                    .map(|_| (0..1536).map(|_| noise(seed)).collect())
                    .collect();
                let objects = (0..objects)
                    .map(|o| {
                        (0..1536)
                            .map(|n| core[o % 5][n] + 2.0 * noise(seed))
                            .collect()
                    })
                    .collect();
                (core, objects)
            })
            .collect()
    }

    /// Fitted on a pool of any size, every payload is the one a pool of one
    /// thread fits, to the byte, and the decoder is left holding the same
    /// gains — objects stepped, held at the quantiser's bounds, absent, and
    /// payloads over the budget among them.
    #[cfg(feature = "parallel")]
    #[test]
    fn any_number_of_threads_fits_what_one_fits() {
        const OBJECTS: usize = 9;
        let mut seed = 5u32;
        let input: Vec<Frame> = (0..24)
            .map(|f| {
                let core: Vec<Vec<f32>> = (0..5)
                    .map(|_| (0..1536).map(|_| noise(&mut seed)).collect())
                    .collect();
                let objects = (0..OBJECTS)
                    .map(|o| {
                        (0..1536)
                            .map(|n| match o % 3 {
                                // Noise beside a channel: over any budget.
                                0 => core[o % 5][n] + 2.0 * noise(&mut seed),
                                // A mix that switches inside every third
                                // frame: a step.
                                1 if f % 3 == 1 && n >= 700 => core[(o + 1) % 5][n],
                                1 => core[o % 5][n],
                                // Past the quantiser's bounds.
                                _ => 20.0 * core[o % 5][n],
                            })
                            .collect()
                    })
                    .collect();
                (core, objects)
            })
            .collect();
        let fit = |threads: usize, budget: usize| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let mut estimator = Estimator::new(OBJECTS, 0);
                estimator.budget = budget;
                let mut payloads = Vec::new();
                for (f, (core, objects)) in input.iter().enumerate() {
                    let core: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
                    let objects: Vec<&[f32]> = objects.iter().map(Vec::as_slice).collect();
                    // One object away every other frame.
                    let present: Vec<bool> = (0..OBJECTS).map(|o| o != 4 || f % 2 == 0).collect();
                    payloads.extend(
                        estimator
                            .push(&core, &objects, &present)
                            .map(|j| j.to_bytes()),
                    );
                }
                payloads.extend(estimator.finish().map(|j| j.to_bytes()));
                let held: Vec<Matrix> = (0..OBJECTS).map(|o| *estimator.held(o)).collect();
                (payloads, held, estimator.squeezed())
            })
        };
        // A budget a quarter under the largest payload: most frames over it,
        // none down to nought.
        let largest = fit(1, usize::MAX).0.iter().map(Vec::len).max().unwrap();
        for budget in [usize::MAX, largest * 3 / 4] {
            let one = fit(1, budget);
            assert_eq!(
                one.2 > 0,
                budget < usize::MAX,
                "squeezed only under a budget"
            );
            let objects: Vec<Object> = one
                .0
                .iter()
                .flat_map(|bytes| crate::payload::read(bytes).unwrap().objects)
                .flatten()
                .collect();
            assert!(objects.iter().any(|o| o.steep), "no object stepped");
            let bound = 95.0 * STEP;
            assert!(
                objects
                    .iter()
                    .filter_map(|o| dequantise(o, 5))
                    .any(|gains| gains.iter().flatten().any(|g| g.abs() >= bound)),
                "no gain held at a bound"
            );
            for threads in [2, 3, 8] {
                assert!(
                    fit(threads, budget) == one,
                    "{threads} threads, a budget of {budget}"
                );
            }
        }
    }

    /// A payload over its budget comes back under it, still a payload the
    /// decoder reads, its gains the nearest the budget allows; a payload
    /// under it is untouched, byte for byte.
    #[test]
    fn a_payload_over_its_budget_is_brought_under_it() {
        let mut seed = 31u32;
        let input = unruly(8, 5, &mut seed);
        let free = run(&mut Estimator::new(8, 0), &input);
        let sizes: Vec<usize> = free.iter().map(|j| j.to_bytes().len()).collect();
        let largest = *sizes.iter().max().unwrap();
        let budget = largest * 3 / 4;
        assert!(
            sizes.iter().any(|&s| s > budget),
            "something to bring under"
        );
        let mut estimator = Estimator::new(8, 0);
        estimator.budget = budget;
        let held = run(&mut estimator, &input);
        for (k, held) in held.iter().enumerate() {
            let bytes = held.to_bytes();
            assert!(
                bytes.len() <= budget,
                "frame {k}: {} bytes for {budget}",
                bytes.len()
            );
            let read = crate::payload::read(&bytes).unwrap();
            for object in read.objects.iter().flatten() {
                assert!(dequantise(object, 5).is_some());
            }
        }
        // A budget every payload is under leaves every one as it was.
        let mut roomy = Estimator::new(8, 0);
        roomy.budget = largest;
        for (k, (free, roomy)) in free.iter().zip(run(&mut roomy, &input)).enumerate() {
            assert_eq!(free.to_bytes(), roomy.to_bytes(), "frame {k}");
        }
    }

    /// The budget's search is scaled by the objects it quantises again, not
    /// by weights an absent object left behind.
    #[test]
    fn an_absent_objects_old_weights_do_not_set_the_budgets_scale() {
        // Two objects, loud for two frames, then the downmix 100 dB quieter
        // and the second object gone. Its weights from the loud frames stayed
        // behind; taken into the search's scale they put every λ it tried
        // far past the useful ones, and the present object was sent with
        // gains all nought, in a payload half its budget.
        const BUDGET: usize = 30;
        for fit in [Fit::Centred, Fit::Synthesis] {
            let mut seed = 43u32;
            let mut estimator = Estimator::new(2, 0);
            estimator.fit = fit;
            let mut payloads = Vec::new();
            for k in 0..8 {
                let level = if k < 2 { 1.0 } else { 1e-5 };
                let core: Vec<Vec<f32>> = (0..5)
                    .map(|_| (0..1536).map(|_| level * noise(&mut seed)).collect())
                    .collect();
                let objects: Vec<Vec<f32>> = (0..2)
                    .map(|o| {
                        (0..1536)
                            .map(|n| core[o][n] + 2.0 * level * noise(&mut seed))
                            .collect()
                    })
                    .collect();
                if k == 5 {
                    estimator.budget = BUDGET;
                }
                let core: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
                let objects: Vec<&[f32]> = objects.iter().map(Vec::as_slice).collect();
                payloads.extend(estimator.push(&core, &objects, &[true, k < 2]));
            }
            assert!(estimator.squeezed() > 0, "{fit:?}: the budget was needed");
            for (frame, joc) in payloads.iter().enumerate().skip(4) {
                let bytes = joc.to_bytes();
                assert!(bytes.len() <= BUDGET, "{fit:?} frame {frame}");
                let object = joc.objects[0].as_ref().unwrap();
                let gains = dequantise(object, 5).unwrap();
                let mean = gains[0].iter().sum::<f32>() / BANDS as f32;
                assert!(
                    mean > 0.5,
                    "{fit:?} frame {frame}: L's gains average {mean} for one, in {} bytes",
                    bytes.len()
                );
            }
        }
    }

    /// With a budget not even gains all nought fit, the payload is the
    /// smallest these objects can make — every code nought — and no panic.
    #[test]
    fn a_budget_below_the_smallest_payload_gives_the_smallest() {
        let mut seed = 37u32;
        let input = unruly(4, 3, &mut seed);
        let mut estimator = Estimator::new(4, 0);
        estimator.budget = 1;
        for joc in run(&mut estimator, &input) {
            for object in joc.objects.iter().flatten() {
                let Data::Dense(points) = &object.data else {
                    panic!("dense");
                };
                assert!(points[0].iter().flatten().all(|&c| c == 0));
            }
        }
    }

    /// The trade as it was searched before [`Trellis`]: everything laid out
    /// afresh for each λ. Kept to hold the trellis to it.
    ///
    /// A channel's fine codes for the least `Σ w·(gain − wanted)² + λ·bits`
    /// over its bands: a Viterbi search over each band's candidate levels — a
    /// few either side of its own, the channel's weighted mean and nought, the
    /// last two so that a flat run, or all nought, is always there to take.
    fn searched(
        wanted: &[f64; BANDS],
        weight: &[f64; BANDS],
        lambda: f64,
        book: &Codebook,
    ) -> Vec<u16> {
        const NEAR: i32 = 6;
        const MOST: usize = 2 * NEAR as usize + 1 + 5 + 1;
        let step = f64::from(STEP);
        let level = |gain: f64| (gain / step).round().clamp(-96.0, 95.0) as i32;
        let total: f64 = weight.iter().sum();
        let mean = if total > 0.0 {
            level(wanted.iter().zip(weight).map(|(g, w)| g * w).sum::<f64>() / total)
        } else {
            0
        };
        let mut candidates = [[0i32; MOST]; BANDS];
        let mut counts = [0usize; BANDS];
        for band in 0..BANDS {
            let own = level(wanted[band]);
            let near = (own - NEAR..=own + NEAR)
                .chain(mean - 2..=mean + 2)
                .chain(0..=0);
            for q in near.filter(|q| (-96..=95).contains(q)) {
                let list = &mut candidates[band][..counts[band]];
                if !list.contains(&q) {
                    candidates[band][counts[band]] = q;
                    counts[band] += 1;
                }
            }
        }
        let bits = |code: i32| book.length(code.rem_euclid(LEVELS) as u16) as f64;
        let error =
            |band: usize, q: i32| weight[band] * (f64::from(q) * step - wanted[band]).powi(2);
        let mut cost = [[0f64; MOST]; BANDS];
        let mut from = [[0usize; MOST]; BANDS];
        for s in 0..counts[0] {
            let q = candidates[0][s];
            cost[0][s] = error(0, q) + lambda * bits(q);
        }
        for band in 1..BANDS {
            for s in 0..counts[band] {
                let q = candidates[band][s];
                let (best, at) = (0..counts[band - 1])
                    .map(|p| {
                        (
                            cost[band - 1][p] + lambda * bits(q - candidates[band - 1][p]),
                            p,
                        )
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .expect("candidates");
                cost[band][s] = best + error(band, q);
                from[band][s] = at;
            }
        }
        let last = BANDS - 1;
        let mut s = (0..counts[last])
            .min_by(|a, b| cost[last][*a].total_cmp(&cost[last][*b]))
            .expect("candidates");
        let mut levels = [0i32; BANDS];
        for band in (0..BANDS).rev() {
            levels[band] = candidates[band][s];
            s = from[band][s];
        }
        (0..BANDS)
            .map(|band| {
                let code = if band == 0 {
                    levels[0]
                } else {
                    levels[band] - levels[band - 1]
                };
                code.rem_euclid(LEVELS) as u16
            })
            .collect()
    }

    /// The trellis trades as the search before it did, code for code: λ
    /// from nought to past where every code is nought, gains past the
    /// quantiser's range either way, bands that weigh nothing — where every
    /// level costs the same, and the first laid out has to be taken — and
    /// gains on a half step.
    #[test]
    fn the_trellis_trades_as_the_search_did() {
        let mut seed = 77u32;
        let book = Codebook::get(Kind::Matrix { fine: true });
        let step = f64::from(STEP);
        let mut trellis = Trellis::default();
        for case in 0..400 {
            let wanted: [f64; BANDS] = std::array::from_fn(|band| match (case + band) % 5 {
                0 => (f64::from(noise(&mut seed)) * 40.0).round() * step + step / 2.0,
                1 => f64::from(noise(&mut seed)) * 30.0,
                _ => f64::from(noise(&mut seed)) * 4.0,
            });
            let weight: [f64; BANDS] = std::array::from_fn(|band| match (case / 3 + band) % 7 {
                0 => 0.0,
                1 => f64::from(noise(&mut seed)).abs() * 1e-9,
                _ => f64::from(noise(&mut seed)) + 0.6,
            });
            trellis.lay(&wanted, &weight);
            for lambda in [0.0, 1e-9, 1e-4, 3e-3, 0.02, 0.1, 0.7, 4.0, 50.0, 1e6] {
                let lambda = lambda * (1.0 + f64::from(noise(&mut seed)) * 0.01);
                assert_eq!(
                    trellis.trade(&Prices::new(lambda))[..],
                    searched(&wanted, &weight, lambda, book)[..],
                    "case {case}, λ {lambda}"
                );
            }
        }
    }

    /// With nothing asked for bits, the trade is the plain quantisation.
    #[test]
    fn a_trade_at_nought_is_the_plain_quantisation() {
        let mut seed = 41u32;
        for _ in 0..20 {
            let wanted: [f64; BANDS] = std::array::from_fn(|_| f64::from(noise(&mut seed)) * 12.0);
            let weight: [f64; BANDS] = std::array::from_fn(|_| f64::from(noise(&mut seed)) + 0.6);
            let (codes, _) = quantise(1, |_, band| wanted[band]);
            let mut trellis = Trellis::default();
            trellis.lay(&wanted, &weight);
            assert_eq!(trellis.trade(&Prices::new(0.0))[..], codes[0][..]);
        }
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

    /// The system as it was summed before the runs: a dot for each entry,
    /// each signal's samples a row of their own.
    fn system_by_dots(
        weighted: &[Vec<f64>],
        from: usize,
        n: usize,
        ridge: f32,
        gram: &mut [f64],
        factor: &mut [f64],
    ) {
        let m = n * BANDS;
        for i in 0..m {
            for j in i..m {
                gram[i * m + j] = dot(&weighted[i][from..], &weighted[j][from..]);
            }
        }
        for i in 0..m {
            for j in i + 1..m {
                gram[j * m + i] = gram[i * m + j];
            }
        }
        for band in 0..BANDS {
            let mean = (0..n)
                .map(|ch| gram[(ch * BANDS + band) * (m + 1)])
                .sum::<f64>()
                / n as f64;
            let lambda = f64::from(ridge) * mean + 1e-12;
            for ch in 0..n {
                gram[(ch * BANDS + band) * (m + 1)] += lambda;
            }
        }
        cholesky(gram, m, factor);
    }

    /// The fit's sums taken in runs side by side are the dots they replace,
    /// to the bit: the system, its factor and a right-hand side, on every
    /// number of channels and a window from any sample. Two of the signals
    /// are noughts, one of each sign: their products are noughts of a sign
    /// only a sum that starts where `dot`'s does keeps.
    #[test]
    fn the_sums_in_runs_are_the_dots_to_the_bit() {
        let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        let mut seed = 21u32;
        for n in 1..=MAX_CHANNELS {
            let m = n * BANDS;
            let mut signals: Vec<Vec<f64>> = (0..m)
                .map(|_| {
                    (0..FRAME)
                        .map(|_| 3.0 * f64::from(noise(&mut seed)))
                        .collect()
                })
                .collect();
            signals[3] = vec![-0.0; FRAME];
            signals[4] = vec![0.0; FRAME];
            let target: Vec<f64> = (0..FRAME).map(|_| f64::from(noise(&mut seed))).collect();
            let mut weighted = vec![0.0; FRAME * wide(m)];
            for (t, row) in weighted.chunks_exact_mut(wide(m)).enumerate() {
                for (w, signal) in row.iter_mut().zip(&signals) {
                    *w = signal[t];
                }
            }
            for from in [0, 1, 7, 700, FRAME - 1, FRAME] {
                let (mut gram, mut factor) = (vec![0.0; m * m], vec![0.0; m * m]);
                synthesis_system(&weighted, from, n, 1e-3, &mut gram, &mut factor);
                let (mut dots, mut dots_factor) = (vec![0.0; m * m], vec![0.0; m * m]);
                system_by_dots(&signals, from, n, 1e-3, &mut dots, &mut dots_factor);
                assert_eq!(bits(&gram), bits(&dots), "{n} channels from {from}");
                assert_eq!(
                    bits(&factor),
                    bits(&dots_factor),
                    "{n} channels from {from}"
                );

                let mut rhs = vec![0.0; m];
                against(&weighted, from, m, &target, &mut rhs);
                let rhs_by_dots: Vec<f64> = signals
                    .iter()
                    .map(|signal| dot(&signal[from..], &target[from..]))
                    .collect();
                assert_eq!(bits(&rhs), bits(&rhs_by_dots), "{n} channels from {from}");
            }
        }
    }

    /// The small solve as it was: the right-hand side eliminated beside the
    /// system.
    #[allow(clippy::needless_range_loop)] // a textbook elimination, indexed as one
    fn solve_beside(
        mut system: [[f64; MAX_CHANNELS]; MAX_CHANNELS],
        mut rhs: [f64; MAX_CHANNELS],
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

    /// And under the ridge, as it was.
    fn ridged_beside(
        mut system: [[f64; MAX_CHANNELS]; MAX_CHANNELS],
        rhs: [f64; MAX_CHANNELS],
        n: usize,
        ridge: f64,
    ) -> [f64; MAX_CHANNELS] {
        let lambda = ridge * (0..n).map(|i| system[i][i]).sum::<f64>() / n as f64 + 1e-12;
        for (i, row) in system.iter_mut().enumerate().take(n) {
            row[i] += lambda;
        }
        solve_beside(system, rhs, n)
    }

    /// An elimination kept and taken again for each right-hand side, alone
    /// or beside other systems', solves to the bit what eliminating each
    /// beside the system did: on every number of channels, with and without
    /// the ridge, on systems of every kind the decision meets — full,
    /// symmetric, singular, a silent channel's nought row, pivots that tie, a
    /// column below the threshold — and right-hand sides with noughts of
    /// either sign.
    #[test]
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn a_kept_elimination_solves_as_one_beside_the_right_hand_side_to_the_bit() {
        const KINDS: usize = 6;
        let mut seed = 53u32;
        let mut value = || 4.0 * f64::from(noise(&mut seed));
        for n in 1..=MAX_CHANNELS {
            let systems: [_; KINDS] = std::array::from_fn(|kind| {
                let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
                // The Gram matrix of a few vectors: fewer than `n` leaves it
                // singular, as a band the downmix fills from fewer channels.
                let gram = |system: &mut [[f64; MAX_CHANNELS]; MAX_CHANNELS],
                            vectors: &[[f64; MAX_CHANNELS]]| {
                    for i in 0..n {
                        for j in 0..n {
                            system[i][j] = vectors.iter().map(|v| v[i] * v[j]).sum();
                        }
                    }
                };
                let mut vectors = [[0f64; MAX_CHANNELS]; 9];
                for v in &mut vectors {
                    *v = std::array::from_fn(|_| value());
                }
                match kind {
                    0 => {
                        for row in &mut system {
                            *row = std::array::from_fn(|_| value());
                        }
                    }
                    1 => gram(&mut system, &vectors[..n.div_ceil(2)]),
                    2 => gram(&mut system, &vectors),
                    3 => {
                        for v in &mut vectors {
                            v[n / 2] = 0.0;
                        }
                        gram(&mut system, &vectors);
                    }
                    4 => {
                        gram(&mut system, &vectors);
                        if n > 1 {
                            system[n - 1] = system[0].map(|x| -x);
                        }
                    }
                    _ => {
                        gram(&mut system, &vectors);
                        for row in &mut system {
                            row[0] = if row[1] > 0.0 { 1e-31 } else { -0.0 };
                        }
                    }
                }
                system
            });
            for ridged in [false, true] {
                let kept = systems.map(|system| match ridged {
                    false => Eliminated::new(system, n),
                    true => Eliminated::ridged(system, n, 1e-5),
                });
                for r in 0..5 {
                    let rhs: [[f64; MAX_CHANNELS]; KINDS] = std::array::from_fn(|_| match r {
                        3 => {
                            let mut rhs: [f64; MAX_CHANNELS] = std::array::from_fn(|_| value());
                            rhs[0] = -0.0;
                            rhs[n - 1] = 0.0;
                            rhs
                        }
                        // Under a column left as it is, a nought below a
                        // negative stays the nought it is.
                        4 => std::array::from_fn(|i| if i % 2 == 0 { -1.0 } else { -0.0 }),
                        _ => std::array::from_fn(|_| value()),
                    });
                    let beside: [_; KINDS] = std::array::from_fn(|kind| match ridged {
                        false => solve_beside(systems[kind], rhs[kind], n),
                        true => ridged_beside(systems[kind], rhs[kind], n, 1e-5),
                    });
                    let bits = |x: &[f64; MAX_CHANNELS]| x.map(f64::to_bits);
                    for kind in 0..KINDS {
                        assert_eq!(
                            bits(&kept[kind].solve(rhs[kind], n)),
                            bits(&beside[kind]),
                            "{n} channels, kind {kind}, right-hand side {r}, ridged {ridged}"
                        );
                    }
                    let side_by_side = Eliminated::solve_side_by_side(kept.each_ref(), rhs, n);
                    assert_eq!(
                        side_by_side.each_ref().map(bits),
                        beside.each_ref().map(bits),
                        "{n} channels side by side, right-hand side {r}, ridged {ridged}"
                    );
                    // As many side by side as the decision takes, every kind
                    // in every place.
                    for first in 0..KINDS {
                        let kind = |s: usize| (first + s) % KINDS;
                        let as_the_decision_takes_them = Eliminated::solve_side_by_side::<SIDE>(
                            std::array::from_fn(|s| &kept[kind(s)]),
                            std::array::from_fn(|s| rhs[kind(s)]),
                            n,
                        );
                        assert_eq!(
                            as_the_decision_takes_them.each_ref().map(bits),
                            std::array::from_fn::<_, SIDE, _>(|s| bits(&beside[kind(s)])),
                            "{n} channels, {SIDE} side by side from kind {first}, \
                             right-hand side {r}, ridged {ridged}"
                        );
                    }
                }
            }
        }
        // A column left as it is takes nothing from the rows below it, not
        // even a nought: one taken would turn the next row's nought, and
        // that row's gain, to the other sign.
        let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
        (system[0][0], system[1][1], system[2][2]) = (1e-31, 1.0, 1.0);
        let rhs = [-1.0, -0.0, -0.0, 0.0, 0.0, 0.0, 0.0];
        let beside = solve_beside(system, rhs, 3).map(f64::to_bits);
        assert_eq!(beside[1], (-0.0f64).to_bits());
        let kept = Eliminated::new(system, 3);
        assert_eq!(kept.solve(rhs, 3).map(f64::to_bits), beside);
        assert_eq!(
            Eliminated::solve_side_by_side([&kept; 2], [rhs; 2], 3).map(|x| x.map(f64::to_bits)),
            [beside; 2]
        );
    }

    /// A band's systems as each object summed them for itself, before they
    /// were the band's: the ramp's, and by split the steps'.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn systems_alone(
        slot_gram: &[[[f64; MAX_CHANNELS]; MAX_CHANNELS]],
        band: usize,
        n: usize,
    ) -> (
        [[f64; MAX_CHANNELS]; MAX_CHANNELS],
        Vec<[[f64; MAX_CHANNELS]; MAX_CHANNELS]>,
    ) {
        let cell = |t: usize| t * BANDS + band;
        let weight = |t: usize| ((t + 1) as f64 / SLOTS as f64).min(1.0);
        let mut ramp = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
        for t in 0..SPAN {
            let (w, a) = (weight(t), &slot_gram[cell(t)]);
            for i in 0..n {
                for j in 0..n {
                    ramp[i][j] += w * w * a[i][j];
                }
            }
        }
        let mut steps = vec![[[0f64; MAX_CHANNELS]; MAX_CHANNELS]; SLOTS + 1];
        let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
        let add = |system: &mut [[f64; MAX_CHANNELS]; MAX_CHANNELS], t: usize| {
            let a = &slot_gram[cell(t)];
            for i in 0..n {
                for j in 0..n {
                    system[i][j] += a[i][j];
                }
            }
        };
        for t in SLOTS..SPAN {
            add(&mut system, t);
        }
        for split in (1..=SLOTS).rev() {
            let t = split;
            if t < SLOTS {
                add(&mut system, t);
            }
            steps[split] = system;
        }
        (ramp, steps)
    }

    /// The systems the steep decision shares out to a frame's objects are
    /// each object's own to the bit, and solve as each object's solved:
    /// on five and seven channels, with a silent channel, two carrying one
    /// signal, and one that starts mid-frame.
    #[test]
    #[allow(clippy::needless_range_loop)] // splits indexed as splits
    fn the_bands_systems_are_each_objects_own_to_the_bit() {
        let mut seed = 59u32;
        for config in [0, 1] {
            let mut estimator = Estimator::new(1, config);
            let n = estimator.channels();
            for frame in 0..2 {
                let mut core: Vec<Vec<f32>> = (0..n)
                    .map(|_| (0..FRAME).map(|_| noise(&mut seed)).collect())
                    .collect();
                core[1].fill(0.0);
                core[2] = core[0].clone();
                if frame == 1 {
                    core[3][..FRAME / 3].fill(0.0);
                }
                let object: Vec<f32> = (0..FRAME).map(|_| noise(&mut seed)).collect();
                let core_refs: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
                estimator.push(&core_refs, &[&object], &[true]);
            }
            estimator.slot_matrices();
            estimator.steep_systems();
            let ridge = f64::from(estimator.ridge);
            for band in 0..BANDS {
                let steep = &estimator.steep[band];
                let (ramp, steps) = systems_alone(&estimator.slot_gram, band, n);
                let bits = |s: &[[f64; MAX_CHANNELS]; MAX_CHANNELS]| s.map(|r| r.map(f64::to_bits));
                for r in 0..3 {
                    let rhs: [f64; MAX_CHANNELS] =
                        std::array::from_fn(|_| f64::from(noise(&mut seed)));
                    assert_eq!(
                        steep.ramp.solve(rhs, n).map(f64::to_bits),
                        ridged_beside(ramp, rhs, n, ridge).map(f64::to_bits),
                        "{n} channels, band {band}, the ramp, right-hand side {r}"
                    );
                    for split in 1..=SLOTS {
                        assert_eq!(
                            bits(&steep.steps[split]),
                            bits(&steps[split]),
                            "{n} channels, band {band}, split {split}"
                        );
                        assert_eq!(
                            steep.stepped[split].solve(rhs, n).map(f64::to_bits),
                            ridged_beside(steps[split], rhs, n, ridge).map(f64::to_bits),
                            "{n} channels, band {band}, split {split}, right-hand side {r}"
                        );
                    }
                }
            }
        }
    }

    /// An object's ramp and steps weighed as each object weighed them for
    /// itself, before the systems were the band's.
    #[allow(clippy::needless_range_loop)] // matrices indexed as matrices
    fn weigh_alone(
        frame: &super::Frame,
        slots: &[Slot; 2 * SLOTS],
        held: &Matrix,
    ) -> (f64, [f64; SLOTS + 1], Vec<[f64; MAX_CHANNELS]>, Vec<f64>) {
        let n = frame.channels;
        let mut slot_rhs = vec![[0.0; MAX_CHANNELS]; SPAN * BANDS];
        let mut slot_energy = vec![0.0; SPAN * BANDS];
        for t in 0..SPAN {
            let target = &slots[t];
            for k in 0..SUBBANDS {
                let cell = t * BANDS + band_of(k);
                let (or, oi) = (f64::from(target.real[k]), f64::from(target.imaginary[k]));
                slot_energy[cell] += or * or + oi * oi;
                for (i, core) in frame.core_slots.iter().enumerate() {
                    let x = &core[t];
                    slot_rhs[cell][i] += f64::from(x.real[k]) * or + f64::from(x.imaginary[k]) * oi;
                }
            }
        }
        let accumulate = |system: &mut [[f64; MAX_CHANNELS]; MAX_CHANNELS],
                          rhs: &mut [f64; MAX_CHANNELS],
                          energy: &mut f64,
                          cell: usize| {
            let (a, g, e) = (&frame.slot_gram[cell], &slot_rhs[cell], slot_energy[cell]);
            for i in 0..n {
                rhs[i] += g[i];
                for j in 0..n {
                    system[i][j] += a[i][j];
                }
            }
            *energy += e;
        };
        let ridge = f64::from(frame.ridge);
        let (mut ramped, mut stepped) = (0f64, [0f64; SLOTS + 1]);
        for band in 0..BANDS {
            let h: [f64; MAX_CHANNELS] = std::array::from_fn(|ch| f64::from(held[ch][band]));
            let cell = |t: usize| t * BANDS + band;
            let weight = |t: usize| ((t + 1) as f64 / SLOTS as f64).min(1.0);
            let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
            let mut rhs = [0f64; MAX_CHANNELS];
            for t in 0..SPAN {
                let (w, a, g) = (weight(t), &frame.slot_gram[cell(t)], &slot_rhs[cell(t)]);
                for i in 0..n {
                    rhs[i] += w * (g[i] - (1.0 - w) * (0..n).map(|j| a[i][j] * h[j]).sum::<f64>());
                    for j in 0..n {
                        system[i][j] += w * w * a[i][j];
                    }
                }
            }
            let c = snap(ridged_beside(system, rhs, n, ridge));
            for t in 0..SPAN {
                let w = weight(t);
                let mix: [f64; MAX_CHANNELS] =
                    std::array::from_fn(|ch| (1.0 - w) * h[ch] + w * c[ch]);
                ramped += error(
                    &frame.slot_gram[cell(t)],
                    &slot_rhs[cell(t)],
                    slot_energy[cell(t)],
                    &mix,
                    n,
                );
            }
            let mut system = [[0f64; MAX_CHANNELS]; MAX_CHANNELS];
            let (mut rhs, mut energy) = ([0f64; MAX_CHANNELS], 0f64);
            for t in SLOTS..SPAN {
                accumulate(&mut system, &mut rhs, &mut energy, cell(t));
            }
            let mut before = [0f64; SLOTS + 1];
            for t in 0..SLOTS {
                before[t + 1] = before[t]
                    + error(
                        &frame.slot_gram[cell(t)],
                        &slot_rhs[cell(t)],
                        slot_energy[cell(t)],
                        &h,
                        n,
                    );
            }
            for split in (1..=SLOTS).rev() {
                let t = split;
                if t < SLOTS {
                    accumulate(&mut system, &mut rhs, &mut energy, cell(t));
                }
                let c = ridged_beside(system, rhs, n, ridge);
                stepped[split] += before[split] + error(&system, &rhs, energy, &c, n);
            }
        }
        (ramped, stepped, slot_rhs, slot_energy)
    }

    /// Each object's ramp and every one of its steps weigh, against the
    /// systems the band shares out, what they weighed against the object's
    /// own, to the bit — and its sums against the downmix, which the step
    /// chosen is weighed again from, are what they were: on five and seven
    /// channels, under two ridges, for an object of noise, a fixed mix, a mix
    /// that switches mid-frame and silence, from gains held on the
    /// quantiser's steps and from noughts.
    #[test]
    fn each_objects_steps_weigh_what_they_weighed_to_the_bit() {
        let mut seed = 61u32;
        for config in [0, 1] {
            let mut estimator = Estimator::new(4, config);
            let n = estimator.channels();
            for _ in 0..2 {
                let mut core: Vec<Vec<f32>> = (0..n)
                    .map(|_| (0..FRAME).map(|_| noise(&mut seed)).collect())
                    .collect();
                core[1].fill(0.0);
                core[2] = core[0].clone();
                let objects: Vec<Vec<f32>> = vec![
                    (0..FRAME).map(|_| noise(&mut seed)).collect(),
                    (0..FRAME).map(|t| 0.5 * core[0][t] - core[3][t]).collect(),
                    (0..FRAME)
                        .map(|t| {
                            if t < FRAME / 3 {
                                core[0][t]
                            } else {
                                core[4][t]
                            }
                        })
                        .collect(),
                    vec![0.0; FRAME],
                ];
                let core_refs: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
                let object_refs: Vec<&[f32]> = objects.iter().map(Vec::as_slice).collect();
                estimator.push(&core_refs, &object_refs, &[true; 4]);
            }
            estimator.slot_matrices();
            for ridge in [estimator.ridge, 1e-3] {
                estimator.ridge = ridge;
                estimator.steep_systems();
                let frame = super::Frame {
                    channels: n,
                    ridge: estimator.ridge,
                    steep_margin: estimator.steep_margin,
                    stepping: true,
                    start: 0,
                    weighted: &estimator.weighted,
                    gram: &estimator.gram,
                    factor: &estimator.factor,
                    window_root: &estimator.window_root,
                    slot_gram: &estimator.slot_gram,
                    steep: &estimator.steep,
                    core_slots: &estimator.core_slots,
                };
                let mut fitter = Fitter::new(n * BANDS);
                for (o, slots) in estimator.object_slots.iter().enumerate() {
                    for from_noughts in [false, true] {
                        let held: Matrix = std::array::from_fn(|_| {
                            std::array::from_fn(|_| match from_noughts {
                                false => (noise(&mut seed) * 40.0).round() * STEP,
                                true => 0.0,
                            })
                        });
                        let (ramped, stepped) = fitter.weigh(&frame, slots, &held);
                        let (ramped_alone, stepped_alone, rhs_alone, energy_alone) =
                            weigh_alone(&frame, slots, &held);
                        let case = format!(
                            "{n} channels, ridge {ridge}, object {o}, from noughts {from_noughts}"
                        );
                        assert_eq!(ramped.to_bits(), ramped_alone.to_bits(), "{case}");
                        assert_eq!(
                            stepped.map(f64::to_bits),
                            stepped_alone.map(f64::to_bits),
                            "{case}"
                        );
                        let bits = |v: &[[f64; MAX_CHANNELS]]| -> Vec<_> {
                            v.iter().map(|r| r.map(f64::to_bits)).collect()
                        };
                        assert_eq!(bits(&fitter.slot_rhs), bits(&rhs_alone), "{case}");
                        assert_eq!(
                            fitter
                                .slot_energy
                                .iter()
                                .map(|e| e.to_bits())
                                .collect::<Vec<_>>(),
                            energy_alone.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
                            "{case}"
                        );
                    }
                }
            }
        }
    }
}
