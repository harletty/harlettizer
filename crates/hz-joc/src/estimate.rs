//! Estimating the parameters: from the downmix and the objects, the matrices
//! that rebuild the objects from the downmix.
//!
//! In the shape every reference stream codes its objects in: twelve parameter
//! bands, fine quantisation, dense coefficients, one data point a frame, the
//! slope smooth. For each object and band the decoder applies one real gain
//! per downmix channel to every QMF subband of the band, and slides from the
//! last frame's gains to this frame's across the frame's 24 slots — at slot
//! `t` it holds `(1 − w)·p + w·c`, `w = (t + 1)/24`, reaching `c` on the last.
//! Two ways to choose `c`, by [`Fit`]:
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
//!
//! Either way the normal equations' matrices depend on the downmix alone and
//! are formed once a frame and band; only the right-hand side is the object's.
//! The least-squares fit is the minimum-mean-square-error reconstruction the
//! joint object coding literature describes (Purnhagen et al., AES 2016); the
//! ramp, the held gains and the quantisation are what make it this decoder's.
//! Which fit to use is measured, not argued: see `docs/joc.md`.

use crate::payload::{Data, Joc, Object};
use crate::qmf::{Analysis, SUBBANDS, Slot};

/// Slots in a frame of six blocks.
pub const SLOTS: usize = 1536 / SUBBANDS;
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
    #[default]
    Centred,
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
    sequence: u16,
    pub fit: Fit,
    /// The ridge on the normal equations, relative to their mean diagonal:
    /// it keeps a band the downmix barely fills from asking for gains the
    /// quantiser cannot hold. 10⁻⁵ unless set: on the reference streams it
    /// put more gains on the reference's step than 10⁻³ did, and 10⁻¹ cost
    /// 13 dB.
    pub ridge: f32,
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
            sequence: 0,
            fit: Fit::default(),
            ridge: 1e-5,
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
        let earlier = self.pending.replace(present.to_vec());
        earlier.map(|present| self.estimate(&present))
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
        // Which slots, and how each weighs on the downmix's two matrices and
        // on the object's side.
        let window: Vec<(usize, f64, f64, f64)> = match self.fit {
            Fit::Ramp => (0..SLOTS)
                .map(|t| {
                    let w = (t + 1) as f64 / SLOTS as f64;
                    (t, w * w, w * (1.0 - w), w)
                })
                .collect(),
            Fit::Centred => (SLOTS / 2..SLOTS + SLOTS / 2)
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
            let mut codes = vec![vec![0u16; BANDS]; n];
            let mut gains = [[0f32; BANDS]; MAX_CHANNELS];
            let mut previous_level = [0i32; MAX_CHANNELS];
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
                    // Quantise, and carry what the decoder will dequantise.
                    let q = (solution[ch] / f64::from(STEP)).round().clamp(-96.0, 95.0) as i32;
                    gains[ch][band] = q as f32 * STEP;
                    // The dense chain: the first band against the quantiser's
                    // zero, every other against the band before, modulo 192.
                    let level = q + 96;
                    let code = if band == 0 {
                        q
                    } else {
                        level - previous_level[ch]
                    };
                    codes[ch][band] = code.rem_euclid(LEVELS) as u16;
                    previous_level[ch] = level;
                }
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

        let joc = Joc {
            downmix_config: self.downmix_config,
            clip_gain: (0, 0),
            sequence: self.sequence,
            objects: out_objects,
            unflagged_tail: 0,
            padding: 0,
            trailing: vec![],
        };
        self.sequence = next_sequence(self.sequence);
        joc
    }
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
