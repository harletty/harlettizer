//! A block's power split into bands the ear would split it into.
//!
//! The K-weighting filter pair of [`crate::kweighting`] says how much a block
//! weighs to a listener as one number. A loudness model needs the same thing
//! per band, because what one sound does to the audibility of another happens
//! band by band: a rumble hides nothing at two kilohertz however loud it is.
//!
//! # The scale
//!
//! The bands are equal steps on the ERB-rate scale of Glasberg and Moore —
//! the number of equivalent rectangular bandwidths of the auditory filter that
//! sit below a frequency — so that every band is the same width *to the ear*,
//! which is wide at the top and narrow at the bottom. How many steps is not
//! decided here: it is a parameter, and `docs/clustering.md` records what
//! each count measured.
//!
//! # How the split is made
//!
//! Not with a filterbank. A block is windowed and transformed once, each bin
//! of the power spectrum is weighted by the K filter's own magnitude at that
//! frequency and summed into the band it falls in. That is the same
//! measurement a bank of K-weighted band filters would make, at a fraction of
//! the cost for any useful number of bands, and it carries no state between
//! blocks — so an analysis is a function of the block alone, and nothing
//! depends on the order the objects are pushed in.
//!
//! The powers are scaled so that they sum to the block's K-weighted mean
//! square, which is the unit the rest of the clustering already speaks.

use crate::fft::Fft;
use crate::kweighting::KWeighting;

/// The ERB-rate scale: how many equivalent rectangular bandwidths of the
/// auditory filter sit below `hz`. Glasberg and Moore, 1990.
pub fn erb_rate(hz: f64) -> f64 {
    21.4 * (4.37e-3 * hz + 1.0).log10()
}

/// The frequency at which the ERB-rate scale reads `rate`: the inverse of
/// [`erb_rate`].
pub fn erb_rate_frequency(rate: f64) -> f64 {
    (10f64.powf(rate / 21.4) - 1.0) / 4.37e-3
}

/// The equivalent rectangular bandwidth of the auditory filter centred at
/// `hz`, in hertz. Glasberg and Moore, 1990.
pub fn erb_width(hz: f64) -> f64 {
    24.7 * (4.37e-3 * hz + 1.0)
}

/// Where the analysis stops. Nothing above it is heard, and the K filter does
/// not roll off on its own.
pub const TOP_HZ: f64 = 20_000.0;

/// A band analysis for blocks of one length at one sample rate.
///
/// Built once for the block length a stream uses, and everything it needs is
/// allocated then: an analysis costs one transform and no allocation.
#[derive(Debug, Clone)]
pub struct Bands {
    fft: Fft,
    frames: usize,
    window: Vec<f64>,
    /// Which band each bin of the spectrum lands in, or `usize::MAX` above
    /// the top.
    band_of: Vec<usize>,
    /// The K filter's power gain at each bin, folded with the normalisation
    /// that makes the bands sum to the block's mean square.
    weight: Vec<f64>,
    /// The band boundaries in hertz, `bands + 1` of them.
    edges: Vec<f64>,
    windowed: Vec<f64>,
    spectrum: Vec<f64>,
}

impl Bands {
    /// An analysis of `frames`-long blocks at `sample_rate`, into `bands`
    /// equal steps of the ERB-rate scale from nought to [`TOP_HZ`] or the
    /// Nyquist frequency, whichever is lower.
    ///
    /// A count of nought is taken as one: the whole audible range as a single
    /// band, which is the K-weighted power and nothing else.
    pub fn new(sample_rate: f64, frames: usize, bands: usize) -> Self {
        let bands = bands.max(1);
        let size = frames.max(2).next_power_of_two();
        let fft = Fft::new(size);

        // A Hann window over the block. Its power sum is what the spectrum is
        // divided by, so that a stationary signal's bands add up to its mean
        // square whatever the window took off the edges.
        let window: Vec<f64> = (0..frames)
            .map(|n| {
                let phase = std::f64::consts::TAU * (n as f64 + 0.5) / frames as f64;
                0.5 - 0.5 * phase.cos()
            })
            .collect();
        let window_power: f64 = window.iter().map(|w| w * w).sum();
        let normalisation = if window_power > 0.0 {
            1.0 / (size as f64 * window_power)
        } else {
            0.0
        };

        let top = TOP_HZ.min(sample_rate / 2.0);
        let low = erb_rate(0.0);
        let high = erb_rate(top);
        let step = (high - low) / bands as f64;
        let edges: Vec<f64> = (0..=bands)
            .map(|band| {
                if band == bands {
                    top
                } else {
                    erb_rate_frequency(low + step * band as f64)
                }
            })
            .collect();

        let k = KWeighting::new(sample_rate);
        let bins = size / 2 + 1;
        let mut band_of = Vec::with_capacity(bins);
        let mut weight = Vec::with_capacity(bins);
        for bin in 0..bins {
            let hz = bin as f64 * sample_rate / size as f64;
            if hz > top {
                band_of.push(usize::MAX);
                weight.push(0.0);
                continue;
            }
            let band = ((erb_rate(hz) - low) / step).floor() as usize;
            band_of.push(band.min(bands - 1));
            // The bins between nought and Nyquist stand for two conjugate
            // halves of the spectrum each; the two ends stand for one.
            let halves = if bin == 0 || bin == size / 2 {
                1.0
            } else {
                2.0
            };
            let gain = k.magnitude(hz, sample_rate);
            weight.push(halves * gain * gain * normalisation);
        }

        Self {
            fft,
            frames,
            window,
            band_of,
            weight,
            edges,
            windowed: vec![0.0; frames],
            spectrum: vec![0.0; bins],
        }
    }

