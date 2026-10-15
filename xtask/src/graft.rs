//! Carry a reference stream's objects on a core this project encoded.
//!
//! ```text
//! cargo run -p xtask --features eac3-oracle -- graft reference.ec3 --out grafted.ec3
//! ```
//!
//! The test that comes before any joint object coding of our own: does the
//! E-AC-3 we write carry objects at all? The reference stream's core is
//! decoded, encoded again by `hz-ac3`, and every syncframe given back the
//! reference frame's Evolution frame — its object metadata and joint object
//! coding parameters, byte for byte — in block 2's skip field, where the
//! reference carries it. Then both streams go through the workspace's object
//! decoder, and each object reconstructed from our core is compared with the
//! same object reconstructed from the reference's.
//!
//! What it checks, end to end: that the frame is E-AC-3 a decoder reads, that
//! the skip field is where a decoder looks and holds what it expects, that our
//! frames line up with the reference's sample for sample, and that our core is
//! close enough to the reference's for its parameters to still rebuild the
//! objects. What it does not: the protection fields come across as they were,
//! a digest over audio that is no longer the audio, and no public decoder
//! checks them.

use eac3::{BedChannel, EmdfSource, Extractor, ObjectPcmDecoder, PcmDecoder};
use hz_ac3::{ChannelMode, Config, DELAY, Encoder, FRAME_SAMPLES, Format, Metadata};
use hz_core::{Error, Result};
use std::path::{Path, PathBuf};

pub struct Options {
    pub reference: PathBuf,
    pub out: PathBuf,
    /// kb/s; the reference's own rate unless asked.
    pub bit_rate: Option<u32>,
}

/// WAV order, the order `hz-ac3` takes its input in.
fn wav_rank(channel: BedChannel) -> usize {
    match channel {
        BedChannel::FrontLeft => 0,
        BedChannel::FrontRight => 1,
        BedChannel::Center => 2,
        BedChannel::LowFrequencyEffects => 3,
        BedChannel::SurroundLeft => 4,
        BedChannel::SurroundRight => 5,
        _ => 6,
    }
}

struct Reference {
    /// The core, planar, in WAV order.
    core: Vec<Vec<f32>>,
    /// Each syncframe's Evolution frame as it stood, behind its sync word.
    emdf: Vec<Vec<u8>>,
    frame_bytes: usize,
    sample_rate: u32,
}

fn frames(path: &Path) -> Result<Vec<Vec<u8>>> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let mut extractor = Extractor::default();
    extractor.push_bytes(&bytes);
    let mut out = Vec::new();
    while let Some(frame) = extractor
        .next_frame()
        .map_err(|e| Error::malformed(path, format!("{e:?}")))?
    {
        out.push(frame.as_bytes().to_vec());
    }
    // The extractor says "nothing more" both at the end and when it is
    // waiting for the rest of a frame; the whole file is in, so anything it
    // still holds is a frame cut short, and a reference that lost its end
    // must not pass for one that did not.
    if extractor.buffered_len() > 0 {
        return Err(Error::malformed(
            path,
            format!(
                "{} bytes after syncframe {} are not a whole syncframe",
                extractor.buffered_len(),
                out.len()
            ),
        ));
    }
    Ok(out)
}

fn read_reference(path: &Path) -> Result<Reference> {
    let mut decoder = PcmDecoder::new();
    let mut core: Vec<Vec<f32>> = vec![Vec::new(); 6];
    let mut emdf = Vec::new();
    let (mut frame_bytes, mut sample_rate) = (0, 0);
    for (index, frame) in frames(path)?.iter().enumerate() {
        let pushed = decoder
            .push_access_unit(frame)
            .map_err(|e| Error::malformed(path, format!("syncframe {index}: {e}")))?;
        let (info, pcm) = (pushed.info, pushed.pcm);
        if info.channel_mode != 7 || !info.lfe_on {
            return Err(Error::unsupported(
                path,
                "a graft takes a 5.1 independent substream, which every reference to hand is",
            ));
        }
        frame_bytes = frame.len();
        sample_rate = pcm.sample_rate;
        for (channel, samples) in pcm
            .fullband_channel_order
            .iter()
            .zip(&pcm.fullband_channels)
        {
            core[wav_rank(*channel)].extend_from_slice(samples);
        }
        core[3].extend_from_slice(pcm.lfe_channel.as_deref().unwrap_or(&[]));

        let carrier: &[u8] = match info.emdf_source {
            EmdfSource::AuxData => &info.aux_data,
            EmdfSource::FrameScanFallback => frame,
            EmdfSource::None => &[],
        };
        let raw = match info.emdf_blocks.first() {
            Some(block) => {
                let (_, end) =
                    hz_meta::emdf::read_synced(carrier, block.sync_offset).ok_or_else(|| {
                        Error::malformed(
                            path,
                            format!("syncframe {index}: an unreadable Evolution frame"),
                        )
                    })?;
                carrier[block.sync_offset..end].to_vec()
            }
            None => Vec::new(),
        };
        emdf.push(raw);
    }
    Ok(Reference {
        core,
        emdf,
        frame_bytes,
        sample_rate,
    })
}

/// Every object channel of every frame a stream reconstructs, and its core,
/// concatenated.
struct Objects {
    objects: Vec<Vec<f32>>,
    core: Vec<f32>,
}

