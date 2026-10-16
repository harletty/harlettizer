//! Read a DTS stream's headers, write them back, and say what is in them.
//!
//! ```text
//! cargo xtask dts stream.dts [--frames N]
//! cargo run -p xtask --features dca-oracle -- dts stream.dts
//! cargo run -p xtask --features dca-oracle -- dts programme.mka
//! ```
//!
//! A Matroska file (`.mka`, `.mkv`) is read through its `A_DTS` track: its
//! blocks, one frame each, are the stream.
//!
//! Phase C0's acceptance: every core frame header and every extension
//! substream header of a reference stream read by `hz-dca` and written again,
//! byte for byte, the substream's checksum computed rather than copied. It
//! also looks for the DTS:X navigation word in each lossless asset's
//! descriptor tail and reports the largest offset and block it has seen
//! against the limits the word can express, which is the first question
//! phase C3 has to answer.
//!
//! With `dca-oracle`, the workspace's `dca` decoder reads the same frames:
//! its core header fields and its navigation are compared with ours, and the
//! standard DTS:X profile's height fold is undone twice — in integers here,
//! [`unfold`], which is the oracle phase C3 is held to, and through the
//! decoder's floating-point presentation helper — so the gap between the two
//! is measured on real streams rather than asserted.

use hz_core::{Error, Result};
use hz_dca::bits::Bits;
use hz_dca::core::CoreHeader;
use hz_dca::dtsx;
use hz_dca::exss::{ChannelMapping, Coding, ExssHeader, XllSync};
use hz_dca::navigation::{self, Navigation};
use hz_dca::peak::{self, DecoderBuffer, Taken};
use hz_dca::split;
use hz_dca::xll;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Options {
    pub input: PathBuf,
    /// Stop after this many frames.
    pub frames: Option<usize>,
    /// The PCM a core-only stream was encoded from: its decode, through the
    /// oracle, is compared with it.
    pub reference: Option<Reference>,
    /// Where to write the standard DTS:X profile's twelve channels before
    /// the fold, through the oracle and [`unfold`].
    pub unfold: Option<PathBuf>,
    /// Where to write the rate, frame by frame, as CSV.
    pub rate_log: Option<PathBuf>,
    /// The ceiling the stream was encoded under, in bits a second: a frame
    /// over it fails the run.
    pub ceiling: Option<u32>,
}

/// Interleaved 32-bit PCM in WAV order — L, R, C, LFE, Ls, Rs for 5.1 —
/// what `ffmpeg -f s32le` writes and `hz-dca`'s encoder reads.
pub struct Reference {
    pub pcm: Vec<i32>,
    pub channels: usize,
}

impl Reference {
    pub fn read(path: &std::path::Path, channels: usize) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        if channels == 0 || bytes.len() % (4 * channels) != 0 {
            return Err(Error::malformed(
                path,
                format!("not whole frames of {channels} 32-bit samples"),
            ));
        }
        let pcm = bytes
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Ok(Self { pcm, channels })
    }

    fn channel(&self, ch: usize) -> impl Iterator<Item = f64> + '_ {
        self.pcm
            .iter()
            .skip(ch)
            .step_by(self.channels)
            .map(|&s| f64::from(s) / 2_147_483_648.0)
    }
}

/// How a decode compares with the PCM it was made from.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// Samples the decode lags the input by.
    pub lag: usize,
    /// Signal to error in decibels, per channel in WAV order; infinite for
    /// a channel decoded exactly, `None` for a silent one.
    pub snr_db: Vec<Option<f64>>,
}

/// Compare a decode, channel by channel in WAV order, with its reference.
///
/// The lag is the one, up to 2048 samples, at which the two correlate best
/// over all channels; the first and last frame are left out of the figures,
/// since the encoder starts from silence and pads the end. `None` when there
/// is too little of either to say: under three frames.
///
/// # Panics
/// If the decode does not have the reference's channels.
#[cfg_attr(not(feature = "dca-oracle"), allow(dead_code))]
pub fn compare(reference: &Reference, decoded: &[Vec<f32>]) -> Option<Comparison> {
    const MAX_LAG: usize = 2048;
    const WINDOW: usize = 8192;
    const EDGE: usize = 512;
    assert_eq!(decoded.len(), reference.channels);
    let reference: Vec<Vec<f64>> = (0..reference.channels)
        .map(|ch| reference.channel(ch).collect())
        .collect();
    let len = reference[0].len();
    let decoded_len = decoded.iter().map(Vec::len).min()?;
    // Every candidate lag has to leave the whole window inside the decode.
    let max_lag = MAX_LAG.min(decoded_len / 2);
    let window = WINDOW.min(len).min(decoded_len - max_lag);
    if len < 3 * EDGE || window < EDGE {
        return None;
    }
    let correlation = |lag: usize| -> f64 {
        reference
            .iter()
            .zip(decoded)
            .map(|(r, d)| {
                r[..window]
                    .iter()
                    .zip(&d[lag..lag + window])
                    .map(|(&a, &b)| a * f64::from(b))
                    .sum::<f64>()
            })
            .sum()
    };
    // The first best, so that a periodic signal is not placed a period late.
    let mut lag = 0;
    let mut best = f64::NEG_INFINITY;
    for candidate in 0..max_lag {
        let c = correlation(candidate);
        if c > best {
            best = c;
            lag = candidate;
        }
    }
    let end = len
        .min(decoded_len.saturating_sub(lag))
        .saturating_sub(2 * EDGE);
    if end <= EDGE {
        return None;
    }
    let snr_db = reference
        .iter()
        .zip(decoded)
        .map(|(r, d)| {
            let (mut signal, mut error) = (0.0f64, 0.0f64);
            for (&a, &b) in r[EDGE..end].iter().zip(&d[lag + EDGE..lag + end]) {
                signal += a * a;
                error += (a - f64::from(b)).powi(2);
            }
            (signal > 0.0).then(|| 10.0 * (signal / error).log10())
        })
        .collect();
    Some(Comparison { lag, snr_db })
}

/// How a lossless decode compares with its input, sample for sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exact {
    pub channels: usize,
    /// Samples compared per channel: the input's.
    pub samples: usize,
    /// Channels the decode does not have, or has too few samples of.
    pub short: Vec<usize>,
    /// Per channel, samples that differ, and the first.
    pub differing: Vec<(usize, usize, usize)>,
}

impl Exact {
    pub fn is_exact(&self) -> bool {
        self.short.is_empty() && self.differing.is_empty()
    }
}

impl std::fmt::Display for Exact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_exact() {
            return write!(
                f,
                "{} channels × {} samples bit-exact",
                self.channels, self.samples
            );
        }
        if !self.short.is_empty() {
            write!(f, "channels {:?} missing or short; ", self.short)?;
        }
        for (ch, count, first) in &self.differing {
            write!(
                f,
                "channel {ch}: {count} samples differ, first at {first}; "
            )?;
        }
        Ok(())
    }
}

/// Compare a decode as 24-bit integers, per channel in WAV order, with the
/// 32-bit input it was made from: every input sample, at no lag.
pub fn compare_exact(reference: &Reference, decoded: &[Vec<i32>]) -> Exact {
    let channels = reference.channels;
    let samples = reference.pcm.len() / channels;
    let mut exact = Exact {
        channels,
        samples,
        short: Vec::new(),
        differing: Vec::new(),
    };
    for ch in 0..channels {
        let Some(plane) = decoded.get(ch).filter(|p| p.len() >= samples) else {
            exact.short.push(ch);
            continue;
        };
        let mut count = 0;
        let mut first = None;
        for (n, (&d, r)) in plane
            .iter()
            .zip(reference.pcm.iter().skip(ch).step_by(channels))
            .enumerate()
        {
            if d != r >> 8 {
                count += 1;
                first.get_or_insert(n);
            }
        }
        if let Some(first) = first {
            exact.differing.push((ch, count, first));
        }
    }
    exact
}

/// A 7.1.4 input's channels as 24-bit integers in 32-bit words: the 7.1
/// with each height folded into its corner as the standard profile folds
/// it, the heights, and the 7.1 as it was — what a decoder of the bed, of
/// the extension, and the unfold of both should give.
pub fn split_7_1_4(reference: &Reference) -> [Reference; 3] {
    let frames = reference.pcm.chunks_exact(12);
    let mut composite = Vec::with_capacity(frames.len() * 8);
    let mut heights = Vec::with_capacity(frames.len() * 4);
    let mut bed = Vec::with_capacity(frames.len() * 8);
    for frame in reference.pcm.chunks_exact(12) {
        for ch in 0..8 {
            let x = frame[ch] >> 8;
            // L, R, Lsr and Rsr take TFL, TFR, TBL and TBR.
            let folded = match [0, 1, 4, 5].iter().position(|&c| c == ch) {
                Some(h) => unfold::fold(x, frame[8 + h] >> 8, unfold::MINUS_3_DB),
                None => x,
            };
            composite.push(folded << 8);
            bed.push(frame[ch]);
        }
        heights.extend_from_slice(&frame[8..]);
    }
    let make = |pcm, channels| Reference { pcm, channels };
    [make(composite, 8), make(heights, 4), make(bed, 8)]
}

