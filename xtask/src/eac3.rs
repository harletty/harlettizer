//! Read E-AC-3 streams somebody else wrote and say what their objects ride in.
//!
//! ```text
//! cargo run -p xtask --features eac3-oracle -- eac3 stream.ec3 [more.ec3 …] [--dump DIR]
//! ```
//!
//! The groundwork for the lossy track (PLAN.md, Track B). Before this project
//! writes a single syncframe with objects in it, it has to know what the
//! reference streams carry — which substreams, which payloads, which
//! protection lengths — and its own reader of the Evolution frame has to read
//! every one of them the way an independent decoder does. So each stream goes
//! through the workspace's `eac3` decoder, used as an oracle, and every
//! Evolution frame it finds is read again by [`hz_meta::emdf`] and then
//! rewritten by it:
//!
//! - **read**: our payloads — identifiers, sample offsets, bytes — must be the
//!   oracle's, frame for frame. A disagreement is a bug in one of the two.
//! - **rewritten**: the frame our writer produces from those payloads must be
//!   the reference's bit for bit up to the protection fields. A disagreement
//!   here is not a bug but a configuration the writer does not produce, and
//!   the report says how often.
//!
//! `--dump` keeps every payload, so that the writers of the object metadata
//! and joint object coding payloads can be held to the reference byte for
//! byte without decoding a stream each time. The file is `<stem>.emdf`: the
//! eight bytes `HZEMDF1\n`, then per payload, little-endian — the syncframe's
//! index (u32), its substream identifier (u8), the payload identifier (u16),
//! the sample offset (i32, −1 when absent), the length (u32) and the bytes.
//!
//! Only elementary streams are read; take one out of a container with
//! `ffmpeg -i film.mkv -map 0:a:N -c copy -f eac3 dumps/…/film.ec3`.

use eac3::{AccessUnitParseError, Decoder, EmdfSource, Extractor, ParsedEmdfPayloadData};
use hz_core::bits::BitWriter;
use hz_core::{Error, Result};
use hz_meta::emdf;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct Options {
    pub inputs: Vec<PathBuf>,
    /// Write each stream's payloads to `<stem>.emdf` in this directory.
    pub dump: Option<PathBuf>,
    /// Stop after this many syncframes per stream.
    pub frames: Option<usize>,
}

