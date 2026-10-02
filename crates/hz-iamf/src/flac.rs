//! FLAC frames, as an IA sequence carries them (IAMF §3.11.3).
//!
//! IAMF does not carry FLAC *streams*. Each audio frame is one bare FLAC
//! `FRAME` of one or two channels, every frame exactly
//! `num_samples_per_frame` long, and the codec config holds the one
//! `STREAMINFO` block that describes them all. Two constraints narrow FLAC
//! itself: the channel assignment is 0 or 1 — mono, or two independent
//! channels, never the mid/side decorrelations — and the frame header states
//! its sample rate and depth outright rather than deferring to `STREAMINFO`.
//!
//! The coder is the classic one: per channel, the cheapest of a constant, a
//! verbatim block, one of FLAC's five fixed polynomial predictors, or a
//! linear predictor fitted to the block, and the residual Rice-coded over
//! the partitioning that costs least. It stays inside FLAC's streamable
//! subset at 48 kHz — prediction order twelve, eight partition levels, a
//! block of at most 4608 — because a decoder inside a browser is entitled
//! to assume it.
//!
//! Written from the format (RFC 9639) rather than ported: Levinson-Durbin
//! and the Tukey window are textbook, and the decisions — how an order is
//! chosen, how a coefficient is rounded, how a partitioning is priced — are
//! made here.

use hz_core::bits::BitWriter;
use std::fmt;

/// Highest linear prediction order: the subset's limit at 48 kHz and below.
const MAX_LPC_ORDER: usize = 12;
/// Coefficient precision in bits, the most the four-bit field can state.
const PRECISION: u32 = 15;
/// Highest partition order: the subset's limit.
const MAX_PARTITION_ORDER: u32 = 8;
/// Highest Rice parameter the five-bit form can state; 31 is its escape.
const MAX_RICE: u32 = 30;
/// The Tukey window's taper: half the block is cosine, which is what FLAC's
/// own reference encoder settled on.
const TAPER: f64 = 0.5;

/// A configuration the format cannot carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub String);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

/// Writes FLAC frames of one block size, rate and depth.
///
/// One encoder serves every substream: a frame depends on nothing but its
/// own samples and its number, and the scratch it needs is the same shape
/// for all of them. Nothing is allocated per frame once the first one is
/// written.
pub struct Encoder {
    sample_rate: u32,
    bits: u32,
    block: usize,
    window: Vec<f64>,
    windowed: Vec<f64>,
    shifted: Vec<i64>,
    residual: Vec<i64>,
    best_residual: Vec<i64>,
    writer: BitWriter,
}

/// How one channel of a frame is coded, when it is not one constant value.
#[derive(Debug, Clone, Copy)]
enum Model {
    Verbatim,
    Fixed(usize),
    Lpc {
        order: usize,
        shift: u32,
        coefficients: [i32; MAX_LPC_ORDER],
    },
}

/// How a residual is Rice-coded: the partitioning, and each partition's
/// parameter or its escape width.
#[derive(Debug, Clone, Copy)]
struct Plan {
    order: u32,
    /// Five-bit parameters rather than four.
    wide: bool,
    /// Per partition: `Ok(k)` for a Rice parameter, `Err(width)` for the
    /// escape to raw `width`-bit samples.
    partitions: [Result<u32, u32>; 1 << MAX_PARTITION_ORDER],
    bits: u64,
}

impl Encoder {
    /// An encoder for `block`-sample frames at `sample_rate` and `bits`.
    pub fn new(sample_rate: u32, bits: u32, block: usize) -> Result<Self, Unsupported> {
        // RFC 9639 names 32 bits too, but decoders older than it — libFLAC
        // before 1.4, symphonia — read that code as reserved and refuse the
        // frame, and a browser's decoder is not something to bet on.
        if ![8, 12, 16, 20, 24].contains(&bits) {
            return Err(Unsupported(format!(
                "{bits}-bit FLAC; the frame header names 8, 12, 16, 20 or 24 bits for \
                 every decoder in use"
            )));
        }
        if sample_rate_code(sample_rate).is_none() {
            return Err(Unsupported(format!(
                "a {sample_rate} Hz FLAC frame, which its header cannot state"
            )));
        }
        // Sixteen is the format's floor for every block but the last, and
        // IAMF has no last block: every frame is this long.
        if !(16..=4608).contains(&block) {
            return Err(Unsupported(format!(
                "{block}-sample FLAC frames; the streamable subset takes 16 to 4608"
            )));
        }
        let window = tukey(block);
        Ok(Self {
            sample_rate,
            bits,
            block,
            window,
            windowed: vec![0.0; block],
            shifted: vec![0; block],
            residual: vec![0; block],
            best_residual: vec![0; block],
            writer: BitWriter::with_capacity(block * 8),
        })
    }

