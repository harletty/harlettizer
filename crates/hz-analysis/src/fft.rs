//! A radix-2 FFT, private to this crate.
//!
//! `hz-analysis` has no dependencies on purpose — it is measurement code that
//! runs over whole programmes and has no business pulling a graph of crates
//! in — and the speech gate needs a spectrum. What it needs is small: one
//! transform size, chosen once, real input, forward direction only. The
//! inverse it needs is the inverse of a real *even* sequence, which the
//! forward transform already computes (see [`Fft::real_even`]).

/// An in-place iterative Cooley-Tukey FFT with its tables precomputed.
#[derive(Debug, Clone)]
pub struct Fft {
    size: usize,
    /// `cos`/`sin` of `-2πk/size` for `k < size/2`.
    twiddles: Vec<(f64, f64)>,
    /// Bit-reversal permutation, as pairs to swap.
    swaps: Vec<(u32, u32)>,
    /// Scratch, so a transform allocates nothing.
    re: Vec<f64>,
    im: Vec<f64>,
}

impl Fft {
    /// # Panics
    /// If `size` is not a power of two, or is smaller than two.
    pub fn new(size: usize) -> Self {
        assert!(size >= 2 && size.is_power_of_two(), "FFT size must be 2^n");

        let twiddles = (0..size / 2)
            .map(|k| {
                let angle = -2.0 * std::f64::consts::PI * k as f64 / size as f64;
                (angle.cos(), angle.sin())
            })
            .collect();

        let bits = size.trailing_zeros();
        let mut swaps = Vec::new();
        for i in 0..size {
            let j = (i as u32).reverse_bits() >> (32 - bits);
            if (i as u32) < j {
                swaps.push((i as u32, j));
            }
        }

        Self {
            size,
            twiddles,
            swaps,
            re: vec![0.0; size],
            im: vec![0.0; size],
        }
    }

    /// Power spectrum of a real signal: `|X[k]|²` for `k` in `0..=size/2`.
    ///
    /// `input` is zero-padded if it is shorter than the transform.
    pub fn power_spectrum(&mut self, input: &[f64], power: &mut [f64]) {
        debug_assert_eq!(power.len(), self.size / 2 + 1);

        let n = input.len().min(self.size);
        self.re[..n].copy_from_slice(&input[..n]);
        self.re[n..].fill(0.0);
        self.im.fill(0.0);
        self.transform();

        for (k, slot) in power.iter_mut().enumerate() {
            *slot = self.re[k] * self.re[k] + self.im[k] * self.im[k];
        }
    }

    /// The inverse transform of a real, even sequence given by its first half.
    ///
    /// A real even sequence has a real even transform, and the forward and
    /// inverse transforms of such a sequence differ only by the `1/N` — so the
    /// cepstrum, which is the inverse transform of a log spectrum, is computed
    /// here with the same forward pass rather than with a second routine that
    /// could disagree with it.
    ///
    /// `half` holds indices `0..=size/2`; the rest is mirrored.
    pub fn real_even(&mut self, half: &[f64], out: &mut [f64]) {
        debug_assert_eq!(half.len(), self.size / 2 + 1);

        self.re[..half.len()].copy_from_slice(half);
        for (k, &value) in half.iter().enumerate().take(self.size / 2).skip(1) {
            self.re[self.size - k] = value;
        }
        self.im.fill(0.0);
        self.transform();

        let scale = 1.0 / self.size as f64;
        for (q, slot) in out.iter_mut().enumerate() {
            *slot = self.re[q] * scale;
        }
    }

    fn transform(&mut self) {
        for &(i, j) in &self.swaps {
            self.re.swap(i as usize, j as usize);
            self.im.swap(i as usize, j as usize);
        }

        let mut span = 1;
        while span < self.size {
            let step = self.size / (span * 2);
            for start in (0..self.size).step_by(span * 2) {
                for k in 0..span {
                    let (wr, wi) = self.twiddles[k * step];
                    let a = start + k;
                    let b = a + span;
                    let tr = wr * self.re[b] - wi * self.im[b];
                    let ti = wr * self.im[b] + wi * self.re[b];
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
            }
            span *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine at bin `b` puts all its energy in bins `b` and `size-b`.
    #[test]
    fn a_tone_lands_on_its_bin() {
        let size = 64;
        let mut fft = Fft::new(size);
        let bin = 7;
        let input: Vec<f64> = (0..size)
            .map(|n| (2.0 * std::f64::consts::PI * bin as f64 * n as f64 / size as f64).sin())
            .collect();

        let mut power = vec![0.0; size / 2 + 1];
        fft.power_spectrum(&input, &mut power);

        let total: f64 = power.iter().sum();
        assert!(
            power[bin] / total > 0.99,
            "bin {bin} holds {:.4} of the energy",
            power[bin] / total
        );
    }

    /// Parseval, which catches a scaling or an indexing error at once.
    #[test]
    fn energy_is_conserved() {
        let size = 128;
        let mut fft = Fft::new(size);
        let input: Vec<f64> = (0..size)
            .map(|n| ((n * 37 % 61) as f64 / 61.0) - 0.5)
            .collect();

        let mut power = vec![0.0; size / 2 + 1];
        fft.power_spectrum(&input, &mut power);

        // Bins 1..size/2 stand for two conjugate halves each.
        let spectral: f64 =
            power[0] + power[size / 2] + 2.0 * power[1..size / 2].iter().sum::<f64>();
        let temporal: f64 = size as f64 * input.iter().map(|x| x * x).sum::<f64>();

        assert!(
            (spectral - temporal).abs() / temporal < 1e-12,
            "spectral {spectral} vs temporal {temporal}"
        );
    }

    /// The round trip the cepstrum depends on: an even sequence, forward and
    /// back, is itself.
    #[test]
    fn an_even_sequence_survives_the_round_trip() {
        let size = 64;
        let mut fft = Fft::new(size);
        let half: Vec<f64> = (0..=size / 2)
            .map(|k| (k as f64 * 0.3).cos() + 2.0)
            .collect();

        let mut cepstrum = vec![0.0; size];
        fft.real_even(&half, &mut cepstrum);

        // Transforming back: the cepstrum is itself even, so the same call
        // works, and the 1/N applies once in each direction.
        let mut back_half = vec![0.0; size / 2 + 1];
        back_half.copy_from_slice(&cepstrum[..size / 2 + 1]);
        let mut again = vec![0.0; size];
        fft.real_even(&back_half, &mut again);

        for k in 0..=size / 2 {
            let expected = half[k] / size as f64;
            assert!(
                (again[k] - expected).abs() < 1e-12,
                "index {k}: {} vs {expected}",
                again[k]
            );
        }
    }
}
