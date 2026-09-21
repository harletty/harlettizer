//! `harlettizer convert` — a master file set to an ADM BW64 file, or back.
//!
//! The audio is carried sample for sample. A master's component is 24-bit
//! big-endian and a BW64's is little-endian, which is a byte order and not a
//! conversion: every sample that goes in comes out, and nothing is scaled,
//! dithered or rounded on the way.

use hz_core::{Error, Result};
use hz_io::adm::bw64;
use hz_io::container::caf::{CafReader, CafWriter};
use hz_io::container::pcm::PcmFormat;
use hz_io::container::wav::{RiffKind, Slot, WavFormat, WavReader, WavWriter};
use hz_io::master::MasterSet;
use hz_io::project;
use std::path::{Path, PathBuf};

/// Frames per block. Large enough that the per-call cost disappears, small
/// enough that the buffer stays friendly to a cache.
const BLOCK_FRAMES: usize = 8192;

pub fn run(input: &Path, output: &Path, presentation: usize) -> Result<()> {
    match input.extension().and_then(|e| e.to_str()) {
        Some("atmos") => master_to_adm(input, output, presentation),
        _ => adm_to_master(input, output),
    }
}

// ---------------------------------------------------------------------------
// Master set -> ADM BW64
// ---------------------------------------------------------------------------

fn master_to_adm(config_path: &Path, output: &Path, presentation: usize) -> Result<()> {
    let set = MasterSet::open(config_path)?;
    let components = set
        .components
        .get(presentation)
        .ok_or_else(|| Error::malformed(config_path, format!("no presentation {presentation}")))?;

    let audio = components
        .audio
        .path()
        .ok_or_else(|| Error::MissingComponent {
            referenced_by: config_path.to_path_buf(),
            reference: set.config.presentations[presentation].audio.clone(),
        })?;
    if components.metadata.is_stale() || components.audio.is_stale() {
        eprintln!(
            "note: the config names components that are not on disk; \
             resolved them by the config's own stem"
        );
    }

    let events = set.read_events(presentation)?;
    let mut reader = CafReader::open(audio)?;
    let source = *reader.format();

    let projected = project::to_adm(
        config_path,
        &set.config,
        presentation,
        &events,
        source.channels,
        reader.frames(),
        source.sample_rate_hz(),
    )?;
    report(&projected.notes);

    // BW64 rather than RIFF regardless of size: an ADM master is what this
    // container exists for, and a file that outgrows RIFF halfway through
    // would otherwise have to be rewritten.
    let format = WavFormat::plain(PcmFormat::wav(
        source.sample_rate,
        source.channels,
        source.bits_per_channel,
    ));
    let chunks = bw64::chunks(&projected.chna, &projected.adm);
    let layout = vec![
        Slot::Ds64,
        Slot::Fmt,
        Slot::Other(chunks[0].clone()),
        Slot::Data,
        Slot::Other(chunks[1].clone()),
    ];

    let mut writer = WavWriter::create_with_layout(output, RiffKind::Bw64, format, &layout)?;
    let channels = source.channels as usize;
    let mut block = vec![0i32; BLOCK_FRAMES * channels];
    loop {
        let frames = reader.read_frames(&mut block)?;
        if frames == 0 {
            break;
        }
        writer.write_frames(&block[..frames * channels])?;
    }
    writer.finish()?;

    println!(
        "{} -> {} ({} channels, {} frames, {} objects)",
        config_path.display(),
        output.display(),
        source.channels,
        reader.frames(),
        projected.adm.objects.len(),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// ADM BW64 -> master set
// ---------------------------------------------------------------------------

fn adm_to_master(input: &Path, config_path: &Path) -> Result<()> {
    let mut reader = WavReader::open(input)?;
    let description = bw64::read(input, &reader)?
        .ok_or_else(|| Error::malformed(input, "no ADM: this is a plain WAV"))?;

    if !description.parsed.ignored.is_empty() {
        eprintln!("note: elements this converter does not model, and did not carry:");
        for (name, count) in &description.parsed.ignored.elements {
            eprintln!("        {name} ×{count}");
        }
    }

    let source = *reader.format();
    let sample_rate = source.pcm.sample_rate_hz();
    let recovered = project::to_master(
        input,
        &description.chna,
        description.adm(),
        sample_rate,
        source.pcm.channels,
    )?;
    report(&recovered.notes);

    // A master set is three files that name each other, so the names have to
    // be settled before the config is written.
    let stem = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".atmos"))
        .ok_or_else(|| Error::malformed(config_path, "a master set's config ends in `.atmos`"))?;
    let metadata_name = format!("{stem}.atmos.metadata");
    let audio_name = format!("{stem}.atmos.audio");
    let directory = config_path.parent().unwrap_or(Path::new("."));

    let mut config = recovered.config;
    config.presentations[0].metadata = metadata_name.clone();
    config.presentations[0].audio = audio_name.clone();
    config.write(config_path)?;
    recovered.events.write(&directory.join(&metadata_name))?;

    let audio_path: PathBuf = directory.join(&audio_name);
    let mut writer = CafWriter::create(
        &audio_path,
        PcmFormat::master(source.pcm.sample_rate, source.pcm.channels),
    )?;
    let channels = source.pcm.channels as usize;
    let mut block = vec![0i32; BLOCK_FRAMES * channels];
    loop {
        let frames = reader.read_frames(&mut block)?;
        if frames == 0 {
            break;
        }
        writer.write_frames(&block[..frames * channels])?;
    }
    writer.finish()?;

    println!(
        "{} -> {} ({} channels, {} frames, {} objects, {} bed channels)",
        input.display(),
        config_path.display(),
        source.pcm.channels,
        reader.frames(),
        config.presentations[0].objects.len(),
        config.presentations[0]
            .bed_instances
            .iter()
            .map(|b| b.channels.len())
            .sum::<usize>(),
    );
    Ok(())
}

/// Print what the projection could not carry.
///
/// On stderr and never suppressed: a conversion that loses something and says
/// nothing is the failure mode this whole crate is arranged against.
fn report(notes: &[String]) {
    for note in notes {
        eprintln!("note: {note}");
    }
}