    pub fn block(&self) -> usize {
        self.block
    }

    /// The `STREAMINFO` block as IAMF constrains it: both block sizes the
    /// frame size, the frame sizes and the signature unknown, and one channel
    /// — the real count of each substream is in its own frame headers.
    pub fn stream_info(&self) -> [u8; 34] {
        stream_info(self.sample_rate, self.bits, self.block, 1)
    }

    /// The codec config's `decoder_config`: the metadata blocks, which is
    /// `STREAMINFO` alone, marked as the last one.
    pub fn decoder_config(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(38);
        // Last-block flag, block type 0, and a 24-bit length.
        out.extend_from_slice(&[0x80, 0x00, 0x00, 34]);
        out.extend_from_slice(&self.stream_info());
        out
    }

    /// Write frame `number` of one channel or two, each exactly a block long.
    ///
    /// The bytes are the encoder's until the next call.
    pub fn encode(&mut self, number: u64, channels: &[&[i32]]) -> &[u8] {
        assert!(
            matches!(channels.len(), 1 | 2),
            "an IAMF FLAC frame is mono or stereo"
        );
        for channel in channels {
            assert_eq!(channel.len(), self.block, "a frame is exactly one block");
        }

        self.writer.clear();
        self.write_header(number, channels.len());
        for channel in channels {
            self.write_subframe(channel);
        }
        self.writer.align_to(8);
        let crc = crc16(self.writer.bytes());
        self.writer.put(16, u32::from(crc));
        self.writer.bytes()
    }

    fn write_header(&mut self, number: u64, channels: usize) {
        let (block_code, block_extra) = block_size_code(self.block);
        let (rate_code, rate_extra) =
            sample_rate_code(self.sample_rate).expect("checked when the encoder was made");
        let w = &mut self.writer;
        // Sync, a reserved zero, and fixed-blocksize numbering.
        w.put(16, 0xfff8);
        w.put(4, block_code);
        w.put(4, rate_code);
        // Channel assignment: 0 or 1, independent channels only.
        w.put(4, channels as u32 - 1);
        w.put(3, sample_size_code(self.bits));
        w.put(1, 0);
        put_utf8(w, number);
        if let Some((bits, value)) = block_extra {
            w.put(bits, value);
        }
        if let Some((bits, value)) = rate_extra {
            w.put(bits, value);
        }
        let crc = crc8(w.bytes());
        w.put(8, u32::from(crc));
    }

    fn write_subframe(&mut self, samples: &[i32]) {
        let n = self.block;
        if samples.iter().all(|&s| s == samples[0]) {
            let w = &mut self.writer;
            w.put(8, 0);
            put_signed_wide(w, self.bits, i64::from(samples[0]));
            return;
        }

        // Low bits every sample leaves at zero — a 16-bit programme in a
        // 24-bit stream — are said once rather than carried in every sample.
        let ors = samples.iter().fold(0i32, |acc, &s| acc | s);
        let wasted = ors.trailing_zeros().min(self.bits - 1);
        let bits = self.bits - wasted;
        for (out, &s) in self.shifted.iter_mut().zip(samples) {
            *out = i64::from(s) >> wasted;
        }

        let (model, plan) = self.choose(bits);
        let w = &mut self.writer;
        let type_code = match model {
            Model::Verbatim => 1,
            Model::Fixed(order) => 8 | order as u32,
            Model::Lpc { order, .. } => 32 | (order as u32 - 1),
        };
        w.put(1, 0);
        w.put(6, type_code);
        if wasted > 0 {
            w.put(1, 1);
            // Unary k − 1: zeros, then a one.
            w.put(wasted, 1);
        } else {
            w.put(1, 0);
        }

        match model {
            Model::Verbatim => {
                for &s in &self.shifted[..n] {
                    put_signed_wide(w, bits, s);
                }
            }
            Model::Fixed(order) => {
                for &s in &self.shifted[..order] {
                    put_signed_wide(w, bits, s);
                }
                put_residual(w, &self.best_residual[order..n], order, n, &plan);
            }
            Model::Lpc {
                order,
                shift,
                coefficients,
            } => {
                for &s in &self.shifted[..order] {
                    put_signed_wide(w, bits, s);
                }
                w.put(4, PRECISION - 1);
                w.put(5, shift);
                for &c in &coefficients[..order] {
                    w.put_signed(PRECISION, c);
                }
                put_residual(w, &self.best_residual[order..n], order, n, &plan);
            }
        }
    }