/// Phase C3's acceptance (a) on a decode through `dca`: the composite bed
/// and the four heights are what the encoder made of the input, and the
/// integer unfold of the two is the input's 7.1. The failures.
#[cfg(feature = "dca-oracle")]
fn compare_heights(
    reference: &Reference,
    decoded: &[Vec<i32>],
    heights: Option<&[Vec<i32>]>,
) -> Vec<String> {
    let [composite, expected_heights, bed] = split_7_1_4(reference);
    let mut failures = Vec::new();
    let exact = compare_exact(&composite, decoded);
    println!("  composite 7.1          {exact}");
    if !exact.is_exact() {
        failures.push(format!(
            "a composite bed through dca that is not the fold: {exact}"
        ));
    }
    let Some(heights) = heights else {
        failures.push("no heights: dca did not present the standard height profile".into());
        return failures;
    };
    let exact = compare_exact(&expected_heights, heights);
    println!("  heights                {exact}");
    if !exact.is_exact() {
        failures.push(format!(
            "heights through dca that are not the input's: {exact}"
        ));
    }
    // The bed taken back out: each corner less its height.
    let unfolded: Vec<Vec<i32>> = decoded
        .iter()
        .enumerate()
        .map(
            |(ch, plane)| match [0, 1, 4, 5].iter().position(|&c| c == ch) {
                Some(h) => plane
                    .iter()
                    .zip(&heights[h])
                    .map(|(&c, &y)| unfold::unfold(c, y, unfold::MINUS_3_DB))
                    .collect(),
                None => plane.clone(),
            },
        )
        .collect();
    let exact = compare_exact(&bed, &unfolded);
    println!("  unfolded 7.1           {exact}");
    if !exact.is_exact() {
        failures.push(format!("an unfold that is not the input's 7.1: {exact}"));
    }
    failures
}

/// Write the twelve channels before the fold, from the composite 7.1 the
/// oracle decoded and its heights, as 32-bit PCM in WAV 7.1.4 order. Each
/// frame is unfolded with the gains its own matrix states, whatever they
/// are; a frame whose matrix `hz_dca::dtsx` does not read is refused, named,
/// rather than unfolded with gains it may not have used.
#[cfg(feature = "dca-oracle")]
fn write_unfolded(
    path: &std::path::Path,
    bed: Option<&[Vec<i32>]>,
    heights: Option<&[Vec<i32>]>,
    folds: &[(usize, Option<Vec<[i32; 8]>>)],
) -> Result<()> {
    let (Some(bed), Some(heights)) = (bed, heights) else {
        return Err(Error::refused(
            path,
            "the stream has no frame dca presents as the standard height profile",
        ));
    };
    let samples = bed[0].len();
    if bed.len() != 8
        || heights.len() != 4
        || heights.iter().any(|h| h.len() != samples)
        || folds.iter().map(|(n, _)| n).sum::<usize>() != samples
    {
        return Err(Error::refused(
            path,
            "not every frame of the stream is the standard height profile",
        ));
    }
    // The bed's column of each WAV channel: L R C LFE Lsr Rsr Lss Rss in
    // C L R LFE Lsr Rsr Lss Rss.
    const COLUMN: [usize; 8] = [1, 2, 0, 3, 4, 5, 6, 7];
    let mut bytes = Vec::with_capacity(samples * 12 * 4);
    let (mut at, mut other) = (0usize, 0usize);
    for (frame, (length, gains)) in folds.iter().enumerate() {
        let Some(gains) = gains else {
            return Err(Error::refused(
                path,
                format!(
                    "frame {frame}: a fold matrix this reader does not take, so its unfold is \
                     not known"
                ),
            ));
        };
        other += usize::from(*gains != hz_dca::dtsx::FoldMatrix::standard().gains().unwrap());
        for n in at..at + length {
            for (ch, plane) in bed.iter().enumerate() {
                let mut x = plane[n];
                for (height, row) in heights.iter().zip(gains) {
                    let q = row[COLUMN[ch]];
                    if q != 0 {
                        x -= hz_dca::dtsx::fold(height[n], q);
                    }
                }
                bytes.extend_from_slice(&(x << 8).to_le_bytes());
            }
            for plane in heights {
                bytes.extend_from_slice(&(plane[n] << 8).to_le_bytes());
            }
        }
        at += length;
    }
    std::fs::write(path, bytes).map_err(|e| Error::io(path, e))?;
    println!(
        "  unfolded               {samples} samples of 7.1.4 written to {}, {} of {} frames \
         with a fold other than the standard one",
        path.display(),
        other,
        folds.len()
    );
    Ok(())
}

/// The integer inverse of the height fold.
///
/// The fold puts `(h × q + 2^14) >> 15` of a height into its lower corner,
/// Q15 rounded half up — the lossless decoder's own rounded multiply. Taking
/// exactly that back out gives the corner as it was before the fold, bit for
/// bit, provided nothing clipped on the way in.
#[cfg_attr(not(test), allow(dead_code))]
pub mod unfold {
    /// `(a × b + 2^14) >> 15`.
    pub fn rmul15(a: i32, b: i32) -> i32 {
        ((i64::from(a) * i64::from(b) + (1 << 14)) >> 15) as i32
    }

    /// The fold of `height` into `bed` at the Q15 gain `q15`.
    pub fn fold(bed: i32, height: i32, q15: i32) -> i32 {
        bed + rmul15(height, q15)
    }

    /// The bed before that fold.
    pub fn unfold(composite: i32, height: i32, q15: i32) -> i32 {
        composite - rmul15(height, q15)
    }

    /// −3 dB in Q15, the gain the standard profile folds its heights at.
    pub const MINUS_3_DB: i32 = 23170;
}

#[derive(Default)]
struct Tally {
    frames: usize,
    cores: usize,
    cores_identical: usize,
    substreams: usize,
    substreams_identical: usize,
    checksums_valid: usize,
    lossless_assets: usize,
    /// Bit offset of the navigation word in the descriptor tail → frames.
    navigation_at: BTreeMap<usize, usize>,
    navigation_max: Option<Navigation>,
    navigation_max_offset: usize,
    /// Every navigated block's offset and size, summed: what of the lossless
    /// frames is the bed before the extension, and what the extension.
    navigation_offsets: u64,
    navigation_sizes: u64,
    navigation_max_size: usize,
    tail_bits: BTreeMap<usize, usize>,
    first_difference: Option<String>,
    lossless: LosslessTally,
    rate: RateTally,
    /// (`vernum`, `dialnorm`) of each core header → frames, and each asset's
    /// `nuDialNormCode` → substreams.
    dialnorm: BTreeMap<(u8, u8), usize>,
    asset_dialnorm: BTreeMap<Option<u8>, usize>,
}

/// The stream's rate frame by frame, and the lossless decoder's smoothing
/// buffer fed with it (phase C4).
#[derive(Default)]
struct RateTally {
    /// Bytes of each frame, core and substream together.
    packets: Vec<u32>,
    /// Bytes of lossless data each substream carries.
    payloads: Vec<u32>,
    /// The sync fields (buffer code, delay, whether the frame starts past the
    /// data's first byte), or none, → substreams.
    sync: BTreeMap<Option<(u8, u8, u32, bool)>, usize>,
    buffer: DecoderBuffer,
    decoded: usize,
    delayed: usize,
    starved: usize,
    first_starved: Option<String>,
    /// Substreams whose data is not exactly the frame decoded from them.
    smoothed: usize,
    /// Per substream with lossless data: the frame's bytes, the lossless
    /// data's, the sync offset and delay when it states them, the frame
    /// decoded with it, and what the buffer holds after.
    log: Vec<RateRow>,
}

/// One substream in [`RateTally::log`].
struct RateRow {
    bytes: u32,
    lossless: u32,
    sync: Option<(u32, u32)>,
    decoded: Option<u32>,
    held: u32,
}

impl RateTally {
    fn packet(&mut self, bytes: usize) {
        self.packets.push(bytes as u32);
    }

    /// Feed one substream's lossless data to the decoder's buffer, and
    /// return the frame it decodes with it, if any.
    fn substream(&mut self, data: &[u8], sync: Option<&XllSync>) -> Option<&[u8]> {
        self.payloads.push(data.len() as u32);
        *self
            .sync
            .entry(sync.map(|s| {
                (
                    s.peak_buffer_code,
                    s.delay_bits,
                    s.delay_frames,
                    s.sync_offset > 0,
                )
            }))
            .or_default() += 1;
        let taken = self.buffer.push(data, sync);
        self.log.push(RateRow {
            bytes: self.packets.last().copied().unwrap_or(0),
            lossless: data.len() as u32,
            sync: sync.map(|s| (s.sync_offset, s.delay_frames)),
            decoded: match taken {
                Ok(Taken::Frame(size)) => Some(size as u32),
                _ => None,
            },
            held: self.buffer.held() as u32,
        });
        match taken {
            Ok(Taken::Frame(size)) => {
                self.decoded += 1;
                self.smoothed += usize::from(size != data.len());
                return Some(self.buffer.frame());
            }
            Ok(Taken::Delayed) => self.delayed += 1,
            Err(why) => {
                self.starved += 1;
                self.first_starved.get_or_insert_with(|| {
                    format!("substream {}: {why:?}", self.payloads.len() - 1)
                });
            }
        }
        None
    }

    /// One line per substream with lossless data: the frame's bytes, the lossless data its substream
    /// carries, the sync offset and delay stated (empty when absent), the
    /// lossless frame decoded with it (empty when none) and what the
    /// decoder's buffer holds after.
    fn write_log(&self, path: &std::path::Path) -> Result<()> {
        let mut csv = String::from("frame,bytes,lossless,sync_offset,delay,decoded,held\n");
        for (n, row) in self.log.iter().enumerate() {
            let (offset, delay) = row.sync.map_or((String::new(), String::new()), |(o, d)| {
                (o.to_string(), d.to_string())
            });
            let decoded = row.decoded.map_or(String::new(), |d| d.to_string());
            let RateRow {
                bytes,
                lossless,
                held,
                ..
            } = row;
            csv += &format!("{n},{bytes},{lossless},{offset},{delay},{decoded},{held}\n");
        }
        std::fs::write(path, csv).map_err(|e| Error::io(path, e))
    }