pub fn run(options: Options) -> Result<()> {
    let dumps = dump_paths(&options)?;
    if let Some(dir) = &options.dump {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    let mut failed = None;
    for (input, dump) in options.inputs.iter().zip(&dumps) {
        let report = survey(input, dump.as_deref(), options.frames)?;
        report.print(input);
        let joc_failed =
            report.joc_rewritten != report.joc_payloads || report.joc_agreed != report.joc_payloads;
        if (report.read_mismatches > 0 || joc_failed) && failed.is_none() {
            failed = Some(input.clone());
        }
    }
    match failed {
        Some(path) => Err(Error::Malformed {
            path,
            detail: "our reading or writing of an Evolution frame or a joint object coding \
                     payload disagrees with the reference"
                .into(),
        }),
        None => Ok(()),
    }
}

/// Where each input's payloads go, refused up front when two inputs would
/// share a file — `a/film.ec3` and `b/film.ec3` both name `film.emdf`, and
/// the second would silently replace the first.
fn dump_paths(options: &Options) -> Result<Vec<Option<PathBuf>>> {
    let Some(dir) = &options.dump else {
        return Ok(vec![None; options.inputs.len()]);
    };
    let mut claimed: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    let mut paths = Vec::with_capacity(options.inputs.len());
    for input in &options.inputs {
        let stem = input.file_stem().unwrap_or_default().to_string_lossy();
        let path = dir.join(format!("{stem}.emdf"));
        if let Some(earlier) = claimed.insert(path.clone(), input) {
            return Err(Error::Refused {
                path,
                detail: format!(
                    "both {} and {} would dump here; rename one, or dump them separately",
                    earlier.display(),
                    input.display()
                ),
            });
        }
        paths.push(Some(path));
    }
    Ok(paths)
}

/// What one stream holds.
#[derive(Default)]
struct Report {
    /// Joint object coding payloads `hz-joc` read, wrote back byte for byte,
    /// and read the same as the oracle; the first that failed each.
    joc_payloads: usize,
    joc_rewritten: usize,
    joc_agreed: usize,
    first_joc_failure: Option<String>,
    /// What the payloads use: sparse and dense objects, steep data points,
    /// unflagged tails, non-zero padding, bytes past it.
    joc_shapes: BTreeMap<String, usize>,
    /// Syncframes by kind: independent, dependent, legacy AC-3, and ones that
    /// did not parse.
    independent: usize,
    dependent: usize,
    legacy: usize,
    unreadable: usize,
    /// The shape of each substream: (independent?, identifier) → (channel
    /// mode code, LFE, kb/s, count).
    substreams: BTreeMap<(bool, u8), (u8, bool, u32, usize)>,
    sample_rates: BTreeMap<u32, usize>,
    /// Evolution frames, and where the oracle found them.
    frames: usize,
    frames_by_source: BTreeMap<String, usize>,
    /// Where the auxiliary data is carried: the skip fields of which blocks,
    /// how many bytes each; and the frame's trailing auxiliary data.
    skip_fields: BTreeMap<String, usize>,
    /// Which blocks carry a skip field, whatever its size.
    skip_blocks: BTreeMap<String, usize>,
    trailing_aux: BTreeMap<usize, usize>,
    /// Payload identifier → (count, total bytes, carrying a sample offset).
    payloads: BTreeMap<u32, (usize, usize, usize)>,
    /// Payload identifier → each configuration it was written with → count.
    configs: BTreeMap<u32, BTreeMap<String, usize>>,
    /// (primary, secondary) protection bits → count.
    protection: BTreeMap<(u32, u32), usize>,
    /// Frames our reader read the same as the oracle, and ones it did not.
    read_matches: usize,
    read_mismatches: usize,
    first_read_mismatch: Option<String>,
    /// Frames our writer reproduced up to the protection fields.
    rewritten: usize,
    first_rewrite_mismatch: Option<String>,
    /// Payloads the oracle could not parse.
    payload_errors: BTreeMap<u32, usize>,
    /// Joint object coding: downmix configuration → count; object and
    /// channel counts seen.
    joc_downmix: BTreeMap<u8, usize>,
    joc_objects: BTreeMap<usize, usize>,
    joc_channels: BTreeMap<usize, usize>,
    oamd_objects: BTreeMap<usize, usize>,
}

/// Bytes read from a stream at a time.
///
/// The stream is fed to the extractor in pieces and drained between them,
/// never whole: the extractor compacts its buffer by moving what is left to
/// the front, and with a whole film behind the cursor that is the whole film
/// moved again every few frames.
const CHUNK: usize = 8 * 1024;

fn survey(input: &Path, dump: Option<&Path>, frames: Option<usize>) -> Result<Report> {
    let mut source = std::fs::File::open(input).map_err(|e| Error::io(input, e))?;
    let mut chunk = vec![0u8; CHUNK];
    let mut eof = false;
    let limit = frames.unwrap_or(usize::MAX);
    let mut dump = match dump {
        Some(path) => {
            let path = path.to_path_buf();
            let mut file = std::io::BufWriter::new(
                std::fs::File::create(&path).map_err(|e| Error::io(&path, e))?,
            );
            file.write_all(b"HZEMDF1\n")
                .map_err(|e| Error::io(&path, e))?;
            Some((path, file))
        }
        None => None,
    };

    let mut report = Report::default();
    let mut extractor = Extractor::default();
    let mut decoder = Decoder::new();
    let mut index = 0usize;

    while index < limit {
        let next = next_frame(&mut extractor, &mut source, &mut chunk, &mut eof)
            .map_err(|e| Error::io(input, e))?;
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                report.unreadable += 1;
                report.first_read_mismatch.get_or_insert(format!(
                    "the extractor stopped at syncframe {index}: {error:?}"
                ));
                break;
            }
        };
        let header = frame.info();
        *report.sample_rates.entry(header.sample_rate).or_default() += 1;

        let info = match decoder.push_access_unit(frame.as_bytes()) {
            Ok(pushed) => pushed.info,
            Err(AccessUnitParseError::NotEac3) => {
                report.legacy += 1;
                index += 1;
                continue;
            }
            Err(_) => {
                report.unreadable += 1;
                index += 1;
                continue;
            }
        };

        let independent = !matches!(header.stream_type, eac3::parser::StreamType::Dependent);
        if independent {
            report.independent += 1;
        } else {
            report.dependent += 1;
        }
        let shape = report
            .substreams
            .entry((independent, info.substreamid))
            .or_insert((info.channel_mode, info.lfe_on, header.bitrate() / 1000, 0));
        shape.3 += 1;

        let fields = info
            .skip_fields
            .iter()
            .map(|field| {
                format!(
                    "block {} {} B",
                    field.block_index.map_or("?".to_string(), |b| b.to_string()),
                    field.bytes.len()
                )
            })
            .collect::<Vec<_>>()
            .join(" + ");
        if !fields.is_empty() {
            *report.skip_fields.entry(fields).or_default() += 1;
        }
        let blocks = info
            .skip_fields
            .iter()
            .map(|field| field.block_index.map_or("?".to_string(), |b| b.to_string()))
            .collect::<Vec<_>>()
            .join("+");
        *report
            .skip_blocks
            .entry(if blocks.is_empty() {
                "none".into()
            } else {
                blocks
            })
            .or_default() += 1;
        *report
            .trailing_aux
            .entry(info.trailing_aux_data.len())
            .or_default() += 1;

        // Where the oracle's sync offsets count from.
        let carrier: &[u8] = match info.emdf_source {
            EmdfSource::AuxData => &info.aux_data,
            EmdfSource::FrameScanFallback => frame.as_bytes(),
            EmdfSource::None => &[],
        };
        for block in &info.emdf_blocks {
            report.frames += 1;
            *report
                .frames_by_source
                .entry(info.emdf_source.to_string())
                .or_default() += 1;
            for payload in &block.payloads {
                let id = u32::from(payload.info.payload_id);
                let entry = report.payloads.entry(id).or_default();
                entry.0 += 1;
                entry.1 += payload.bytes.len();
                entry.2 += usize::from(payload.info.sample_offset.is_some());
                if payload.parse_error.is_some() {
                    *report.payload_errors.entry(id).or_default() += 1;
                }
                match &payload.parsed {
                    ParsedEmdfPayloadData::Joc(joc) => {
                        *report.joc_downmix.entry(joc.downmix_config).or_default() += 1;
                        *report.joc_objects.entry(joc.object_count).or_default() += 1;
                        *report.joc_channels.entry(joc.channel_count).or_default() += 1;
                        check_joc(&mut report, index, &payload.bytes, joc);
                    }
                    ParsedEmdfPayloadData::Oamd(oamd) => {
                        *report.oamd_objects.entry(oamd.object_count).or_default() += 1;
                    }
                    ParsedEmdfPayloadData::Unknown => {}
                }
                if let Some((path, file)) = &mut dump {
                    write_record(file, index, info.substreamid, id, payload)
                        .map_err(|e| Error::io(&*path, e))?;
                }
            }
            check_frame(&mut report, carrier, block, index);
        }
        index += 1;
    }

    if let Some((path, mut file)) = dump {
        file.flush().map_err(|e| Error::io(&path, e))?;
    }
    Ok(report)
}