    /// The cheapest model for the shifted samples, and its residual in
    /// `best_residual`.
    fn choose(&mut self, bits: u32) -> (Model, Plan) {
        let n = self.block;
        let verbatim = n as u64 * u64::from(bits);
        let mut best = (Model::Verbatim, empty_plan(verbatim));

        // The fixed predictors: the order is chosen on the residual's
        // magnitude, which is what Rice coding costs in, and only the winner
        // is priced in full.
        let order = best_fixed_order(&self.shifted[..n]);
        fixed_residual(&self.shifted[..n], order, &mut self.residual);
        if let Some(plan) = plan_rice(&self.residual[order..n], order, n) {
            let cost = plan.bits + order as u64 * u64::from(bits);
            if cost < best.1.bits {
                best = (Model::Fixed(order), Plan { bits: cost, ..plan });
                self.best_residual[order..n].copy_from_slice(&self.residual[order..n]);
            }
        }

        if let Some((order, shift, coefficients)) = self.fit_lpc(bits) {
            let fits = lpc_residual(
                &self.shifted[..n],
                &coefficients[..order],
                shift,
                &mut self.residual,
            );
            let plan = fits
                .then(|| plan_rice(&self.residual[order..n], order, n))
                .flatten();
            if let Some(plan) = plan {
                let side = order as u64 * u64::from(bits + PRECISION) + 4 + 5;
                let cost = plan.bits + side;
                if cost < best.1.bits {
                    best = (
                        Model::Lpc {
                            order,
                            shift,
                            coefficients,
                        },
                        Plan { bits: cost, ..plan },
                    );
                    self.best_residual[order..n].copy_from_slice(&self.residual[order..n]);
                }
            }
        }
        best
    }

    /// A linear predictor for the shifted samples: an order, and its
    /// coefficients quantised. `None` when the block has nothing to predict.
    fn fit_lpc(&mut self, bits: u32) -> Option<(usize, u32, [i32; MAX_LPC_ORDER])> {
        let n = self.block;
        let max_order = MAX_LPC_ORDER.min(n - 1);
        for ((out, &s), &w) in self
            .windowed
            .iter_mut()
            .zip(&self.shifted)
            .zip(&self.window)
        {
            *out = s as f64 * w;
        }
        let mut r = [0.0f64; MAX_LPC_ORDER + 1];
        for (lag, r) in r.iter_mut().enumerate().take(max_order + 1) {
            *r = self.windowed[lag..]
                .iter()
                .zip(&self.windowed)
                .map(|(a, b)| a * b)
                .sum();
        }
        if r[0] <= 0.0 {
            return None;
        }

        // Levinson-Durbin, keeping every order's coefficients and error so
        // that the order can be chosen after.
        let mut a = [0.0f64; MAX_LPC_ORDER];
        let mut sets = [[0.0f64; MAX_LPC_ORDER]; MAX_LPC_ORDER];
        let mut errors = [0.0f64; MAX_LPC_ORDER];
        let mut error = r[0];
        let mut reached = 0;
        for i in 0..max_order {
            let acc = r[i + 1] - (0..i).map(|j| a[j] * r[i - j]).sum::<f64>();
            let k = acc / error;
            let previous = a;
            a[i] = k;
            for j in 0..i {
                a[j] = previous[j] - k * previous[i - 1 - j];
            }
            error *= 1.0 - k * k;
            sets[i] = a;
            errors[i] = error;
            reached = i + 1;
            if error <= 0.0 {
                break;
            }
        }
        if reached == 0 {
            return None;
        }

        // The order whose expected size is least: a residual of variance σ²
        // Rice-codes in about log₂σ bits a sample, and each order adds a warm-
        // up sample and a coefficient. Ranking, not pricing — the winner is
        // priced exactly by the caller.
        let energy: f64 = self.window.iter().map(|w| w * w).sum();
        let mut chosen = 1;
        let mut least = f64::INFINITY;
        for order in 1..=reached {
            let variance = (errors[order - 1] / energy).max(1e-12);
            let per_sample = (0.5 * variance.log2()).max(0.0) + 1.0;
            let cost = per_sample * (n - order) as f64 + order as f64 * f64::from(bits + PRECISION);
            if cost < least {
                least = cost;
                chosen = order;
            }
        }

        let (shift, coefficients) = quantise(&sets[chosen - 1][..chosen])?;
        Some((chosen, shift, coefficients))
    }
}