    /// What the decoder's buffer model says is wrong with the stream: a
    /// substream that leaves it without its frame, a stream that sends
    /// frames ahead without stating a buffer or holds more than the buffer
    /// it states, and frames over `ceiling`'s budget when one is given.
    fn failures(&self, samples: u32, rate: u32, ceiling: Option<u32>) -> Vec<String> {
        let mut failures = Vec::new();
        if self.starved > 0 {
            failures.push(format!(
                "{} substreams that leave the decoder's smoothing buffer without the frame due",
                self.starved
            ));
        }
        let stated = self.sync.keys().flatten().map(|s| s.0).max();
        match stated {
            Some(0) | None if self.smoothed > 0 => failures.push(format!(
                "{} substreams that carry other than their frame in a stream that states no \
                 smoothing buffer",
                self.smoothed
            )),
            Some(code) if code > 0 && self.buffer.peak() > peak::buffer_bytes(code) => failures
                .push(format!(
                    "a smoothing buffer holding {} bytes, over the {} the stream states",
                    self.buffer.peak(),
                    peak::buffer_bytes(code)
                )),
            _ => {}
        }
        if let Some(ceiling) = ceiling {
            let budget = peak::frame_budget(ceiling, samples, rate);
            let over = self
                .packets
                .iter()
                .filter(|&&b| b as usize > budget)
                .count();
            if over > 0 {
                failures.push(format!(
                    "{over} frames over the {budget} bytes {ceiling} b/s allows"
                ));
            }
        }
        failures
    }

    /// The rate against [`peak::CEILING`], the lossless frames' sizes, the
    /// sync fields, and the buffer: frames of `samples` at `rate` Hz.
    fn report(&self, samples: u32, rate: u32, lossless: &[u32]) {
        if self.packets.is_empty() {
            return;
        }
        let frame_seconds = f64::from(samples) / f64::from(rate);
        let mbps = |bytes: f64| bytes * 8.0 / frame_seconds / 1e6;
        let largest = *self.packets.iter().max().unwrap_or(&0);
        let mean =
            self.packets.iter().map(|&b| f64::from(b)).sum::<f64>() / self.packets.len() as f64;
        let budget = peak::frame_budget(peak::CEILING, samples, rate);
        let over = self
            .packets
            .iter()
            .filter(|&&b| b as usize > budget)
            .count();
        println!(
            "  rate                   {:.3} Mb/s on average, {:.3} at the largest frame ({largest} \
             bytes); {over} frames over {} bytes ({} Mb/s)",
            mbps(mean),
            mbps(f64::from(largest)),
            budget,
            f64::from(peak::CEILING) / 1e6
        );
        if !lossless.is_empty() {
            let mut sorted = lossless.to_vec();
            sorted.sort_unstable();
            let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
            let mean = sorted.iter().map(|&b| f64::from(b)).sum::<f64>() / sorted.len() as f64;
            let sd = (sorted
                .iter()
                .map(|&b| (f64::from(b) - mean).powi(2))
                .sum::<f64>()
                / sorted.len() as f64)
                .sqrt();
            let step = lossless
                .windows(2)
                .map(|w| w[1].abs_diff(w[0]))
                .max()
                .unwrap_or(0);
            println!(
                "  lossless frame sizes   min {} · p10 {} · median {} · p90 {} · p99 {} · max {}; \
                 sd {:.0} ({:.1} % of the mean); largest change between frames {step}",
                sorted[0],
                at(0.1),
                at(0.5),
                at(0.9),
                at(0.99),
                sorted[sorted.len() - 1],
                sd,
                100.0 * sd / mean
            );
        }
        let sync: Vec<String> = self
            .sync
            .iter()
            .map(|(s, n)| match s {
                Some((code, bits, delay, offset)) => format!(
                    "buffer code {code} delay {delay} ({bits} bits){} × {n}",
                    if *offset { " offset > 0" } else { "" }
                ),
                None => format!("no sync × {n}"),
            })
            .collect();
        println!("  lossless sync          {}", sync.join(", "));
        println!(
            "  smoothing buffer       {} frames decoded, {} delayed, {} starved; {} substreams \
             carry other than the frame decoded; {} bytes held at most",
            self.decoded,
            self.delayed,
            self.starved,
            self.smoothed,
            self.buffer.peak()
        );
        if let Some(why) = &self.first_starved {
            println!("                         first starved: {why}");
        }
    }
}

/// The lossless frames' headers, read and written back.
#[derive(Default)]
struct LosslessTally {
    /// The lossless frame being reassembled from the substreams that carry
    /// it, when it spans them (peak bit rate smoothing).
    pending: Vec<u8>,
    frames: usize,
    identical: usize,
    checksums_valid: usize,
    /// Assets whose data neither starts a frame nor says where one starts,
    /// while no frame is pending.
    unsynchronised: usize,
    /// Frames whose headers this reader refuses, and why the first was.
    refused: usize,
    first_refusal: Option<String>,
    /// Frames that span substreams.
    spanning: usize,
    /// Channel sets per frame → frames.
    shapes: BTreeMap<String, usize>,
    /// Bytes of the frames read, padding included, and of the largest.
    bytes: u64,
    largest: usize,
    /// Every frame's bytes, in order.
    sizes: Vec<u32>,
    /// The standard DTS:X profile's extension blocks.
    extension: ExtensionTally,
}

/// The extension blocks of the standard DTS:X profile, read and written
/// back: the fold matrix and the height set's headers.
#[derive(Default)]
struct ExtensionTally {
    blocks: usize,
    identical: usize,
    checksums_valid: usize,
    /// Blocks that are their headers, their data, [`dtsx::TRAILER`] zero
    /// bytes and zeroes to the word, and end the frame.
    laid_out: usize,
    refused: usize,
    first_refusal: Option<String>,
    /// Fold matrices by their bytes, the checksum and the byte after it
    /// included.
    matrices: BTreeMap<String, usize>,
    /// Height sets by their PCM and storage resolutions.
    resolutions: BTreeMap<(u8, u8), usize>,
    largest: usize,
    /// Navigation words, and those that point at the block of the frame the
    /// decoder takes out of its buffer with them.
    words: usize,
    words_agree: usize,
}

/// The DTS:X block of a lossless frame: at the first word after the bed's
/// band data, when one starts there, and to the frame's end.
fn block_after_bed(frame: &[u8], headers: &xll::Headers) -> Option<Navigation> {
    let start = (headers.len() + headers.band_data_len()).next_multiple_of(4);
    let block = frame
        .get(start..)
        .filter(|b| b.len() >= 4 && u32::from_be_bytes([b[0], b[1], b[2], b[3]]) == dtsx::SYNC)?;
    Some(Navigation {
        offset: start,
        size: block.len(),
    })
}

impl ExtensionTally {
    /// Read the block, if `frame` has one, at the word after its bed.
    fn frame(&mut self, frame: &[u8], headers: &xll::Headers, at: usize) {
        let Some(found) = block_after_bed(frame, headers) else {
            return;
        };
        let block = &frame[found.offset..];
        self.blocks += 1;
        self.largest = self.largest.max(block.len());
        let common = &headers.common;
        match dtsx::Block::read(block, common) {
            Ok(read) => {
                if read.checksums(block, common) {
                    self.checksums_valid += 1;
                }
                if matches!(read.write(common), Ok(w) if w[..] == block[..read.len(common)]) {
                    self.identical += 1;
                }
                let data_end = read.len(common) + read.heights.data_len();
                if read.size(common, read.heights.data_len()) == block.len()
                    && block
                        .get(data_end..)
                        .is_some_and(|t| t.iter().all(|&b| b == 0))
                {
                    self.laid_out += 1;
                }
                let matrix: String = block[..read.matrix.len()]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                *self.matrices.entry(matrix).or_default() += 1;
                let set = &read.heights.set;
                *self
                    .resolutions
                    .entry((set.pcm_bit_res, set.storage_bit_res))
                    .or_default() += 1;
            }
            Err(e) => {
                self.refused += 1;
                self.first_refusal
                    .get_or_insert_with(|| format!("frame at byte {at}: {}", e.0));
            }
        }
    }
}