/// The next whole syncframe, reading the stream one chunk at a time until the
/// extractor has one or the stream ends.
fn next_frame(
    extractor: &mut Extractor,
    source: &mut impl Read,
    chunk: &mut [u8],
    eof: &mut bool,
) -> std::io::Result<std::result::Result<Option<eac3::Frame>, eac3::ExtractError>> {
    loop {
        match extractor.next_frame() {
            Ok(None) if !*eof => {
                let read = source.read(chunk)?;
                if read == 0 {
                    *eof = true;
                } else {
                    extractor.push_bytes(&chunk[..read]);
                }
            }
            other => return Ok(other),
        }
    }
}

/// Read a joint object coding payload with `hz-joc`, write it back, and hold
/// what it read to what the oracle read.
fn check_joc(report: &mut Report, index: usize, bytes: &[u8], oracle: &eac3::JocPayload) {
    report.joc_payloads += 1;
    let ours = match hz_joc::read(bytes) {
        Ok(ours) => ours,
        Err(e) => {
            report
                .first_joc_failure
                .get_or_insert(format!("syncframe {index}: hz-joc cannot read it: {e}"));
            return;
        }
    };
    let shapes = &mut report.joc_shapes;
    let mut count = |what: String| *shapes.entry(what).or_default() += 1;
    for object in ours.objects.iter().flatten() {
        count(if object.sparse() { "sparse" } else { "dense" }.into());
        count(if object.fine { "fine" } else { "coarse" }.into());
        count(format!("{:02} bands", object.bands()));
        if object.steep {
            count("steep".into());
        }
        if object.points() == 2 {
            count("two points".into());
        }
    }
    if ours.unflagged_tail > 0 {
        count("unflagged tail".into());
    }
    if ours.padding != 0 {
        count("non-zero padding".into());
    }
    if !ours.trailing.is_empty() {
        count("bytes past the padding".into());
    }

    if ours.to_bytes() == bytes {
        report.joc_rewritten += 1;
    } else {
        report
            .first_joc_failure
            .get_or_insert(format!("syncframe {index}: written back, the bytes differ"));
    }
    match joc_disagreement(&ours, oracle) {
        None => report.joc_agreed += 1,
        Some(what) => {
            report
                .first_joc_failure
                .get_or_insert(format!("syncframe {index}: against the oracle, {what}"));
        }
    }
}