/// Coefficients to `PRECISION`-bit integers and the shift that scales them
/// back, carrying each one's rounding error into the next so that the sum
/// the decoder forms errs as little as the integers allow.
fn quantise(coefficients: &[f64]) -> Option<(u32, [i32; MAX_LPC_ORDER])> {
    let largest = coefficients.iter().fold(0.0f64, |m, c| m.max(c.abs()));
    if largest == 0.0 || !largest.is_finite() {
        return None;
    }
    // Room for the largest coefficient's integer part and a sign; the rest
    // of the precision is fraction. The shift field is five bits and may not
    // be negative.
    let magnitude = largest.log2().floor() as i32 + 1;
    let shift = (PRECISION as i32 - 1 - magnitude).clamp(0, 15) as u32;
    let limit = 1i64 << (PRECISION - 1);
    let mut out = [0i32; MAX_LPC_ORDER];
    let mut carried = 0.0;
    for (out, &c) in out.iter_mut().zip(coefficients) {
        let wanted = c * f64::from(1u32 << shift) + carried;
        let q = (wanted.round() as i64).clamp(-limit, limit - 1);
        carried = wanted - q as f64;
        *out = q as i32;
    }
    Some((shift, out))
}

/// The residual of a quantised linear predictor. False when a residual does
/// not fit the 32 bits a decoder holds it in, which can only happen to a
/// predictor that has diverged and is not worth having.
fn lpc_residual(samples: &[i64], coefficients: &[i32], shift: u32, out: &mut [i64]) -> bool {
    let order = coefficients.len();
    for i in order..samples.len() {
        let mut prediction = 0i64;
        for (j, &c) in coefficients.iter().enumerate() {
            prediction += i64::from(c) * samples[i - 1 - j];
        }
        let r = samples[i] - (prediction >> shift);
        if r < i64::from(i32::MIN) || r > i64::from(i32::MAX) {
            return false;
        }
        out[i] = r;
    }
    true
}

/// The fixed predictor whose residual is smallest in total magnitude.
fn best_fixed_order(samples: &[i64]) -> usize {
    let mut totals = [0u64; 5];
    for i in 4..samples.len() {
        let e0 = samples[i];
        let e1 = e0 - samples[i - 1];
        let e2 = e1 - (samples[i - 1] - samples[i - 2]);
        let e3 = e2 - (samples[i - 1] - 2 * samples[i - 2] + samples[i - 3]);
        let e4 = e3 - (samples[i - 1] - 3 * samples[i - 2] + 3 * samples[i - 3] - samples[i - 4]);
        for (total, e) in totals.iter_mut().zip([e0, e1, e2, e3, e4]) {
            *total += e.unsigned_abs();
        }
    }
    (0..5).min_by_key(|&o| totals[o]).unwrap_or(0)
}