impl LosslessTally {
    /// Take one asset's lossless data and check every frame it completes.
    fn asset(&mut self, data: &[u8], sync_offset: Option<u32>, one_to_one: bool, at: usize) {
        let starts =
            |d: &[u8]| d.len() >= 4 && u32::from_be_bytes([d[0], d[1], d[2], d[3]]) == xll::SYNC;
        if self.pending.is_empty() {
            let start = if starts(data) {
                0
            } else {
                match sync_offset.map(|o| o as usize) {
                    Some(o) if o < data.len() && starts(&data[o..]) => o,
                    _ => {
                        self.unsynchronised += 1;
                        return;
                    }
                }
            };
            self.pending.extend_from_slice(&data[start..]);
        } else {
            self.pending.extend_from_slice(data);
            self.spanning += 1;
        }
        while !self.pending.is_empty() {
            if !starts(&self.pending) {
                break;
            }
            let size = match xll::CommonHeader::read(&self.pending) {
                Ok(common) => common.frame_size as usize,
                // A header cut short by the substream's end: wait for the
                // rest. Common headers are at most 256 bytes.
                Err(_) if self.pending.len() < 256 => return,
                Err(_) => break,
            };
            if size > self.pending.len() {
                return;
            }
            let frame = &self.pending[..size];
            self.frames += 1;
            self.sizes.push(size as u32);
            self.bytes += size as u64;
            self.largest = self.largest.max(size);
            match xll::Headers::read(frame, one_to_one) {
                Ok(headers) => {
                    if headers.checksums(frame).all() {
                        self.checksums_valid += 1;
                    }
                    if matches!(headers.write(one_to_one), Ok(w) if w[..] == frame[..headers.len()])
                    {
                        self.identical += 1;
                    }
                    let shape: Vec<String> = headers
                        .channel_sets
                        .iter()
                        .map(|c| {
                            // The channels coded against the core, when any.
                            let full = (1u32 << c.channels) - 1;
                            let residual = full & !u32::from(c.residual_encode);
                            if residual == 0 {
                                format!("{}", c.channels)
                            } else {
                                format!("{} (residual {residual:#x})", c.channels)
                            }
                        })
                        .collect();
                    *self.shapes.entry(shape.join("+")).or_default() += 1;
                    self.extension.frame(frame, &headers, at);
                }
                Err(e) => {
                    self.refused += 1;
                    self.first_refusal
                        .get_or_insert_with(|| format!("frame at byte {at}: {}", e.0));
                }
            }
            self.pending.drain(..size);
        }
        // What is left does not start a frame: drop it, and resynchronise on
        // the next asset.
        self.pending.clear();
    }
}

/// The stream an `A_DTS` track carries: its blocks one after the other,
/// each of which has to be whole frames.
fn demux(path: &std::path::Path, file: &[u8]) -> Result<Vec<u8>> {
    use hz_matroska::{id, read};
    let malformed = |e: read::Malformed| Error::malformed(path, e.0);
    let (_, parts) = read::segment(file).map_err(malformed)?;
    let tracks = read::only(&parts, id::TRACKS).map_err(malformed)?;
    let mut number = None;
    for entry in read::children(file, tracks.at, tracks.end).map_err(malformed)? {
        let fields = read::children(file, entry.at, entry.end).map_err(malformed)?;
        let codec = read::only(&fields, id::CODEC_ID).map_err(malformed)?;
        if &file[codec.at..codec.end] == b"A_DTS" {
            number = Some(read::uint(
                file,
                read::only(&fields, id::TRACK_NUMBER).map_err(malformed)?,
            ));
            break;
        }
    }
    let number = number.ok_or_else(|| Error::malformed(path, "no A_DTS track"))?;
    let blocks = read::blocks(file, number).map_err(malformed)?;
    let padded = blocks
        .iter()
        .filter(|b| b.discard_padding.is_some())
        .count();
    println!(
        "matroska: track {number}, A_DTS, {} blocks, {} of them stating padding",
        blocks.len(),
        padded
    );
    Ok(blocks.iter().flat_map(|b| b.frame).copied().collect())
}

pub fn run(options: Options) -> Result<()> {
    let file = std::fs::read(&options.input).map_err(|e| Error::io(&options.input, e))?;
    let matroska = options
        .input
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mka") || e.eq_ignore_ascii_case("mkv"));
    let bytes = if matroska {
        demux(&options.input, &file)?
    } else {
        file
    };
    let limit = options.frames.unwrap_or(usize::MAX);
    let mut tally = Tally::default();
    let mut first_core = None;
    let mut first_exss = None;
    #[cfg(feature = "dca-oracle")]
    let mut oracle = oracle::Oracle::default();
    #[cfg(feature = "dca-oracle")]
    if options.reference.is_some() || options.unfold.is_some() {
        oracle.keep_pcm();
    }
    if let Some(unfold) = &options.unfold
        && hz_io::paths::aliases(&options.input, unfold)?
    {
        return Err(Error::refused(
            unfold,
            format!(
                "is {}, the stream it is unfolded from: writing it would destroy the stream",
                options.input.display()
            ),
        ));
    }
    if let Some(log) = &options.rate_log
        && hz_io::paths::aliases(&options.input, log)?
    {
        return Err(Error::refused(
            log,
            format!(
                "is {}, the stream whose rate it logs: writing it would destroy the stream",
                options.input.display()
            ),
        ));
    }
    #[cfg(not(feature = "dca-oracle"))]
    if options.unfold.is_some() {
        return Err(Error::refused(
            &options.input,
            "--unfold decodes through the oracle: it needs --features dca-oracle",
        ));
    }

    for frame in split::frames(&bytes).take(limit) {
        let frame = frame.map_err(|e| Error::malformed(&options.input, e.0))?;
        tally.frames += 1;
        tally
            .rate
            .packet(frame.core.map_or(0, <[u8]>::len) + frame.exss.map_or(0, <[u8]>::len));
        let mut navigation = None;
        if let Some(core) = frame.core {
            let header = CoreHeader::read(core).map_err(|e| {
                Error::malformed(&options.input, format!("core at {}: {}", frame.at, e.0))
            })?;
            let mut w = hz_core::bits::BitWriter::new();
            header.write(&mut w);
            tally.cores += 1;
            if w.bytes() == &core[..header.len()] {
                tally.cores_identical += 1;
            } else if tally.first_difference.is_none() {
                tally.first_difference = Some(format!("core header at byte {}", frame.at));
            }
            #[cfg(feature = "dca-oracle")]
            oracle.core(&header, core);
            *tally
                .dialnorm
                .entry((header.encoder_revision, header.dialnorm_code))
                .or_default() += 1;
            first_core.get_or_insert(header);
        }
        if let Some(exss) = frame.exss {
            let header = ExssHeader::read(exss).map_err(|e| {
                Error::malformed(
                    &options.input,
                    format!("substream at {}: {}", frame.at, e.0),
                )
            })?;
            tally.substreams += 1;
            if ExssHeader::crc_is_valid(exss, header.header_size) {
                tally.checksums_valid += 1;
            }
            match header.write() {
                Ok(written) if written == exss[..header.header_size as usize] => {
                    tally.substreams_identical += 1;
                }
                Ok(_) | Err(_) if tally.first_difference.is_none() => {
                    tally.first_difference = Some(format!(
                        "substream header at byte {}",
                        frame.at + frame.core.map_or(0, <[u8]>::len)
                    ));
                }
                _ => {}
            }
            navigation = navigate(&header, &mut tally);
            let lossless = lossless_asset(&header, exss);
            if let Some((data, sync, one_to_one)) = lossless {
                tally
                    .lossless
                    .asset(data, sync.map(|s| s.sync_offset), one_to_one, frame.at);
            }
            // The word describes the frame the decoder takes out of its
            // buffer with this substream, which under peak-rate smoothing is
            // not the frame the substream's data starts or completes.
            let decoded = lossless.and_then(|(data, sync, one_to_one)| {
                let frame = tally.rate.substream(data, sync)?;
                let headers = xll::Headers::read(frame, one_to_one).ok()?;
                block_after_bed(frame, &headers)
            });
            if let Some(word) = navigation {
                let extension = &mut tally.lossless.extension;
                extension.words += 1;
                extension.words_agree += usize::from(decoded == Some(word));
            }
            for asset in &header.assets {
                *tally.asset_dialnorm.entry(asset.dialnorm_code).or_default() += 1;
            }
            first_exss.get_or_insert(header);
        }
        // Every frame goes through the decoder, a core without an extension
        // substream as much as one with.
        #[cfg(feature = "dca-oracle")]
        oracle.frame(frame.core, frame.exss, navigation);
        #[cfg(not(feature = "dca-oracle"))]
        let _ = navigation;
    }

    println!("{}", options.input.display());
    println!("  frames                 {}", tally.frames);
    if let Some(core) = &first_core {
        println!(
            "  core                   {} ch{} · {} Hz · {} samples · {} bytes · rate code {}",
            core.channels(),
            ["", " + LFE", " + LFE"][usize::from(core.lfe)],
            core.sample_rate(),
            core.samples(),
            core.frame_size,
            core.bit_rate_code
        );
    }
    println!(
        "  core headers           {}/{} rewritten byte for byte",
        tally.cores_identical, tally.cores
    );
    if !tally.dialnorm.is_empty() {
        let core: Vec<String> = tally
            .dialnorm
            .iter()
            .map(|((v, d), n)| format!("revision {v} code {d} × {n}"))
            .collect();
        let asset: Vec<String> = tally
            .asset_dialnorm
            .iter()
            .map(|(d, n)| match d {
                Some(d) => format!("code {d} × {n}"),
                None => format!("absent × {n}"),
            })
            .collect();
        println!(
            "  dialnorm               core {}; asset {}",
            core.join(", "),
            if asset.is_empty() {
                "none".to_string()
            } else {
                asset.join(", ")
            }
        );
    }
    if let Some(exss) = &first_exss {
        describe(exss);
    }
    println!(
        "  substream headers      {}/{} rewritten byte for byte, {}/{} checksums valid",
        tally.substreams_identical, tally.substreams, tally.checksums_valid, tally.substreams
    );
    if let Some(at) = &tally.first_difference {
        println!("  first difference       {at}");
    }
    if tally.lossless_assets > 0 {
        let tails: Vec<String> = tally
            .tail_bits
            .iter()
            .map(|(bits, n)| format!("{bits} bits × {n}"))
            .collect();
        println!("  descriptor tails       {}", tails.join(", "));
        let found: usize = tally.navigation_at.values().sum();
        let at: Vec<String> = tally
            .navigation_at
            .iter()
            .map(|(bit, n)| format!("bit {bit} × {n}"))
            .collect();
        println!(
            "  navigation word        {found}/{} lossless assets{}",
            tally.lossless_assets,
            if at.is_empty() {
                String::new()
            } else {
                format!(", at {}", at.join(", "))
            }
        );
        if found > 0 {
            println!(
                "  largest navigated      offset {} of {} bytes, block {} of {} bytes",
                tally.navigation_max_offset,
                navigation::MAX_OFFSET,
                tally.navigation_max_size,
                navigation::MAX_SIZE
            );
            println!(
                "  navigated on average   offset {:.1} bytes, block {:.1} bytes",
                tally.navigation_offsets as f64 / found as f64,
                tally.navigation_sizes as f64 / found as f64
            );
        }
    }
    let lossless = &tally.lossless;
    if lossless.frames > 0 || lossless.unsynchronised > 0 {
        let shapes: Vec<String> = lossless
            .shapes
            .iter()
            .map(|(shape, n)| format!("{shape} × {n}"))
            .collect();
        println!(
            "  lossless headers       {}/{} rewritten byte for byte, {}/{} checksums valid; \
             channel sets {}",
            lossless.identical,
            lossless.frames,
            lossless.checksums_valid,
            lossless.frames,
            shapes.join(", ")
        );
        if lossless.frames > 0 {
            println!(
                "  lossless frames        {} bytes, {:.1} a frame, {} at most",
                lossless.bytes,
                lossless.bytes as f64 / lossless.frames as f64,
                lossless.largest
            );
        }
        if lossless.spanning > 0 || lossless.unsynchronised > 0 {
            println!(
                "                         {} substreams continue a frame, {} start without one",
                lossless.spanning, lossless.unsynchronised
            );
        }
        if let Some(why) = &lossless.first_refusal {
            println!(
                "  lossless refused       {} frames; first, {why}",
                lossless.refused
            );
        }
        report_extension(&lossless.extension);
    }
    let (samples, rate) = first_core
        .as_ref()
        .map_or((512, 48000), |c| (c.samples(), c.sample_rate()));
    tally.rate.report(samples, rate, &tally.lossless.sizes);
    if let Some(path) = &options.rate_log {
        tally.rate.write_log(path)?;
    }
    let mut failures = tally.rate.failures(samples, rate, options.ceiling);
    if tally.cores_identical != tally.cores || tally.substreams_identical != tally.substreams {
        failures.push("headers that do not rewrite byte for byte".to_string());
    }
    if lossless.identical != lossless.frames || lossless.refused > 0 {
        failures.push("lossless headers that do not rewrite byte for byte".to_string());
    }
    if lossless.checksums_valid != lossless.frames {
        failures.push("lossless header checksums that do not hold".to_string());
    }
    if tally.checksums_valid != tally.substreams {
        failures.push("substream checksums that do not hold".to_string());
    }
    let extension = &lossless.extension;
    if extension.identical != extension.blocks
        || extension.checksums_valid != extension.blocks
        || extension.laid_out != extension.blocks
    {
        failures.push("DTS:X blocks that do not rewrite, check or lay out as written".to_string());
    }
    if extension.words_agree != extension.words {
        failures.push(format!(
            "{} navigation words that miss the DTS:X block after the bed",
            extension.words - extension.words_agree
        ));
    }
    #[cfg(feature = "dca-oracle")]
    {
        oracle.report();
        failures.extend(oracle.failures());
        if let Some(path) = &options.unfold {
            write_unfolded(path, oracle.lossless(), oracle.heights(), oracle.folds())?;
        }
        if let (Some(reference), Some(decoded)) = (&options.reference, oracle.lossless()) {
            if reference.channels == 12 {
                failures.extend(compare_heights(reference, decoded, oracle.heights()));
            } else {
                let exact = compare_exact(reference, decoded);
                println!("  lossless round trip    {exact}");
                if !exact.is_exact() {
                    failures.push(format!(
                        "a lossless decode through dca that is not its input: {exact}"
                    ));
                }
            }
        } else if let Some(reference) = &options.reference {
            match oracle.pcm() {
                Some(pcm) if pcm.len() != reference.channels => failures.push(format!(
                    "a decode of {} channels against a reference of {}",
                    pcm.len(),
                    reference.channels
                )),
                Some(pcm) => match compare(reference, pcm) {
                    Some(comparison) => report_comparison(&comparison),
                    None => println!("  round trip             too short to compare"),
                },
                None => {}
            }
        }
    }
    #[cfg(not(feature = "dca-oracle"))]
    if options.reference.is_some() {
        println!("  round trip             needs --features dca-oracle to decode");
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::refused(&options.input, failures.join("; ")))
    }
}