/// The first field `hz-joc` and the oracle read differently, if any.
fn joc_disagreement(ours: &hz_joc::Joc, oracle: &eac3::JocPayload) -> Option<String> {
    if u8::from(oracle.downmix_config) != ours.downmix_config {
        return Some("the downmix configuration".into());
    }
    if oracle.object_count != ours.objects.len() || oracle.objects.len() != ours.objects.len() {
        return Some("the object count".into());
    }
    if oracle.sequence_counter != ours.sequence {
        return Some("the sequence count".into());
    }
    let (x, y) = ours.clip_gain;
    let gain = 1.0 + f32::from(y) / 32.0 * 2f32.powi(i32::from(x) - 4);
    if (gain - oracle.gain).abs() > 1e-6 {
        return Some("the clip gain".into());
    }
    for (i, (o, theirs)) in ours.objects.iter().zip(&oracle.objects).enumerate() {
        let Some(o) = o else {
            if theirs.active {
                return Some(format!("object {i}'s presence"));
            }
            continue;
        };
        let header = theirs.active
            && theirs.bands_index == Some(o.bands_index)
            && theirs.sparse_coded == o.sparse()
            && theirs.quantization_table == Some(u8::from(o.fine))
            && theirs.steep_slope == o.steep
            && theirs.data_points == o.points()
            && theirs
                .timeslot_offsets
                .iter()
                .map(|t| t - 1)
                .eq(o.offsets.iter().copied());
        if !header {
            return Some(format!("object {i}'s header"));
        }
        let data = match (&theirs.data, &o.data) {
            (
                Some(eac3::JocObjectData::Sparse {
                    channel_indices,
                    vectors,
                }),
                hz_joc::Data::Sparse(points),
            ) => {
                points.len() == channel_indices.len()
                    && points.iter().zip(channel_indices.iter().zip(vectors)).all(
                        |(point, (channels, vector))| {
                            point.channels == *channels && point.vector == *vector
                        },
                    )
            }
            (Some(eac3::JocObjectData::Dense { matrices }), hz_joc::Data::Dense(points)) => {
                matrices == points
            }
            _ => false,
        };
        if !data {
            return Some(format!("object {i}'s data"));
        }
    }
    None
}