/// The residual of fixed predictor `order`: the `order`-th difference.
fn fixed_residual(samples: &[i64], order: usize, out: &mut [i64]) {
    for i in order..samples.len() {
        let s = |k: usize| samples[i - k];
        out[i] = match order {
            0 => s(0),
            1 => s(0) - s(1),
            2 => s(0) - 2 * s(1) + s(2),
            3 => s(0) - 3 * s(1) + 3 * s(2) - s(3),
            _ => s(0) - 4 * s(1) + 6 * s(2) - 4 * s(3) + s(4),
        };
    }
}

fn empty_plan(bits: u64) -> Plan {
    Plan {
        order: 0,
        wide: false,
        partitions: [Ok(0); 1 << MAX_PARTITION_ORDER],
        bits,
    }
}

/// A residual folded onto the unsigned integers: 0, −1, 1, −2, …
#[inline]
fn zigzag(r: i64) -> u64 {
    ((r << 1) ^ (r >> 63)) as u64
}

/// Two's-complement bits a value needs.
#[inline]
fn signed_width(v: i64) -> u32 {
    let magnitude = if v < 0 { !v } else { v };
    65 - magnitude.leading_zeros()
}

/// The cheapest Rice coding of `residual` — the samples after the first
/// `predictor` of a `block` — over every partitioning the subset allows.
///
/// Priced on the closed form `n(k + 1) + Σu ≫ k`, which counts the unary
/// part of a whole partition from its sum rather than sample by sample: it
/// errs below the exact count by under a bit a sample, and it is what makes
/// trying nine partitionings cost one pass over the residual. `None` when a
/// residual is too wide for any parameter, which only an escape can carry
/// and is cheaper verbatim.
fn plan_rice(residual: &[i64], predictor: usize, block: usize) -> Option<Plan> {
    // The finest partitioning the block allows: the block divides evenly,
    // and the first partition, which loses the warm-up samples, keeps some.
    let mut finest = 0;
    while finest < MAX_PARTITION_ORDER
        && block.is_multiple_of(1 << (finest + 1))
        && (block >> (finest + 1)) > predictor
    {
        finest += 1;
    }

    // Sums and widths at the finest level; coarser levels merge them.
    let count = 1usize << finest;
    let span = block >> finest;
    let mut sums = [0u64; 1 << MAX_PARTITION_ORDER];
    let mut widths = [0u32; 1 << MAX_PARTITION_ORDER];
    let mut lengths = [0u64; 1 << MAX_PARTITION_ORDER];
    for p in 0..count {
        let start = (p * span).saturating_sub(predictor);
        let end = (p + 1) * span - predictor;
        let mut sum = 0u64;
        let mut width = 1;
        for &r in &residual[start..end] {
            sum = sum.saturating_add(zigzag(r));
            width = width.max(signed_width(r));
        }
        sums[p] = sum;
        widths[p] = width;
        lengths[p] = (end - start) as u64;
    }

    let mut best: Option<Plan> = None;
    for order in (0..=finest).rev() {
        let partitions = 1usize << order;
        let merge = 1usize << (finest - order);
        let mut plan = empty_plan(0);
        plan.order = order;
        let mut bits = 2 + 4;
        let mut widest_k = 0;
        for p in 0..partitions {
            let range = p * merge..(p + 1) * merge;
            let sum: u64 = sums[range.clone()]
                .iter()
                .fold(0, |a, &b| a.saturating_add(b));
            let n: u64 = lengths[range.clone()].iter().sum();
            let width = widths[range].iter().copied().max().unwrap_or(1);
            let (k, rice) = best_parameter(sum, n);
            // The escape states its width in five bits, so a 32-bit
            // partition has no escape.
            let raw = if width < 32 {
                5 + n * u64::from(width)
            } else {
                u64::MAX
            };
            if rice <= raw && k <= MAX_RICE {
                plan.partitions[p] = Ok(k);
                bits += rice;
                widest_k = widest_k.max(k);
            } else {
                plan.partitions[p] = Err(width);
                bits += raw;
            }
        }
        plan.wide = widest_k > 14;
        bits += partitions as u64 * if plan.wide { 5 } else { 4 };
        plan.bits = bits;
        if best.is_none_or(|b| bits < b.bits) {
            best = Some(plan);
        }
    }
    best
}