fn report_extension(extension: &ExtensionTally) {
    if extension.blocks == 0 {
        return;
    }
    println!(
        "  DTS:X blocks           {}/{} rewritten byte for byte, {}/{} checksums valid, {}/{} \
         laid out as written (trailer and padding zero), {} bytes at most",
        extension.identical,
        extension.blocks,
        extension.checksums_valid,
        extension.blocks,
        extension.laid_out,
        extension.blocks,
        extension.largest
    );
    for (matrix, n) in &extension.matrices {
        println!("                         fold matrix {matrix} × {n}");
    }
    let resolutions: Vec<String> = extension
        .resolutions
        .iter()
        .map(|((pcm, storage), n)| format!("{pcm}/{storage} bits × {n}"))
        .collect();
    println!(
        "                         height sets {}; navigation words {}/{} point at the block \
         after the bed",
        resolutions.join(", "),
        extension.words_agree,
        extension.words
    );
    if let Some(why) = &extension.first_refusal {
        println!(
            "  DTS:X refused          {} blocks; first, {why}",
            extension.refused
        );
    }
}

#[cfg_attr(not(feature = "dca-oracle"), allow(dead_code))]
fn report_comparison(comparison: &Comparison) {
    const NAMES: [&str; 6] = ["L", "R", "C", "LFE", "Ls", "Rs"];
    let names: &[&str] = match comparison.snr_db.len() {
        1 => &["C"],
        2 => &["L", "R"],
        4 => &["L", "R", "Ls", "Rs"],
        5 => &["L", "R", "C", "Ls", "Rs"],
        _ => &NAMES,
    };
    let snr: Vec<String> = comparison
        .snr_db
        .iter()
        .zip(names)
        .map(|(snr, name)| match snr {
            Some(db) => format!("{name} {db:.2}"),
            None => format!("{name} silent"),
        })
        .collect();
    println!(
        "  round trip             lag {} samples; SNR dB {}",
        comparison.lag,
        snr.join(", ")
    );
}

/// The first lossless asset's data in a substream, its sync fields, and
/// whether it maps its channels one to one.
fn lossless_asset<'a>(
    header: &'a ExssHeader,
    exss: &'a [u8],
) -> Option<(&'a [u8], Option<&'a XllSync>, bool)> {
    let mut offset = header.header_size as usize;
    for (asset, &size) in header.assets.iter().zip(&header.asset_sizes) {
        let one_to_one = !matches!(
            asset.statics.as_ref().map(|s| &s.mapping),
            Some(ChannelMapping::Representation(_))
        );
        if let Coding::Components {
            core,
            xbr,
            xxch,
            x96,
            lbr,
            xll: Some(x),
            ..
        } = &asset.coding
        {
            // The components before it in the asset, in their order.
            let before = core.map_or(0, |(s, _)| s)
                + xbr.unwrap_or(0)
                + xxch.unwrap_or(0)
                + x96.unwrap_or(0)
                + lbr.as_ref().map_or(0, |l| l.size);
            offset += before as usize;
            let data = exss.get(offset..offset + x.size as usize)?;
            return Some((data, x.sync.as_ref(), one_to_one));
        }
        if let Coding::Lossless(x) = &asset.coding {
            let data = exss.get(offset..offset + x.size as usize)?;
            return Some((data, x.sync.as_ref(), one_to_one));
        }
        offset += size as usize;
    }
    None
}

/// The navigation word in the lossless asset's descriptor tail, if there is
/// one. Searched at every bit offset: where the tail's other fields end is
/// not established, and a word found at a varying offset would say so.
fn navigate(header: &ExssHeader, tally: &mut Tally) -> Option<Navigation> {
    let asset = header.assets.iter().find(|a| a.coding.xll().is_some())?;
    tally.lossless_assets += 1;
    *tally.tail_bits.entry(asset.tail.len()).or_default() += 1;
    let (bit, navigation) = find_word(&asset.tail)?;
    *tally.navigation_at.entry(bit).or_default() += 1;
    tally.navigation_max_offset = tally.navigation_max_offset.max(navigation.offset);
    tally.navigation_offsets += navigation.offset as u64;
    tally.navigation_sizes += navigation.size as u64;
    tally.navigation_max_size = tally.navigation_max_size.max(navigation.size);
    tally.navigation_max = Some(navigation);
    Some(navigation)
}

fn find_word(tail: &Bits) -> Option<(usize, Navigation)> {
    (0..tail.len().saturating_sub(63))
        .find_map(|bit| Navigation::read(tail.word64(bit)?).map(|n| (bit, n)))
}

