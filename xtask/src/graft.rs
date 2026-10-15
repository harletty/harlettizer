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
    /// Carry parameters `hz-joc` estimates, from the reference's core and the
    /// objects the reference rebuilds, in place of the reference's own.
    pub refit: Option<hz_joc::estimate::Fit>,
    /// With `refit`, the estimator's ridge, relative to the normal
    /// equations' mean diagonal; its own default unless asked.
    pub ridge: Option<f32>,
    /// With `refit`, list the frames where ours lose most against the
    /// reference's parameters.
    pub probe: bool,
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
    frames_of(path, &bytes)
}

fn frames_of(path: &Path, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut extractor = Extractor::default();
    extractor.push_bytes(bytes);
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
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    decode_objects_of(path, &bytes)
}

/// `path` names the stream in errors only.
fn decode_objects_of(path: &Path, bytes: &[u8]) -> Result<Objects> {
    let mut decoder = ObjectPcmDecoder::new();
    let mut objects: Vec<Vec<f32>> = Vec::new();
    let mut core = Vec::new();
    for (index, frame) in frames_of(path, bytes)?.iter().enumerate() {
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

/// The reference's core coded by `hz-ac3`, each frame carrying `emdf[k]`;
/// with the rate and whether it coupled.
fn encode(
    out: &Path,
    reference: &Reference,
    emdf: &[Vec<u8>],
    bit_rate: u32,
) -> Result<(Vec<u8>, u32, bool)> {
    let mut encoder = Encoder::new(Config {
        format: Format::Eac3,
        sample_rate: reference.sample_rate,
        mode: ChannelMode::ThreeTwo,
        lfe: true,
        bit_rate: Some(bit_rate),
        coupling: true,
        metadata: Metadata::default(),
    })
    .map_err(|e| Error::unsupported(out, e.0))?;
    // Our frame k decodes, DELAY later, to input samples from k·1536 − DELAY:
    // feeding the core DELAY samples early lines frame k up with the
    // reference's frame k, whose parameters it then carries.
    let mut planes = vec![vec![0f32; FRAME_SAMPLES]; 6];
    let mut stream = Vec::new();
    for (k, frame_emdf) in emdf.iter().enumerate() {
        for (plane, source) in planes.iter_mut().zip(&reference.core) {
            for (n, v) in plane.iter_mut().enumerate() {
                *v = source
                    .get(k * FRAME_SAMPLES + n + DELAY)
                    .copied()
                    .unwrap_or(0.0);
            }
        }
        if frame_emdf.len() > 511 {
            return Err(Error::refused(
                out,
                format!(
                    "syncframe {k}: an Evolution frame of {} bytes, past the 511 a skip field holds",
                    frame_emdf.len()
                ),
            ));
        }
        if !frame_emdf.is_empty() {
            encoder.carry_skip_field(frame_emdf);
        }
        let refs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
        encoder
            .encode(&refs, &mut stream)
            .map_err(|e| Error::refused(out, e.0))?;
    }
    Ok((stream, encoder.bit_rate(), encoder.coupled()))
}

/// Every frame's Evolution frame with its joint object coding payload
/// replaced by `hz-joc`'s estimate: from the reference's decoded core and the
/// objects the reference rebuilds from it, advanced by the decoder's latency
/// onto the core's clock. The objects present, the downmix configuration and
/// the sequence count follow the reference's. Prints how close the estimated
/// gains come to the reference's own.
fn refit(
    path: &Path,
    reference: &Reference,
    theirs: &Objects,
    fit: hz_joc::estimate::Fit,
    ridge: Option<f32>,
) -> Result<(Vec<Vec<u8>>, Vec<Vec<bool>>)> {
    use hz_joc::estimate::{Estimator, dequantise};
    use hz_meta::emdf;

    // Each frame's Evolution frame read, and its payload read by hz-joc.
    let mut frames = Vec::with_capacity(reference.emdf.len());
    let mut jocs = Vec::with_capacity(reference.emdf.len());
    for (k, raw) in reference.emdf.iter().enumerate() {
        let (frame, _) = emdf::read_synced(raw, 0).ok_or_else(|| {
            Error::malformed(
                path,
                format!("syncframe {k}: an unreadable Evolution frame"),
            )
        })?;
        let joc = frame
            .payloads
            .iter()
            .find(|p| p.id == emdf::JOC)
            .ok_or_else(|| {
                Error::malformed(path, format!("syncframe {k}: no joint object coding"))
            })?;
        jocs.push(
            hz_joc::read(&joc.bytes)
                .map_err(|e| Error::malformed(path, format!("syncframe {k}: {e}")))?,
        );
        frames.push(frame);
    }
    let count = theirs.objects.len();
    let first = jocs
        .first()
        .ok_or_else(|| Error::malformed(path, "no frames"))?;
    let mut estimator = Estimator::new(count, first.downmix_config);
    estimator.set_sequence(first.sequence);
    estimator.fit = fit;
    if let Some(ridge) = ridge {
        estimator.ridge = ridge;
    }

    // The downmix in the order joint object coding names it: L, R, C, Ls, Rs
    // — WAV planes 0, 1, 2, 4, 5.
    let core_order = [0usize, 1, 2, 4, 5];
    // The decoder hands objects back this much after the core went in; any
    // other offset puts the subbands out of phase and the fit collapses.
    let latency = eac3::JOC_LATENCY_SAMPLES;
    let silence = vec![0f32; FRAME_SAMPLES];
    let mut ideal = vec![vec![0f32; FRAME_SAMPLES]; count];
    let mut estimates = Vec::with_capacity(jocs.len());
    for (k, joc) in jocs.iter().enumerate() {
        let start = k * FRAME_SAMPLES;
        let core: Vec<&[f32]> = core_order
            .iter()
            .map(|ch| {
                reference.core[*ch]
                    .get(start..start + FRAME_SAMPLES)
                    .unwrap_or(&silence)
            })
            .collect();
        for (plane, object) in ideal.iter_mut().zip(&theirs.objects) {
            for (n, v) in plane.iter_mut().enumerate() {
                *v = object.get(start + n + latency).copied().unwrap_or(0.0);
            }
        }
        let objects: Vec<&[f32]> = ideal.iter().map(Vec::as_slice).collect();
        let present: Vec<bool> = (0..count)
            .map(|o| joc.objects.get(o).is_some_and(Option::is_some))
            .collect();
        estimates.extend(estimator.push(&core, &objects, &present));
    }
    estimates.extend(estimator.finish());

    // How close the gains come, where both carry an object.
    // Where the gains part from the reference's, band by band: the narrow
    // low bands, a subband or two each, are where the subbands' leakage into
    // their neighbours weighs most against a fit made band by band.
    let mut per_band = [(0usize, 0usize); hz_joc::estimate::BANDS];
    for (ours, theirs) in estimates.iter().zip(&jocs) {
        let channels = theirs.channels();
        for (o, t) in ours.objects.iter().zip(&theirs.objects) {
            let (Some(o), Some(t)) = (o, t) else {
                continue;
            };
            let (Some(a), Some(b)) = (dequantise(o, channels), dequantise(t, channels)) else {
                continue;
            };
            for ch in 0..channels {
                for (band, (count, off)) in per_band.iter_mut().enumerate() {
                    *count += 1;
                    *off += usize::from(a[ch][band] != b[ch][band]);
                }
            }
        }
    }
    let (mut squared, mut gains, mut codes_equal, mut codes) = (0f64, 0usize, 0usize, 0usize);
    for (ours, theirs) in estimates.iter().zip(&jocs) {
        let channels = theirs.channels();
        for (o, t) in ours.objects.iter().zip(&theirs.objects) {
            let (Some(o), Some(t)) = (o, t) else {
                continue;
            };
            let (Some(a), Some(b)) = (dequantise(o, channels), dequantise(t, channels)) else {
                continue;
            };
            for ch in 0..channels {
                for band in 0..hz_joc::estimate::BANDS {
                    let d = f64::from(a[ch][band] - b[ch][band]);
                    squared += d * d;
                    gains += 1;
                    codes += 1;
                    codes_equal += usize::from(a[ch][band] == b[ch][band]);
                }
            }
        }
    }
    let size = |jocs: &mut dyn Iterator<Item = usize>| {
        let sizes: Vec<usize> = jocs.collect();
        (
            sizes.iter().sum::<usize>() as f64 / sizes.len().max(1) as f64,
            sizes.iter().copied().max().unwrap_or(0),
        )
    };
    let (our_mean, our_max) = size(&mut estimates.iter().map(|j| j.to_bytes().len()));
    let (their_mean, their_max) = size(&mut jocs.iter().map(|j| j.to_bytes().len()));
    println!(
        "  payload      {our_mean:.0} bytes a frame on average, {our_max} at most; the reference's {their_mean:.0} and {their_max}"
    );
    println!(
        "  parameters   {gains} gains against the reference's: RMS difference {:.3} ({:.1} steps), {:.1} % on the same step",
        (squared / gains.max(1) as f64).sqrt(),
        (squared / gains.max(1) as f64).sqrt() / f64::from(hz_joc::estimate::STEP),
        100.0 * codes_equal as f64 / codes.max(1) as f64
    );
    println!(
        "               off the reference's step by band: {} %",
        per_band
            .iter()
            .map(|(count, off)| format!("{:.1}", 100.0 * *off as f64 / (*count).max(1) as f64))
            .collect::<Vec<_>>()
            .join(" ")
    );

    let protection = |(primary, secondary): (u32, u32)| {
        let length = |bits| match bits {
            8 => emdf::ProtectionLength::Bits8,
            32 => emdf::ProtectionLength::Bits32,
            _ => emdf::ProtectionLength::Bits128,
        };
        emdf::Protection {
            primary: length(primary),
            secondary: (secondary != 0).then(|| length(secondary)),
        }
    };
    let mut out = Vec::with_capacity(frames.len());
    for (frame, estimate) in frames.iter().zip(&estimates) {
        let joc = estimate.to_bytes();
        let payloads: Vec<emdf::Payload<'_>> = frame
            .payloads
            .iter()
            .map(|p| emdf::Payload {
                id: p.id,
                bytes: if p.id == emdf::JOC { &joc } else { &p.bytes },
                sample_offset: p.sample_offset.unwrap_or(0),
                config: p.config,
            })
            .collect();
        let mut container = hz_core::bits::BitWriter::new();
        emdf::write(&mut container, &payloads, protection(frame.protection));
        let container = container.finish();
        let mut synced = emdf::SYNC_WORD.to_be_bytes().to_vec();
        synced.extend_from_slice(&(container.len() as u16).to_be_bytes());
        synced.extend_from_slice(&container);
        out.push(synced);
    }
    // Where the reference stepped its parameters at a slot rather than
    // sliding them: the frames a smooth fit cannot follow.
    let steep = jocs
        .iter()
        .map(|joc| {
            (0..count)
                .map(|o| {
                    joc.objects
                        .get(o)
                        .and_then(Option::as_ref)
                        .is_some_and(|x| x.steep)
                })
                .collect()
        })
        .collect();
    Ok((out, steep))
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

pub fn run(options: Options) -> Result<()> {
    // Written before the reference is decoded a second time for the
    // comparison, an output that is the reference would replace it and the
    // graft would be compared with itself.
    if crate::paths::aliases(&options.reference, &options.out)? {
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

    let theirs = decode_objects(&options.reference)?;
    let (emdf, baseline, steep) = match options.refit {
        None => (reference.emdf.clone(), None, None),
        Some(fit) => {
            let (emdf, steep) = refit(&options.reference, &reference, &theirs, fit, options.ridge)?;
            // The graft of the reference's own parameters, beside ours, on
            // the same core.
            let stream = encode(&options.out, &reference, &reference.emdf, bit_rate)?.0;
            (
                emdf,
                Some(decode_objects_of(&options.out, &stream)?),
                Some(steep),
            )
        }
    };
    let (out, rate, coupled) = encode(&options.out, &reference, &emdf, bit_rate)?;
    std::fs::write(&options.out, &out).map_err(|e| Error::io(&options.out, e))?;
    println!(
        "  grafted      {}: {frames} syncframes at {rate} kb/s{}{}",
        options.out.display(),
        if coupled { ", coupled" } else { "" },
        match options.refit {
            Some(fit) => format!(", carrying hz-joc's parameters ({fit:?} fit)"),
            None => String::new(),
        }
    );
    if let Some(baseline) = &baseline {
        let all_t: Vec<f32> = theirs.objects.concat();
        let all_b: Vec<f32> = baseline.objects.concat();
        println!(
            "  baseline     the reference's own parameters on the same core: objects {:.2} dB",
            snr(&all_t, &all_b)
        );
    }
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
    if let (Some(steep), Some(baseline)) = (&steep, &baseline) {
        // The error split by frame: where the reference stepped the object's
        // parameters, and where it slid them.
        let mut energy = [[0f64; 3]; 2]; // [stepped?][signal, ours, baseline]
        for (o, (t, (u, b))) in theirs
            .objects
            .iter()
            .zip(ours.objects.iter().zip(&baseline.objects))
            .enumerate()
        {
            let frames = t.len() / FRAME_SAMPLES;
            for (n, ((t, u), b)) in t.iter().zip(u).zip(b).enumerate() {
                let frame = n / FRAME_SAMPLES;
                // The stream's two ends are not the programme's: the
                // reference starts a decoder cold mid-film, and the last
                // frame's fit sees silence past the end.
                if frame < 2 || frame + 2 >= frames {
                    continue;
                }
                let stepped = steep.get(frame).is_some_and(|f| f[o]);
                let e = &mut energy[usize::from(stepped)];
                let (t, u, b) = (f64::from(*t), f64::from(*u), f64::from(*b));
                e[0] += t * t;
                e[1] += (u - t) * (u - t);
                e[2] += (b - t) * (b - t);
            }
        }
        if options.probe {
            let mut excess: Vec<(f64, usize, usize, f64, f64)> = Vec::new();
            for (o, (t, (u, b))) in theirs
                .objects
                .iter()
                .zip(ours.objects.iter().zip(&baseline.objects))
                .enumerate()
            {
                for f in 0..t.len() / FRAME_SAMPLES {
                    let (mut s, mut eu, mut eb) = (0f64, 0f64, 0f64);
                    for n in f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES {
                        let (t, u, b) = (f64::from(t[n]), f64::from(u[n]), f64::from(b[n]));
                        s += t * t;
                        eu += (u - t) * (u - t);
                        eb += (b - t) * (b - t);
                    }
                    excess.push((eu - eb, f, o, s, eu));
                }
            }
            let total: f64 = excess.iter().map(|e| e.0).sum();
            excess.sort_by(|a, b| b.0.total_cmp(&a.0));
            let top: f64 = excess.iter().take(20).map(|e| e.0).sum();
            println!(
                "  probe        the 20 worst frames hold {:.1} % of our excess error",
                100.0 * top / total.max(1e-30)
            );
            for (_, f, o, s, eu) in excess.iter().take(10) {
                println!(
                    "               frame {f} object {o}: signal {s:.2e}, our error {eu:.2e}, stepped {}",
                    steep.get(*f).is_some_and(|x| x[*o])
                );
            }
        }
        let db = |s: f64, n: f64| 10.0 * (s / n.max(1e-30)).log10();
        for (stepped, e) in energy.iter().enumerate() {
            if e[0] == 0.0 {
                continue;
            }
            println!(
                "  {}  ours {:.2} dB, the reference's parameters {:.2} dB, {:.1} % of the objects' energy (ends excluded)",
                if stepped == 1 {
                    "stepped  "
                } else {
                    "slid     "
                },
                db(e[0], e[1]),
                db(e[0], e[2]),
                100.0 * e[0] / (energy[0][0] + energy[1][0]).max(1e-30)
            );
        }
    }
    Ok(())
}