fn decode_objects(path: &Path) -> Result<Objects> {
    let mut decoder = ObjectPcmDecoder::new();
    let mut objects: Vec<Vec<f32>> = Vec::new();
    let mut core = Vec::new();
    for (index, frame) in frames(path)?.iter().enumerate() {
        let Some(pushed) = decoder
            .push_access_unit(frame)
            .map_err(|e| Error::malformed(path, format!("syncframe {index}: {e}")))?
        else {
            return Err(Error::malformed(
                path,
                format!("syncframe {index} carries no joint object coding"),
            ));
        };
        let pcm = pushed.pcm;
        if objects.is_empty() {
            objects = vec![Vec::new(); pcm.object_count()];
        }
        for (object, samples) in objects.iter_mut().zip(&pcm.object_channels) {
            object.extend_from_slice(samples);
        }
        for samples in &pcm.core.fullband_channels {
            core.extend_from_slice(samples);
        }
    }
    Ok(Objects { objects, core })
}

fn snr(reference: &[f32], test: &[f32]) -> f64 {
    let (mut signal, mut noise) = (0f64, 0f64);
    for (r, t) in reference.iter().zip(test) {
        let (r, t) = (f64::from(*r), f64::from(*t));
        signal += r * r;
        noise += (t - r) * (t - r);
    }
    if signal == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (signal / noise.max(1e-30)).log10()
}

/// Whether `out` names the file `reference` is, by any path: written first,
/// it would replace the reference before the reference is decoded for the
/// comparison, which would then compare the graft with itself.
fn aliases(reference: &Path, out: &Path) -> Result<bool> {
    let reference_at = std::fs::canonicalize(reference).map_err(|e| Error::io(reference, e))?;
    let out_at = match std::fs::canonicalize(out) {
        Ok(path) => path,
        // Not there yet: where it would be, through its directory's links.
        Err(_) => {
            let parent = match out.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            match (std::fs::canonicalize(parent), out.file_name()) {
                (Ok(dir), Some(name)) => dir.join(name),
                _ => return Ok(false),
            }
        }
    };
    if reference_at == out_at {
        return Ok(true);
    }
    // Another name for the same file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (std::fs::metadata(reference), std::fs::metadata(out)) {
            return Ok(a.dev() == b.dev() && a.ino() == b.ino());
        }
    }
    Ok(false)
}

pub fn run(options: Options) -> Result<()> {
    if aliases(&options.reference, &options.out)? {
        return Err(Error::refused(
            &options.out,
            "the output is the reference: writing it would replace what the graft is compared with",
        ));
    }
    let reference = read_reference(&options.reference)?;
    let frames = reference.emdf.len();
    let carried = reference.emdf.iter().filter(|e| !e.is_empty()).count();
    // A syncframe of six blocks: bytes × 8 / 32 ms is the rate.
    let reference_rate = (reference.frame_bytes as u64 * 8 * u64::from(reference.sample_rate)
        / FRAME_SAMPLES as u64
        / 1000) as u32;
    let bit_rate = options.bit_rate.unwrap_or(reference_rate);
    println!(
        "{}: {frames} syncframes, {carried} with an Evolution frame, 5.1 at {reference_rate} kb/s",
        options.reference.display()
    );

    let mut encoder = Encoder::new(Config {
        format: Format::Eac3,
        sample_rate: reference.sample_rate,
        mode: ChannelMode::ThreeTwo,
        lfe: true,
        bit_rate: Some(bit_rate),
        coupling: true,
        metadata: Metadata::default(),
    })
    .map_err(|e| Error::unsupported(&options.out, e.0))?;

    // Our frame k decodes, DELAY later, to input samples from k·1536 − DELAY:
    // feeding the core DELAY samples early lines frame k up with the
    // reference's frame k, whose parameters it then carries.
    let mut planes = vec![vec![0f32; FRAME_SAMPLES]; 6];
    let mut out = Vec::new();
    for (k, emdf) in reference.emdf.iter().enumerate() {
        for (plane, source) in planes.iter_mut().zip(&reference.core) {
            for (n, v) in plane.iter_mut().enumerate() {
                *v = source
                    .get(k * FRAME_SAMPLES + n + DELAY)
                    .copied()
                    .unwrap_or(0.0);
            }
        }
        if !emdf.is_empty() {
            encoder.carry_skip_field(emdf);
        }
        let refs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
        encoder
            .encode(&refs, &mut out)
            .map_err(|e| Error::refused(&options.out, e.0))?;
    }
    std::fs::write(&options.out, &out).map_err(|e| Error::io(&options.out, e))?;
    println!(
        "  grafted      {}: {frames} syncframes at {} kb/s{}",
        options.out.display(),
        encoder.bit_rate(),
        if encoder.coupled() { ", coupled" } else { "" }
    );

    let theirs = decode_objects(&options.reference)?;
    let ours = decode_objects(&options.out)?;
    if ours.objects.len() != theirs.objects.len() {
        return Err(Error::malformed(
            &options.out,
            format!(
                "reconstructs {} objects where the reference does {}",
                ours.objects.len(),
                theirs.objects.len()
            ),
        ));
    }
    let per_object: Vec<f64> = theirs
        .objects
        .iter()
        .zip(&ours.objects)
        .map(|(t, o)| snr(t, o))
        .collect();
    let all_t: Vec<f32> = theirs.objects.concat();
    let all_o: Vec<f32> = ours.objects.concat();
    println!(
        "  core         {:.2} dB against the reference's decoded core",
        snr(&theirs.core, &ours.core)
    );
    println!(
        "  objects      {} reconstructed from our core, {:.2} dB against the reference's own, by object {}",
        per_object.len(),
        snr(&all_t, &all_o),
        per_object
            .iter()
            .map(|v| if v.is_finite() {
                format!("{v:.1}")
            } else {
                "silent".into()
            })
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok(())
}