fn describe(header: &ExssHeader) {
    println!(
        "  substream              index {} · {} header bytes{} · {} asset(s)",
        header.index,
        header.header_size,
        if header.wide { " (wide)" } else { "" },
        header.assets.len()
    );
    for asset in &header.assets {
        if let Some(s) = &asset.statics {
            println!(
                "  asset {}                {} ch · {} bits · rate code {} · {:?}",
                asset.index, s.channels, s.pcm_bit_res, s.max_sample_rate_code, s.mapping
            );
        }
        let coding = match &asset.coding {
            Coding::Components { mask, .. } => format!("components {mask:#05x}"),
            Coding::Lossless(_) => "lossless only".into(),
            Coding::LowBitRate(_) => "low bit rate".into(),
            Coding::Auxiliary { .. } => "auxiliary".into(),
        };
        let xll = asset.coding.xll().map_or(String::new(), |x| {
            format!(
                " · XLL sync {}",
                x.sync.as_ref().map_or("absent".into(), |s| format!(
                    "peak buffer code {}, delay {} frame(s)",
                    s.peak_buffer_code, s.delay_frames
                ))
            )
        });
        println!(
            "                         {coding}{xll} · descriptor {} bytes, tail {} bits",
            asset.size,
            asset.tail.len()
        );
    }
}

#[cfg(feature = "dca-oracle")]
mod oracle {
    use super::unfold;
    use dca::{FoldPlan, HdDecoder, HdError, PcmDecoder, XMetadata, XPresentation};
    use hz_dca::core::CoreHeader;
    use hz_dca::navigation::Navigation;

    const SCALE: f32 = 8_388_608.0;

    pub struct Oracle {
        /// Built with `new`, not `Default`: the derived default leaves the
        /// lossless decoder without its buffers.
        decoder: HdDecoder,
        /// For frames that are a core alone.
        core_decoder: PcmDecoder,
        cores: usize,
        cores_agree: usize,
        decoded: usize,
        decode_errors: usize,
        /// Frames the lossless decoder holds back while its peak-rate buffer
        /// fills: not a refusal.
        pending: usize,
        navigations: usize,
        navigations_agree: usize,
        /// Of those, located by the decoder's own reading of the word.
        navigations_by_word: usize,
        /// Extension blocks in frames whose descriptor has no word.
        blocks_without_word: usize,
        height_frames: usize,
        unfolded: usize,
        float_differs: usize,
        float_worst: i32,
        not_q15: usize,
        /// Frames by the presentation `dca` detects.
        presentations: std::collections::BTreeMap<String, usize>,
        metadata_refused: usize,
        /// Decoded core-only frames, per channel in WAV order, when asked.
        pcm: Option<Vec<Vec<f32>>>,
        /// Decoded lossless frames as the decoder's 24-bit integers, per
        /// channel in WAV order, when asked.
        lossless: Option<Vec<Vec<i32>>>,
        /// The standard profile's heights, TFL TFR TBL TBR, as 24-bit
        /// integers, when asked.
        heights: Option<Vec<Vec<i32>>>,
        /// For each frame whose heights are kept, its samples and the Q15
        /// gain each height was folded into each bed column at
        /// (`[height][column]`), as its own matrix states it — `None` for
        /// a matrix `hz_dca::dtsx` does not read.
        folds: Vec<(usize, Option<Vec<[i32; 8]>>)>,
    }

    impl Default for Oracle {
        fn default() -> Self {
            Self {
                decoder: HdDecoder::new(),
                core_decoder: PcmDecoder::new(),
                cores: 0,
                cores_agree: 0,
                decoded: 0,
                decode_errors: 0,
                pending: 0,
                navigations: 0,
                navigations_agree: 0,
                navigations_by_word: 0,
                blocks_without_word: 0,
                height_frames: 0,
                unfolded: 0,
                float_differs: 0,
                float_worst: 0,
                not_q15: 0,
                presentations: Default::default(),
                metadata_refused: 0,
                pcm: None,
                lossless: None,
                heights: None,
                folds: Vec::new(),
            }
        }
    }

    /// A channel's place in WAV order among those a core can carry.
    fn wav_rank(channel: dca::BedChannel) -> usize {
        use dca::BedChannel::*;
        match channel {
            FrontLeft => 0,
            FrontRight => 1,
            Center => 2,
            LowFrequencyEffects => 3,
            SurroundLeft => 4,
            SurroundRight => 5,
            RearCenter => 6,
            RearLeft => 7,
            RearRight => 8,
            WideLeft => 9,
            WideRight => 10,
        }
    }

    impl Oracle {
        /// Keep the PCM of core-only frames, for [`super::compare`].
        pub fn keep_pcm(&mut self) {
            self.pcm = Some(Vec::new());
            self.lossless = Some(Vec::new());
            self.heights = Some(Vec::new());
        }

        /// The heights of the frames dca presents as the standard height
        /// profile, when any did.
        pub fn heights(&self) -> Option<&[Vec<i32>]> {
            self.heights.as_deref().filter(|h| !h.is_empty())
        }

        /// The fold of each frame whose heights are kept: its samples, and
        /// its matrix's gains when `hz_dca::dtsx` reads it.
        pub fn folds(&self) -> &[(usize, Option<Vec<[i32; 8]>>)] {
            &self.folds
        }

        pub fn pcm(&self) -> Option<&[Vec<f32>]> {
            self.pcm.as_deref()
        }

        /// The lossless frames' samples, when any decoded.
        pub fn lossless(&self) -> Option<&[Vec<i32>]> {
            self.lossless.as_deref().filter(|l| !l.is_empty())
        }

        pub fn core(&mut self, ours: &CoreHeader, core: &[u8]) {
            self.cores += 1;
            // A header dca refuses is a disagreement too.
            if let Ok(theirs) = dca::parse_header(core) {
                let agree = theirs.frame_size == ours.frame_size as usize
                    && theirs.npcmblocks == ours.pcm_blocks
                    && theirs.sample_rate == ours.sample_rate()
                    && theirs.lfe_present == ours.lfe
                    && theirs.crc_present == ours.crc_present
                    && theirs.audio_mode.channel_count() == usize::from(ours.channels());
                self.cores_agree += usize::from(agree);
            }
        }

        pub fn frame(
            &mut self,
            core: Option<&[u8]>,
            exss: Option<&[u8]>,
            ours: Option<Navigation>,
        ) {
            let Some(exss) = exss else {
                // A core alone. The split only yields frames with at least
                // one half.
                match self.core_decoder.push_access_unit(core.unwrap_or(&[])) {
                    Ok(pushed) => {
                        self.decoded += 1;
                        if let Some(pcm) = &mut self.pcm {
                            let frame = pushed.pcm;
                            let mut planes: Vec<(usize, Vec<f32>)> = frame
                                .fullband_channel_order
                                .iter()
                                .map(|&c| wav_rank(c))
                                .zip(frame.fullband_channels)
                                .collect();
                            if let Some(lfe) = frame.lfe_channel {
                                planes.push((3, lfe));
                            }
                            planes.sort_by_key(|(rank, _)| *rank);
                            pcm.resize(planes.len(), Vec::new());
                            for (plane, (_, samples)) in pcm.iter_mut().zip(planes) {
                                plane.extend(samples);
                            }
                        }
                    }
                    Err(_) => self.decode_errors += 1,
                }
                return;
            };
            let frame = match self.decoder.decode(core.unwrap_or(&[]), exss) {
                Ok(frame) => frame,
                Err(HdError::Pending) => {
                    self.pending += 1;
                    return;
                }
                Err(_) => {
                    self.decode_errors += 1;
                    return;
                }
            };
            self.decoded += 1;
            if let Some(lossless) = &mut self.lossless {
                // dca's speaker slots, the side surrounds moved into Ls/Rs:
                // WAV's 7.1 is L R C LFE, the rears, the sides; its 5.1
                // L R C LFE Ls Rs.
                let speakers: Vec<(usize, &[i32])> = self.decoder.lossless_samples().collect();
                let rears = speakers.iter().any(|&(s, _)| s == 7 || s == 8);
                let order: &[usize] = if rears {
                    &[1, 2, 0, 5, 7, 8, 3, 4]
                } else {
                    &[1, 2, 0, 5, 3, 4]
                };
                let planes: Vec<&[i32]> = order
                    .iter()
                    .filter_map(|&want| speakers.iter().find(|&&(s, _)| s == want).map(|&(_, v)| v))
                    .collect();
                lossless.resize(planes.len().max(lossless.len()), Vec::new());
                for (plane, samples) in lossless.iter_mut().zip(planes) {
                    plane.extend_from_slice(samples);
                }
            }
            // The block the decoder located — by the word where its reader
            // takes it, by the end of the lossless band data where not. The
            // alternate profiles carry no word at all.
            if !frame.x_payload.is_empty() {
                let located = Navigation {
                    offset: frame.x_payload_offset,
                    size: frame.x_payload.len(),
                };
                match ours {
                    Some(ours) => {
                        self.navigations += 1;
                        self.navigations_agree += usize::from(ours == located);
                        self.navigations_by_word +=
                            usize::from(frame.x_descriptor_offset.is_some());
                    }
                    None => self.blocks_without_word += 1,
                }
            }
            let presentation = XPresentation::detect(&frame);
            let name = match presentation {
                Some(p) => format!("{p:?}"),
                None if frame.x_present => "extension without a presentation".into(),
                None => "no extension".into(),
            };
            *self.presentations.entry(name).or_default() += 1;
            if presentation != Some(XPresentation::Height) {
                return;
            }
            let Ok(metadata) = XMetadata::parse(&frame.x_payload, 4) else {
                self.metadata_refused += 1;
                return;
            };
            self.height_frames += 1;
            let plan = FoldPlan::from_metadata(&metadata);
            let heights: Vec<Vec<i32>> = frame
                .x_samples
                .iter()
                .map(|feed| feed.iter().map(|&s| (s * SCALE) as i32).collect())
                .collect();
            if let Some(kept) = &mut self.heights {
                kept.resize(heights.len().max(kept.len()), Vec::new());
                for (plane, samples) in kept.iter_mut().zip(&heights) {
                    plane.extend_from_slice(samples);
                }
                // The fold this frame states, read by our own reader from
                // the block dca located: what an unfold of it has to undo.
                let gains = hz_dca::dtsx::FoldMatrix::read(&frame.x_payload)
                    .ok()
                    .filter(|m| m.crc_is_valid(&frame.x_payload))
                    .and_then(|m| m.gains().ok())
                    .filter(|g| g.len() == heights.len());
                self.folds
                    .push((heights.first().map_or(0, Vec::len), gains));
            }
            let (mut unfolded, mut differs, mut worst) = (0usize, 0usize, 0i32);
            for (speaker, composite) in self.decoder.lossless_samples() {
                if !plan.touches(speaker) {
                    continue;
                }
                let mut q15 = [0i32; 4];
                for (feed, q) in q15.iter_mut().enumerate() {
                    let gain = plan.gain(speaker, feed) * 32768.0;
                    if gain.fract() != 0.0 {
                        self.not_q15 += 1;
                        return;
                    }
                    *q = gain as i32;
                }
                for (sample, &c) in composite.iter().enumerate() {
                    let mut exact = c;
                    for (feed, &q) in q15.iter().enumerate() {
                        if q != 0 {
                            exact = unfold::unfold(exact, heights[feed][sample], q);
                        }
                    }
                    let bed = c as f32 / SCALE;
                    let float =
                        (plan.clean(speaker, bed, sample, &frame.x_samples) * SCALE).round() as i32;
                    unfolded += 1;
                    if float != exact {
                        differs += 1;
                        worst = worst.max((float - exact).abs());
                    }
                }
            }
            self.unfolded += unfolded;
            self.float_differs += differs;
            self.float_worst = self.float_worst.max(worst);
        }