/// Read the frame at the oracle's sync offset ourselves, compare, and rewrite.
fn check_frame(report: &mut Report, carrier: &[u8], block: &eac3::EmdfBlockInfo, index: usize) {
    let Some((ours, end)) = emdf::read_synced(carrier, block.sync_offset) else {
        report.read_mismatches += 1;
        report.first_read_mismatch.get_or_insert(format!(
            "syncframe {index}: no frame of ours at byte {}",
            block.sync_offset
        ));
        return;
    };
    *report.protection.entry(ours.protection).or_default() += 1;
    for payload in &ours.payloads {
        *report
            .configs
            .entry(payload.id)
            .or_default()
            .entry(describe(payload.sample_offset, &payload.config))
            .or_default() += 1;
    }

    let same = ours.payloads.len() == block.payloads.len()
        && ours
            .payloads
            .iter()
            .zip(&block.payloads)
            .all(|(ours, theirs)| {
                ours.id == u32::from(theirs.info.payload_id)
                    && ours.sample_offset == theirs.info.sample_offset.map(u32::from)
                    && ours.bytes == theirs.bytes
            });
    if same {
        report.read_matches += 1;
    } else {
        report.read_mismatches += 1;
        report.first_read_mismatch.get_or_insert(format!(
            "syncframe {index}: ours {:?} against the oracle's {:?}",
            ours.payloads
                .iter()
                .map(|p| (p.id, p.sample_offset, p.bytes.len()))
                .collect::<Vec<_>>(),
            block
                .payloads
                .iter()
                .map(|p| (p.info.payload_id, p.info.sample_offset, p.bytes.len()))
                .collect::<Vec<_>>(),
        ));
        return;
    }

    let Some(protection) = protection_of(ours.protection) else {
        report.first_rewrite_mismatch.get_or_insert(format!(
            "syncframe {index}: protection lengths {:?} have no code",
            ours.protection
        ));
        return;
    };
    let payloads: Vec<emdf::Payload<'_>> = ours
        .payloads
        .iter()
        .map(|payload| emdf::Payload {
            id: payload.id,
            bytes: &payload.bytes,
            sample_offset: payload.sample_offset.unwrap_or(0),
            config: payload.config,
        })
        .collect();
    let mut writer = BitWriter::with_capacity(end - block.sync_offset);
    let at = emdf::write(&mut writer, &payloads, protection);
    let written = writer.finish();
    let original = &carrier[block.sync_offset + 4..end];
    match first_difference(&written, original, at.primary) {
        None => report.rewritten += 1,
        Some(bit) => {
            report.first_rewrite_mismatch.get_or_insert(format!(
                "syncframe {index}: the first differing bit is {bit} of {}",
                at.primary
            ));
        }
    }
}

fn protection_of((primary, secondary): (u32, u32)) -> Option<emdf::Protection> {
    let length = |bits: u32| match bits {
        8 => Some(emdf::ProtectionLength::Bits8),
        32 => Some(emdf::ProtectionLength::Bits32),
        128 => Some(emdf::ProtectionLength::Bits128),
        _ => None,
    };
    Some(emdf::Protection {
        primary: length(primary)?,
        secondary: if secondary == 0 {
            None
        } else {
            Some(length(secondary)?)
        },
    })
}

/// The first of the leading `bits` bits at which `a` and `b` differ.
fn first_difference(a: &[u8], b: &[u8], bits: usize) -> Option<usize> {
    let bit = |bytes: &[u8], n: usize| bytes.get(n / 8).map(|byte| byte >> (7 - n % 8) & 1);
    (0..bits).find(|n| bit(a, *n) != bit(b, *n))
}

fn write_record(
    file: &mut impl Write,
    index: usize,
    substream: u8,
    id: u32,
    payload: &eac3::EmdfPayloadInfo,
) -> std::io::Result<()> {
    file.write_all(&(index as u32).to_le_bytes())?;
    file.write_all(&[substream])?;
    file.write_all(&(id as u16).to_le_bytes())?;
    let offset = payload.info.sample_offset.map_or(-1, i32::from);
    file.write_all(&offset.to_le_bytes())?;
    file.write_all(&(payload.bytes.len() as u32).to_le_bytes())?;
    file.write_all(&payload.bytes)
}

