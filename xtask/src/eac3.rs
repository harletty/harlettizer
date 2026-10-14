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
        if report.read_mismatches > 0 && failed.is_none() {
            failed = Some(input.clone());
        }
    }
    match failed {
        Some(path) => Err(Error::Malformed {
            path,
            detail: "our reading of an Evolution frame disagrees with the oracle's".into(),
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