        /// What the oracle saw that should not be: a frame it refuses, or a
        /// field where it and we disagree. The floating-point helper's
        /// rounding is measured, not judged, and is not among them.
        pub fn failures(&self) -> Vec<String> {
            let mut failures = Vec::new();
            if self.decode_errors > 0 {
                failures.push(format!("dca refuses {} frames", self.decode_errors));
            }
            if self.cores_agree != self.cores {
                failures.push(format!(
                    "{} core headers where dca and we disagree",
                    self.cores - self.cores_agree
                ));
            }
            if self.navigations_agree != self.navigations {
                failures.push(format!(
                    "{} navigation words that miss the block dca located",
                    self.navigations - self.navigations_agree
                ));
            }
            if self.metadata_refused > 0 {
                failures.push(format!(
                    "{} height frames whose fold matrix dca refuses",
                    self.metadata_refused
                ));
            }
            failures
        }

        pub fn report(&self) {
            println!(
                "  oracle: core headers   {}/{} agree with dca",
                self.cores_agree, self.cores
            );
            println!(
                "  oracle: decoded        {} frames, {} refused{}",
                self.decoded,
                self.decode_errors,
                if self.pending > 0 {
                    format!(", {} held back by the peak-rate buffer", self.pending)
                } else {
                    String::new()
                }
            );
            println!(
                "  oracle: navigation     {}/{} point at the block dca located \
                 ({} of which dca's own reading of the word finds)",
                self.navigations_agree, self.navigations, self.navigations_by_word
            );
            if self.blocks_without_word > 0 {
                println!(
                    "  oracle: no word        {} frames carry an extension block and no \
                     navigation word",
                    self.blocks_without_word
                );
            }
            let shapes: Vec<String> = self
                .presentations
                .iter()
                .map(|(name, n)| format!("{name} × {n}"))
                .collect();
            println!("  oracle: presentation   {}", shapes.join(", "));
            if self.metadata_refused > 0 {
                println!(
                    "  oracle: metadata       {} height frames whose fold matrix dca refused",
                    self.metadata_refused
                );
            }
            if self.height_frames > 0 {
                println!(
                    "  oracle: height unfold  {} frames, {} corner samples; the float helper \
                     differs from the integer unfold on {} ({:.4} %), by at most {} LSB{}",
                    self.height_frames,
                    self.unfolded,
                    self.float_differs,
                    100.0 * self.float_differs as f64 / self.unfolded.max(1) as f64,
                    self.float_worst,
                    if self.not_q15 > 0 {
                        format!(
                            "; {} frames with a gain off the Q15 grid skipped",
                            self.not_q15
                        )
                    } else {
                        String::new()
                    }
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::unfold::*;
    use super::{Reference, compare};

    /// At 96 kb/s — rate code 3 — FFmpeg's encoder takes the lossless step
    /// sizes and its ADPCM stream decodes to noise; the port's has to decode
    /// to the tone it was made from. Through FFmpeg's decoder, which reads
    /// rate code 3 as 96 kb/s: the `dca` oracle reads it as lossless and
    /// decodes every core at that rate wrong (PLAN.md, phase C1). Skipped
    /// where there is no `ffmpeg`.
    #[test]
    fn adpcm_at_rate_code_3_decodes_to_its_input() {
        use hz_dca::encoder::{Config, Encoder, FRAME_SAMPLES, Layout};
        use std::process::Command;
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("no ffmpeg: rate code 3 not decoded");
            return;
        }
        let pcm: Vec<i32> = (0..16000)
            .map(|n| {
                let phase = std::f64::consts::TAU * 440.0 * f64::from(n) / 16000.0;
                (phase.sin() * f64::from(1 << 30)) as i32
            })
            .collect();
        let mut encoder = Encoder::new(Config {
            layout: Layout::Mono,
            sample_rate: 16000,
            bit_rate: 96_000,
            adpcm: true,
            ..Config::reference_five_one()
        })
        .unwrap();
        assert_eq!(encoder.header().bit_rate_code, 3);
        let mut stream = Vec::new();
        let mut frame = [0i32; FRAME_SAMPLES];
        for chunk in pcm.chunks(FRAME_SAMPLES) {
            frame[..chunk.len()].copy_from_slice(chunk);
            frame[chunk.len()..].fill(0);
            encoder.encode(&frame, &mut stream);
        }
        let path = std::env::temp_dir().join(format!("hz-dts-rate3-{}.dts", std::process::id()));
        std::fs::write(&path, &stream).unwrap();
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "dts", "-i"])
            .arg(&path)
            .args(["-f", "f32le", "-"])
            .output()
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(output.status.success());
        let decoded: Vec<f32> = output
            .stdout
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        // Scored at the core's delay, 512 samples: a lone tone correlates
        // almost as well a sample either side.
        let (mut signal, mut error) = (0.0f64, 0.0f64);
        for (n, &s) in pcm.iter().enumerate().skip(512).take(16000 - 1024) {
            let s = f64::from(s) / 2_147_483_648.0;
            signal += s * s;
            error += (s - f64::from(decoded[n + 512])).powi(2);
        }
        let snr = 10.0 * (signal / error).log10();
        assert!(snr > 60.0, "{snr} dB");
    }

    #[test]
    fn a_decode_too_short_to_compare_is_not_compared() {
        for len in [0, 1, 512, 1535] {
            let reference = Reference {
                pcm: vec![0; len],
                channels: 1,
            };
            for decoded_len in [0, len, len + 512] {
                assert_eq!(compare(&reference, &[vec![0.0; decoded_len]]), None);
            }
        }
    }

    #[test]
    fn a_delayed_decode_is_found_and_scored() {
        let channels = 2;
        let len = 48000;
        let mut pcm = Vec::with_capacity(len * channels);
        let mut seed = 0x9e37_79b9u32;
        for _ in 0..len {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            pcm.push((seed as i32) >> 2);
            pcm.push(0);
        }
        let reference = Reference { pcm, channels };
        let lag = 512;
        let mut decoded = vec![vec![0f32; len + lag]; channels];
        for n in 0..len {
            let s = reference.pcm[n * channels] as f64 / 2_147_483_648.0;
            // An error forty decibels down.
            decoded[0][n + lag] = (s * 1.01) as f32;
        }
        let comparison = compare(&reference, &decoded).unwrap();
        assert_eq!(comparison.lag, lag);
        assert!((comparison.snr_db[0].unwrap() - 40.0).abs() < 0.1);
        assert_eq!(comparison.snr_db[1], None);
    }