impl Report {
    fn print(&self, input: &Path) {
        println!("{}", input.display());
        println!(
            "  syncframes   {} independent, {} dependent, {} legacy AC-3, {} unreadable",
            self.independent, self.dependent, self.legacy, self.unreadable
        );
        println!("  sample rate  {}", histogram(&self.sample_rates));
        for ((independent, id), (mode, lfe, kbps, count)) in &self.substreams {
            println!(
                "  substream    {} {id}: acmod {mode}{}, {kbps} kb/s, {count} frames",
                if *independent {
                    "independent"
                } else {
                    "dependent"
                },
                if *lfe { " + LFE" } else { "" },
            );
        }
        println!(
            "  evolution    {} frames ({})",
            self.frames,
            histogram(&self.frames_by_source)
        );
        if !self.skip_fields.is_empty() {
            // The most common layouts only: payload sizes vary frame to frame.
            let mut layouts: Vec<_> = self.skip_fields.iter().collect();
            layouts.sort_by(|a, b| b.1.cmp(a.1));
            println!("  skip blocks  {}", histogram(&self.skip_blocks));
            println!(
                "  skip fields  {} layouts; most common {}",
                layouts.len(),
                layouts
                    .iter()
                    .take(3)
                    .map(|(layout, count)| format!("[{layout}] ×{count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        println!("  trailing aux {} bytes", histogram(&self.trailing_aux));
        for (id, (count, bytes, offsets)) in &self.payloads {
            println!(
                "  payload {id:>3}  {} ×{count}, {:.1} bytes each, {offsets} with a sample offset{}",
                name(*id),
                *bytes as f64 / *count as f64,
                match self.payload_errors.get(id) {
                    Some(errors) => format!(", {errors} the oracle could not parse"),
                    None => String::new(),
                }
            );
        }
        for (id, configs) in &self.configs {
            println!("  config {id:>4}  {}", histogram(configs));
        }
        println!(
            "  protection   {}",
            self.protection
                .iter()
                .map(|((p, s), count)| format!("{p}+{s} bits ×{count}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !self.joc_downmix.is_empty() {
            println!(
                "  joc          downmix config {}; {} channels; {} objects",
                histogram(&self.joc_downmix),
                histogram(&self.joc_channels),
                histogram(&self.joc_objects)
            );
        }
        if self.joc_payloads > 0 {
            println!(
                "  joc payload  {}/{} rewritten byte for byte by hz-joc, {}/{} read as the oracle reads them; {}",
                self.joc_rewritten,
                self.joc_payloads,
                self.joc_agreed,
                self.joc_payloads,
                histogram(&self.joc_shapes)
            );
            if let Some(first) = &self.first_joc_failure {
                println!("               first failure: {first}");
            }
        }
        if !self.oamd_objects.is_empty() {
            println!("  oamd         {} objects", histogram(&self.oamd_objects));
        }
        println!(
            "  read         {}/{} frames as the oracle does",
            self.read_matches,
            self.read_matches + self.read_mismatches
        );
        if let Some(first) = &self.first_read_mismatch {
            println!("               first disagreement: {first}");
        }
        println!(
            "  rewritten    {}/{} frames bit for bit up to the protection fields",
            self.rewritten, self.read_matches
        );
        if let Some(first) = &self.first_rewrite_mismatch {
            println!("               first difference: {first}");
        }
    }
}

/// A payload's configuration in a few words, for the report.
fn describe(sample_offset: Option<u32>, config: &emdf::Config) -> String {
    let mut words = Vec::new();
    if let Some(offset) = sample_offset {
        words.push(format!("offset {offset}"));
    }
    if let Some(duration) = config.duration {
        words.push(format!("duration {duration}"));
    }
    if let Some(group) = config.group {
        words.push(format!("group {group}"));
    }
    if let Some(data) = config.codec_data {
        words.push(format!("codec data {data:#04x}"));
    }
    match config.retain {
        None => words.push("discard if unknown".into()),
        Some(retain) => {
            if let Some((create, remove)) = retain.frame_aligned {
                words.push(format!("frame aligned (create {create}, remove {remove})"));
            }
            words.push(format!(
                "keep, priority {} processing {}",
                retain.priority, retain.processing
            ));
        }
    }
    words.join(", ")
}

fn name(id: u32) -> &'static str {
    match id {
        emdf::LOUDNESS => "programme loudness",
        emdf::PROGRAMME_INFORMATION => "programme information",
        emdf::OAMD => "object metadata",
        emdf::JOC => "joint object coding",
        _ => "unknown",
    }
}

fn histogram<K: std::fmt::Display>(counts: &BTreeMap<K, usize>) -> String {
    counts
        .iter()
        .map(|(key, count)| format!("{key} ×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}