/// The Rice parameter that codes `n` values summing to `sum` in fewest bits,
/// and that count.
fn best_parameter(sum: u64, n: u64) -> (u32, u64) {
    if n == 0 {
        return (0, 0);
    }
    let mut best = (0, u64::MAX);
    for k in 0..=MAX_RICE {
        let bits = n * u64::from(k + 1) + (sum >> k);
        if bits < best.1 {
            best = (k, bits);
        }
        // The cost is convex in k: once it rises it does not come back.
        if bits > best.1 {
            break;
        }
    }
    best
}

fn put_residual(w: &mut BitWriter, residual: &[i64], predictor: usize, block: usize, plan: &Plan) {
    w.put(2, u32::from(plan.wide));
    w.put(4, plan.order);
    let escape = if plan.wide { 31 } else { 15 };
    let parameter_bits = if plan.wide { 5 } else { 4 };
    let span = block >> plan.order;
    for p in 0..1usize << plan.order {
        let start = (p * span).saturating_sub(predictor);
        let end = (p + 1) * span - predictor;
        let values = &residual[start..end];
        match plan.partitions[p] {
            Ok(k) => {
                w.put(parameter_bits, k);
                for &r in values {
                    put_rice(w, zigzag(r), k);
                }
            }
            Err(width) => {
                w.put(parameter_bits, escape);
                w.put(5, width);
                for &r in values {
                    put_signed_wide(w, width, r);
                }
            }
        }
    }
}

/// One Rice codeword: the quotient in unary — zeros, then a one — and the
/// remainder in `k` bits. One write when it fits, which is nearly always.
#[inline]
fn put_rice(w: &mut BitWriter, u: u64, k: u32) {
    let quotient = u >> k;
    let low = (u & ((1u64 << k) - 1)) as u32;
    if quotient + 1 + u64::from(k) <= 32 {
        // The quotient's zeros are the leading bits of a wider field.
        w.put(quotient as u32 + 1 + k, (1u32 << k) | low);
        return;
    }
    let mut zeros = quotient;
    while zeros >= 32 {
        w.put(32, 0);
        zeros -= 32;
    }
    w.put(zeros as u32 + 1, 1);
    w.put(k, low);
}

/// A signed value of up to 32 bits, held wider.
#[inline]
fn put_signed_wide(w: &mut BitWriter, bits: u32, value: i64) {
    debug_assert!(
        signed_width(value) <= bits,
        "{value} does not fit {bits} bits"
    );
    let mask = if bits == 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    };
    w.put(bits, (value as u32) & mask);
}

/// The frame number, in the UTF-8-shaped coding the header uses.
fn put_utf8(w: &mut BitWriter, value: u64) {
    assert!(
        value < 1 << 31,
        "frame {value} is past what a header can number"
    );
    let value = value as u32;
    if value < 0x80 {
        w.put(8, value);
        return;
    }
    let continuation = match value {
        0x80..0x800 => 1,
        0x800..0x1_0000 => 2,
        0x1_0000..0x20_0000 => 3,
        0x20_0000..0x400_0000 => 4,
        _ => 5,
    };
    // The lead byte: as many ones as bytes, a zero, then the top bits.
    let lead_marker = (0xff00u32 >> (continuation + 1)) & 0xff;
    w.put(8, lead_marker | (value >> (6 * continuation)));
    for i in (0..continuation).rev() {
        w.put(8, 0x80 | ((value >> (6 * i)) & 0x3f));
    }
}

/// The header's block-size code, and the field after the frame number that
/// some codes defer to.
fn block_size_code(block: usize) -> (u32, Option<(u32, u32)>) {
    match block {
        192 => (1, None),
        576 | 1152 | 2304 | 4608 => (2 + (block / 576).trailing_zeros(), None),
        256 | 512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768 => {
            (8 + (block / 256).trailing_zeros(), None)
        }
        _ if block <= 256 => (6, Some((8, block as u32 - 1))),
        _ => (7, Some((16, block as u32 - 1))),
    }
}