    #[test]
    fn the_unfold_undoes_the_fold_exactly() {
        let mut state = 0x1234_5678u32;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as i32) >> 9
        };
        for _ in 0..100_000 {
            let (bed, height) = (next() / 2, next());
            let composite = fold(bed, height, MINUS_3_DB);
            assert_eq!(unfold(composite, height, MINUS_3_DB), bed);
        }
    }

    #[test]
    fn a_sample_the_float_helper_was_seen_to_miss() {
        // Reported against the decoder's floating-point presentation path,
        // which returns it one LSB low; the integer unfold does not.
        let (bed, height) = (1_782_691, -3_535_125);
        assert_eq!(rmul15(height, MINUS_3_DB), -2_499_660);
        let composite = fold(bed, height, MINUS_3_DB);
        assert_eq!(composite, -716_969);
        assert_eq!(unfold(composite, height, MINUS_3_DB), bed);
    }

    #[cfg(feature = "dca-oracle")]
    fn run_on(name: &str, stream: &[u8]) -> hz_core::Result<()> {
        let dir = std::env::temp_dir().join(format!("hz-dts-oracle-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stream.dts");
        std::fs::write(&path, stream).unwrap();
        let result = super::run(super::Options {
            input: path,
            frames: None,
            reference: None,
            unfold: None,
            rate_log: None,
            ceiling: None,
        });
        std::fs::remove_dir_all(&dir).unwrap();
        result
    }

    #[cfg(feature = "dca-oracle")]
    #[test]
    fn a_core_the_oracle_refuses_fails_the_command() {
        use hz_dca::core::CoreHeader;
        let header = CoreHeader {
            normal_frame: true,
            crc_present: false,
            pcm_blocks: 16,
            frame_size: 2012,
            audio_mode: 9,
            sample_rate_code: 13,
            bit_rate_code: 24,
            drc_present: false,
            timestamp_present: false,
            aux_present: false,
            hdcd_master: false,
            ext_audio_type: 0,
            ext_audio_present: false,
            sync_ssf: true,
            lfe: 2,
            predictor_history: true,
            header_crc: None,
            filter_perfect: false,
            encoder_revision: 7,
            copy_history: 0,
            pcm_resolution_code: 6,
            sumdiff_front: false,
            sumdiff_surround: false,
            dialnorm_code: 0,
        };
        let mut w = hz_core::bits::BitWriter::new();
        header.write(&mut w);
        let mut stream = w.finish();
        stream.resize(2012, 0);
        let message = run_on("core", &stream)
            .expect_err("a refused core must fail the command")
            .to_string();
        assert!(message.contains("dca refuses 1 frames"), "{message}");
    }

    #[cfg(feature = "dca-oracle")]
    #[test]
    fn a_frame_the_oracle_refuses_fails_the_command() {
        use hz_dca::exss::{AssetDescriptor, Coding, ExssHeader, Xll};
        let mut header = ExssHeader {
            user_defined: 0,
            index: 0,
            wide: false,
            header_size: 0,
            size: 0,
            statics: None,
            asset_sizes: vec![64],
            assets: vec![AssetDescriptor {
                size: 0,
                index: 0,
                statics: None,
                drc_code: None,
                dialnorm_code: None,
                stereo_drc_code: None,
                mixing: None,
                coding: Coding::Lossless(Xll {
                    size: 64,
                    sync: None,
                }),
                hd_stream_id: Some(0),
                tail: Default::default(),
            }],
            backward_core: vec![None],
            reserved: Default::default(),
            crc: 0,
        };
        header.fit().unwrap();
        header.size = header.header_size + 64;
        let mut stream = header.write().unwrap();
        stream.resize(header.size as usize, 0);
        let result = run_on("exss", &stream);
        let message = result
            .expect_err("a refused frame must fail the command")
            .to_string();
        assert!(message.contains("dca refuses 1 frames"), "{message}");
    }

    /// A synthetic 7.1.4 stream, four frames, and its input: tones in the
    /// corners and the heights, at half scale.
    fn height_stream() -> (Vec<u8>, Vec<i32>) {
        use hz_dca::encoder::FRAME_SAMPLES;
        use hz_dca::lossless::Layout;
        use hz_dca::master::{Config, Encoder};
        let frames = 4;
        let pcm: Vec<i32> = (0..frames * FRAME_SAMPLES * 12)
            .map(|i| {
                let (n, ch) = (i / 12, i % 12);
                let f = 110.0 * (ch + 1) as f64;
                let x = (std::f64::consts::TAU * f * n as f64 / 48000.0).sin();
                ((x * 0.45 * f64::from(1 << 23)) as i32) << 8
            })
            .collect();
        let mut encoder = Encoder::new(Config::reference(Layout::SevenPointOneFour, 24)).unwrap();
        let mut stream = Vec::new();
        for frame in pcm.chunks(FRAME_SAMPLES * 12) {
            encoder.encode(frame, &mut stream).unwrap();
        }
        encoder.finish(&mut stream).unwrap();
        (stream, pcm)
    }

    /// `--unfold` never writes over the stream it reads, under any name.
    #[test]
    fn an_unfold_onto_its_own_stream_is_refused() {
        let dir = std::env::temp_dir().join(format!("hz-dts-unfold-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("stream.dts");
        let (stream, _) = height_stream();
        std::fs::write(&input, &stream).unwrap();
        let link = dir.join("unfolded.s32");
        std::fs::hard_link(&input, &link).unwrap();
        let result = super::run(super::Options {
            input: input.clone(),
            frames: None,
            reference: None,
            unfold: Some(link),
            rate_log: None,
            ceiling: None,
        });
        let after = std::fs::read(&input).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let message = result
            .expect_err("an unfold onto its own stream")
            .to_string();
        assert!(message.contains("destroy"), "{message}");
        assert_eq!(after, stream);
    }

    /// `--rate-log` never writes over the stream it reads either: not by its
    /// own name, not through a link to it.
    #[test]
    fn a_rate_log_onto_its_own_stream_is_refused() {
        let dir = std::env::temp_dir().join(format!("hz-dts-log-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("stream.dts");
        let (stream, _) = height_stream();
        std::fs::write(&input, &stream).unwrap();
        let link = dir.join("rate.csv");
        std::fs::hard_link(&input, &link).unwrap();
        for log in [input.clone(), link] {
            let result = super::run(super::Options {
                input: input.clone(),
                frames: None,
                reference: None,
                unfold: None,
                rate_log: Some(log.clone()),
                ceiling: None,
            });
            let message = result
                .expect_err("a rate log onto its own stream")
                .to_string();
            assert!(message.contains("destroy"), "{}: {message}", log.display());
            assert_eq!(std::fs::read(&input).unwrap(), stream);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A Matroska file's `A_DTS` track is read as the stream its blocks
    /// carry: a frame a block, as `harlettizer dts` writes it, gives back
    /// the raw stream byte for byte, and the command reads it.
    #[test]
    fn a_matroska_track_is_read_as_the_stream_it_carries() {
        use hz_matroska::{Muxer, Track};
        let (stream, _) = height_stream();
        let mut file = std::io::Cursor::new(Vec::new());
        let track = Track {
            codec_id: "A_DTS",
            codec_private: None,
            sample_rate: 48_000,
            frame: 512,
            channels: 8,
            bits: Some(24),
            codec_delay: None,
            seek_pre_roll: None,
            uid: 1,
            writing_app: "test",
        };
        let (mut muxer, _) = Muxer::start(&mut file, &track).unwrap();
        let mut frames = 0;
        for (n, frame) in hz_dca::split::frames(&stream).enumerate() {
            let frame = frame.unwrap();
            let len = frame.core.map_or(0, <[u8]>::len) + frame.exss.map_or(0, <[u8]>::len);
            muxer
                .block(&mut file, n as u64, &stream[frame.at..frame.at + len], 0)
                .unwrap();
            frames += 1;
        }
        muxer.finish(&mut file, frames * 512).unwrap();
        let file = file.into_inner();
        let path = std::path::Path::new("stream.mka");
        assert_eq!(super::demux(path, &file).unwrap(), stream);

        let dir = std::env::temp_dir().join(format!("hz-dts-matroska-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("stream.mka");
        std::fs::write(&input, &file).unwrap();
        let result = super::run(super::Options {
            input,
            frames: None,
            reference: None,
            unfold: None,
            rate_log: None,
            ceiling: None,
        });
        std::fs::remove_dir_all(&dir).unwrap();
        result.unwrap();
    }

    /// Each frame is unfolded with the fold its own matrix states: a stream
    /// whose TFL row is rewritten to unity in two frames, the checksum
    /// recomputed, unfolds those two at unity and the others at −3 dB.
    #[cfg(feature = "dca-oracle")]
    #[test]
    fn an_unfold_takes_each_frames_own_matrix() {
        use hz_dca::dtsx::{FoldMatrix, UNITY, fold};
        use hz_dca::encoder::FRAME_SAMPLES;
        let (mut stream, pcm) = height_stream();
        let mut standard = FoldMatrix::standard();
        standard.fit();
        let standard = standard.write().unwrap();
        let mut unity = FoldMatrix::standard();
        unity.rows[0].codes = vec![UNITY];
        unity.fit();
        let unity = unity.write().unwrap();
        assert_eq!(unity.len(), standard.len());
        let at: Vec<usize> = (0..stream.len() - standard.len())
            .filter(|&i| stream[i..i + standard.len()] == standard[..])
            .collect();
        assert_eq!(at.len(), 4);
        for &i in &at[1..3] {
            stream[i..i + unity.len()].copy_from_slice(&unity);
        }
        let dir = std::env::temp_dir().join(format!("hz-dts-unfold-gains-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (input, out) = (dir.join("stream.dts"), dir.join("unfolded.s32"));
        std::fs::write(&input, &stream).unwrap();
        let result = super::run(super::Options {
            input,
            frames: None,
            reference: None,
            unfold: Some(out.clone()),
            rate_log: None,
            ceiling: None,
        });
        let unfolded = std::fs::read(&out);
        std::fs::remove_dir_all(&dir).unwrap();
        result.unwrap();
        let unfolded: Vec<i32> = unfolded
            .unwrap()
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 8)
            .collect();
        assert_eq!(unfolded.len(), pcm.len());
        for (n, (got, input)) in unfolded.chunks(12).zip(pcm.chunks(12)).enumerate() {
            let (l, tfl) = (input[0] >> 8, input[8] >> 8);
            // The stream carries L with TFL folded in at −3 dB; frames 1 and
            // 2 now say unity, and are taken back out at unity.
            let composite = l + fold(tfl, 23170);
            let q = if (1..3).contains(&(n / FRAME_SAMPLES)) {
                32768
            } else {
                23170
            };
            assert_eq!(got[0], composite - fold(tfl, q), "sample {n}");
            assert_eq!(
                &got[1..],
                &input[1..].iter().map(|s| s >> 8).collect::<Vec<_>>()[..]
            );
        }
    }
}