    /// How many bands the analysis splits a block into.
    pub fn bands(&self) -> usize {
        self.edges.len() - 1
    }

    /// The block length this was built for.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The band boundaries in hertz, one more than there are bands.
    pub fn edges(&self) -> &[f64] {
        &self.edges
    }

    /// One block's K-weighted power, per band.
    ///
    /// `block` is the samples, as many as this was built for — a shorter one
    /// is taken as padded with silence, a longer one is cut. `out` is filled,
    /// one entry per band, and the entries sum to the block's K-weighted mean
    /// square.
    pub fn analyse(&mut self, block: &[f64], out: &mut [f64]) {
        debug_assert_eq!(out.len(), self.bands());
        out.fill(0.0);
        if self.frames == 0 {
            return;
        }
        let taken = block.len().min(self.frames);
        for ((slot, sample), window) in self.windowed.iter_mut().zip(block).zip(&self.window) {
            *slot = sample * window;
        }
        self.windowed[taken..].fill(0.0);
        self.fft.power_spectrum(&self.windowed, &mut self.spectrum);
        for ((power, band), weight) in self.spectrum.iter().zip(&self.band_of).zip(&self.weight) {
            if let Some(slot) = out.get_mut(*band) {
                *slot += power * weight;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kweighting::State;

    const RATE: f64 = 48_000.0;

    /// The scale reads what the paper publishes: about forty ERBs below
    /// twenty kilohertz, a hundred and thirty-three hertz wide at one.
    #[test]
    fn the_scale_is_the_published_one() {
        assert!((erb_width(1000.0) - 132.6).abs() < 0.1);
        assert!((erb_rate(1000.0) - 15.6).abs() < 0.1);
        for hz in [50.0, 300.0, 1000.0, 4000.0, 16_000.0] {
            let back = erb_rate_frequency(erb_rate(hz));
            assert!((back - hz).abs() < 1e-6, "{hz} came back as {back}");
        }
        // Monotone, so that a band is an interval.
        assert!(erb_rate(200.0) < erb_rate(201.0));
    }

    /// The bands add up to the K-weighted mean square, which is what makes
    /// them the same unit as the block's plain power.
    #[test]
    fn the_bands_sum_to_the_k_weighted_power() {
        const FRAMES: usize = 1280;
        const RUN_UP: usize = 8;
        // Noise-like: a sum of tones that is not periodic in the block, over
        // a run-up long enough for the filter to have settled by the last
        // block, which is the one that is compared.
        let signal: Vec<f64> = (0..FRAMES * RUN_UP)
            .map(|n| {
                let t = n as f64 / RATE;
                [97.0, 411.0, 1333.0, 3070.0, 7900.0]
                    .iter()
                    .map(|hz| (std::f64::consts::TAU * hz * t).sin())
                    .sum::<f64>()
                    * 0.2
            })
            .collect();
        let block = &signal[FRAMES * (RUN_UP - 1)..];

        // The K-weighted signal through the filter itself, then the same
        // window the analysis uses over its last block: what the analysis
        // measures, made the other way round.
        let k = KWeighting::new(RATE);
        let mut state = (State::default(), State::default());
        let weighted: Vec<f64> = signal.iter().map(|x| k.step(&mut state, *x)).collect();
        let window =
            |n: usize| 0.5 - 0.5 * (std::f64::consts::TAU * (n as f64 + 0.5) / FRAMES as f64).cos();
        let mut power = 0.0;
        let mut window_power = 0.0;
        for (n, y) in weighted[FRAMES * (RUN_UP - 1)..].iter().enumerate() {
            power += (y * window(n)).powi(2);
            window_power += window(n).powi(2);
        }
        let filtered = power / window_power;

        for bands in [1usize, 6, 24] {
            let mut analysis = Bands::new(RATE, FRAMES, bands);
            let mut out = vec![0.0; bands];
            analysis.analyse(block, &mut out);
            let summed: f64 = out.iter().sum();
            // Weighting the spectrum is not quite filtering the signal — the
            // window's leakage is weighted too — and the two agree to a per
            // cent on anything broadband.
            assert!(
                (summed / filtered - 1.0).abs() < 0.02,
                "{bands} bands summed to {summed} against {filtered} filtered"
            );
        }
    }

    /// A tone lands in the band its frequency names, and nowhere much else.
    ///
    /// Tones at the middle of their bands: the window's main lobe is two bins
    /// wide, and a tone within that of an edge leaks into the neighbour,
    /// which is the window and not the split.
    #[test]
    fn a_tone_lands_in_its_band() {
        const FRAMES: usize = 1920;
        let mut analysis = Bands::new(RATE, FRAMES, 12);
        let mut out = vec![0.0; 12];
        for band in [1usize, 4, 7, 10] {
            let edges = analysis.edges();
            let hz = erb_rate_frequency((erb_rate(edges[band]) + erb_rate(edges[band + 1])) / 2.0);
            let block: Vec<f64> = (0..FRAMES)
                .map(|n| (std::f64::consts::TAU * hz * n as f64 / RATE).sin())
                .collect();
            analysis.analyse(&block, &mut out);
            let total: f64 = out.iter().sum();
            let (loudest, share) = out
                .iter()
                .enumerate()
                .map(|(band, power)| (band, power / total))
                .fold((0, 0.0), |m, x| if x.1 > m.1 { x } else { m });
            assert_eq!(loudest, band, "{hz:.0} Hz landed in band {loudest}");
            assert!(
                share > 0.98,
                "{hz:.0} Hz put {share:.3} of itself in its band"
            );
        }
    }

    /// One band is the K-weighted power, to the accuracy the window allows,
    /// and a silent block is silent in every band.
    #[test]
    fn one_band_is_the_whole_and_silence_is_nothing() {
        const FRAMES: usize = 1280;
        let mut one = Bands::new(RATE, FRAMES, 1);
        let mut many = Bands::new(RATE, FRAMES, 16);
        let block: Vec<f64> = (0..FRAMES)
            .map(|n| (std::f64::consts::TAU * 1000.0 * n as f64 / RATE).sin())
            .collect();
        let mut whole = [0.0];
        let mut split = vec![0.0; 16];
        one.analyse(&block, &mut whole);
        many.analyse(&block, &mut split);
        assert!((whole[0] - split.iter().sum::<f64>()).abs() < 1e-9);
        // A kilohertz sine has a mean square of a half, and the K filter is
        // 0.69 dB up there.
        let expected = 0.5 * 10f64.powf(0.069);
        assert!(
            (whole[0] / expected - 1.0).abs() < 0.02,
            "{} against {expected}",
            whole[0]
        );

        one.analyse(&[], &mut whole);
        assert_eq!(whole[0], 0.0);
        many.analyse(&vec![0.0; FRAMES], &mut split);
        assert!(split.iter().all(|p| *p == 0.0));
    }

    /// The bands are the same width on the ERB-rate scale, and the last one
    /// ends where hearing does.
    #[test]
    fn the_bands_are_equal_steps_of_the_scale() {
        let analysis = Bands::new(RATE, 1280, 8);
        let edges = analysis.edges();
        assert_eq!(edges.len(), 9);
        assert_eq!(edges[0], 0.0);
        assert_eq!(edges[8], TOP_HZ);
        let step = erb_rate(TOP_HZ) / 8.0;
        for (band, pair) in edges.windows(2).enumerate() {
            let width = erb_rate(pair[1]) - erb_rate(pair[0]);
            assert!((width - step).abs() < 1e-9, "band {band} is {width} wide");
        }
        // And at a low rate the top is the Nyquist frequency instead.
        let low = Bands::new(32_000.0, 640, 4);
        assert_eq!(*low.edges().last().unwrap(), 16_000.0);
    }
}