/// The header's sample-rate code and its deferred field, for a rate the
/// header can state without `STREAMINFO`.
fn sample_rate_code(rate: u32) -> Option<(u32, Option<(u32, u32)>)> {
    Some(match rate {
        88_200 => (1, None),
        176_400 => (2, None),
        192_000 => (3, None),
        8_000 => (4, None),
        16_000 => (5, None),
        22_050 => (6, None),
        24_000 => (7, None),
        32_000 => (8, None),
        44_100 => (9, None),
        48_000 => (10, None),
        96_000 => (11, None),
        r if r % 1000 == 0 && r / 1000 < 256 => (12, Some((8, r / 1000))),
        r if r < 65_536 => (13, Some((16, r))),
        r if r % 10 == 0 && r / 10 < 65_536 => (14, Some((16, r / 10))),
        _ => return None,
    })
}

fn sample_size_code(bits: u32) -> u32 {
    match bits {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        _ => unreachable!("checked when the encoder was made"),
    }
}

/// A `STREAMINFO` block. Public within the crate for the tests, which wrap
/// frames into a plain FLAC stream and need the real channel count in it.
pub(crate) fn stream_info(sample_rate: u32, bits: u32, block: usize, channels: u32) -> [u8; 34] {
    let mut w = BitWriter::with_capacity(34);
    w.put(16, block as u32);
    w.put(16, block as u32);
    // Frame sizes unknown.
    w.put(24, 0);
    w.put(24, 0);
    w.put(20, sample_rate);
    w.put(3, channels - 1);
    w.put(5, bits - 1);
    // Total samples unknown: 36 bits of zero.
    w.put(32, 0);
    w.put(4, 0);
    // No signature.
    w.put_zeroes(128);
    let bytes = w.finish();
    bytes.try_into().expect("STREAMINFO is 34 bytes")
}

/// Half a cosine at each end and flat between: the window the predictor is
/// fitted through, so that the block's edges do not read as a step.
fn tukey(n: usize) -> Vec<f64> {
    let last = (n - 1) as f64;
    (0..n)
        .map(|i| {
            let x = i as f64 / last;
            let edge = TAPER / 2.0;
            let rise = |t: f64| 0.5 * (1.0 - (std::f64::consts::TAU * t / TAPER).cos());
            if x < edge {
                rise(x)
            } else if x > 1.0 - edge {
                rise(1.0 - x)
            } else {
                1.0
            }
        })
        .collect()
}

/// CRC-8 of the frame header: polynomial x⁸ + x² + x + 1, from zero.
fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |crc, &b| CRC8[usize::from(crc ^ b)])
}

/// CRC-16 of the whole frame: polynomial x¹⁶ + x¹⁵ + x² + 1, from zero.
fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0u16, |crc, &b| {
        (crc << 8) ^ CRC16[usize::from((crc >> 8) as u8 ^ b)]
    })
}

static CRC8: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

