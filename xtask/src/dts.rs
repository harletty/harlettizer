//! Read a DTS stream's headers, write them back, and say what is in them.
//!
//! ```text
//! cargo xtask dts stream.dts [--frames N]
//! cargo run -p xtask --features dca-oracle -- dts stream.dts
//! ```
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
use hz_dca::exss::{Coding, ExssHeader};
use hz_dca::navigation::{self, Navigation};
use hz_dca::split;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Options {
    pub input: PathBuf,
    /// Stop after this many frames.
    pub frames: Option<usize>,
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
    navigation_max_size: usize,
    tail_bits: BTreeMap<usize, usize>,
    first_difference: Option<String>,
}

pub fn run(options: Options) -> Result<()> {
    let bytes = std::fs::read(&options.input).map_err(|e| Error::io(&options.input, e))?;
    let limit = options.frames.unwrap_or(usize::MAX);
    let mut tally = Tally::default();
    let mut first_core = None;
    let mut first_exss = None;
    #[cfg(feature = "dca-oracle")]
    let mut oracle = oracle::Oracle::default();

    for frame in split::frames(&bytes).take(limit) {
        let frame = frame.map_err(|e| Error::malformed(&options.input, e.0))?;
        tally.frames += 1;
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
            let navigation = navigate(&header, &mut tally);
            #[cfg(feature = "dca-oracle")]
            oracle.frame(frame.core, exss, navigation);
            #[cfg(not(feature = "dca-oracle"))]
            let _ = navigation;
            first_exss.get_or_insert(header);
        }
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
        }
    }
    let mut failures = Vec::new();
    if tally.cores_identical != tally.cores || tally.substreams_identical != tally.substreams {
        failures.push("headers that do not rewrite byte for byte".to_string());
    }
    if tally.checksums_valid != tally.substreams {
        failures.push("substream checksums that do not hold".to_string());
    }
    #[cfg(feature = "dca-oracle")]
    {
        oracle.report();
        failures.extend(oracle.failures());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::refused(&options.input, failures.join("; ")))
    }
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
    use dca::{FoldPlan, HdDecoder, XMetadata, XPresentation};
    use hz_dca::core::CoreHeader;
    use hz_dca::navigation::Navigation;

    const SCALE: f32 = 8_388_608.0;

    pub struct Oracle {
        /// Built with `new`, not `Default`: the derived default leaves the
        /// lossless decoder without its buffers.
        decoder: HdDecoder,
        cores: usize,
        cores_agree: usize,
        decoded: usize,
        decode_errors: usize,
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
    }

    impl Default for Oracle {
        fn default() -> Self {
            Self {
                decoder: HdDecoder::new(),
                cores: 0,
                cores_agree: 0,
                decoded: 0,
                decode_errors: 0,
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
            }
        }
    }

    impl Oracle {
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

        pub fn frame(&mut self, core: Option<&[u8]>, exss: &[u8], ours: Option<Navigation>) {
            let core = core.unwrap_or(&[]);
            let frame = match self.decoder.decode(core, exss) {
                Ok(frame) => frame,
                Err(_) => {
                    self.decode_errors += 1;
                    return;
                }
            };
            self.decoded += 1;
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
                "  oracle: decoded        {} frames, {} refused",
                self.decoded, self.decode_errors
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
        let dir = std::env::temp_dir().join(format!("hz-dts-oracle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("refused.dts");
        std::fs::write(&path, &stream).unwrap();
        let result = super::run(super::Options {
            input: path.clone(),
            frames: None,
        });
        std::fs::remove_dir_all(&dir).unwrap();
        let message = result
            .expect_err("a refused frame must fail the command")
            .to_string();
        assert!(message.contains("dca refuses 1 frames"), "{message}");
    }
}