static CRC16: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia_bundle_flac::FlacDecoder;
    use symphonia_core::audio::{AudioBufferRef, Signal};
    use symphonia_core::codecs::{CODEC_TYPE_FLAC, CodecParameters, Decoder, DecoderOptions};
    use symphonia_core::formats::Packet;

    /// Decode frames through an independent decoder, one channel vector each.
    fn decode(encoder: &Encoder, channels: u32, frames: &[Vec<u8>]) -> Vec<Vec<i32>> {
        let info = stream_info(encoder.sample_rate, encoder.bits, encoder.block, channels);
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_FLAC)
            .with_extra_data(info.to_vec().into_boxed_slice());
        let mut decoder = FlacDecoder::try_new(&params, &DecoderOptions { verify: true })
            .expect("the decoder takes the STREAMINFO");
        let mut out = vec![Vec::new(); channels as usize];
        for (i, frame) in frames.iter().enumerate() {
            let packet = Packet::new_from_slice(0, i as u64 * encoder.block as u64, 0, frame);
            let decoded = decoder.decode(&packet).expect("the frame decodes");
            let shift = 32 - encoder.bits;
            match decoded {
                AudioBufferRef::S32(buf) => {
                    for (c, out) in out.iter_mut().enumerate() {
                        out.extend(buf.chan(c).iter().map(|&s| s >> shift));
                    }
                }
                other => panic!("unexpected sample format {:?}", other.spec()),
            }
        }
        out
    }

    fn round_trip(bits: u32, block: usize, channels: Vec<Vec<i32>>) -> usize {
        let mut encoder = Encoder::new(48_000, bits, block).unwrap();
        let frames_count = channels[0].len() / block;
        let mut frames = Vec::new();
        let mut size = 0;
        for f in 0..frames_count {
            let slices: Vec<&[i32]> = channels
                .iter()
                .map(|c| &c[f * block..(f + 1) * block])
                .collect();
            let bytes = encoder.encode(f as u64, &slices).to_vec();
            size += bytes.len();
            frames.push(bytes);
        }
        let decoded = decode(&encoder, channels.len() as u32, &frames);
        for (c, (got, want)) in decoded.iter().zip(&channels).enumerate() {
            assert_eq!(got.len(), want.len(), "channel {c}");
            assert!(got == want, "channel {c} differs");
        }
        size
    }

    /// A deterministic noise source, so a failure reproduces.
    fn noise(seed: u64, n: usize, amplitude: i32) -> Vec<i32> {
        let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((x >> 33) as i64 % (2 * i64::from(amplitude) + 1) - i64::from(amplitude)) as i32
            })
            .collect()
    }

    fn tone(n: usize, amplitude: f64, period: f64) -> Vec<i32> {
        (0..n)
            .map(|i| (amplitude * (std::f64::consts::TAU * i as f64 / period).sin()).round() as i32)
            .collect()
    }

    #[test]
    fn a_tone_round_trips_and_compresses() {
        let n = 4096 * 6;
        let left = tone(n, 3_000_000.0, 97.3);
        let right = tone(n, 2_000_000.0, 41.7);
        let size = round_trip(24, 4096, vec![left, right]);
        // A pure tone predicts almost perfectly: far under the 24 bits a
        // sample it arrived in.
        assert!(size * 8 < n * 2 * 8, "{size} bytes");
    }

    #[test]
    fn noise_round_trips_at_every_depth() {
        for bits in [16, 20, 24] {
            let amplitude = ((1i64 << (bits - 1)) - 1) as i32;
            let n = 1024 * 3;
            round_trip(bits, 1024, vec![noise(u64::from(bits), n, amplitude)]);
        }
    }

    /// Silence, a constant, a block with wasted bits, a full-scale square:
    /// the shapes that take a model of their own or none at all.
    #[test]
    fn the_degenerate_blocks_round_trip() {
        let block = 576;
        let mut mono = vec![0; block];
        mono.extend(std::iter::repeat_n(-1234, block));
        mono.extend(noise(7, block, 30_000).iter().map(|s| s << 8));
        mono.extend((0..block).map(|i| if i % 64 < 32 { 8_388_607 } else { -8_388_608 }));
        round_trip(24, block, vec![mono]);
    }

    /// A block length the header has to spell out after the frame number.
    #[test]
    fn an_odd_block_length_round_trips() {
        let block = 960;
        let n = block * 4;
        round_trip(24, block, vec![tone(n, 1e6, 33.3), noise(3, n, 1000)]);
    }

    /// Frame numbers past one byte take the multi-byte form.
    #[test]
    fn frame_numbers_take_the_utf8_form() {
        let mut encoder = Encoder::new(48_000, 16, 192).unwrap();
        let silent = vec![0i32; 192];
        for number in [0u64, 127, 128, 2047, 2048, 65_535, 65_536, 1 << 30] {
            let bytes = encoder.encode(number, &[&silent]).to_vec();
            // Sync, codes, then the number.
            let lead = bytes[4];
            let extra = lead.leading_ones().saturating_sub(1) as usize;
            let mut value = u64::from(lead & (0x7f >> extra.min(6)));
            for &b in &bytes[5..5 + extra] {
                assert_eq!(b & 0xc0, 0x80);
                value = (value << 6) | u64::from(b & 0x3f);
            }
            assert_eq!(value, number);
        }
    }

    #[test]
    fn the_checksums_are_the_formats() {
        // Check values for the two polynomials over "123456789".
        assert_eq!(crc8(b"123456789"), 0xf4);
        assert_eq!(crc16(b"123456789"), 0xfee8);
    }
}
