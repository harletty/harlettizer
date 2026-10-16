//! `harlettizer ac3`: a programme rendered to 5.1 or stereo and coded as AC-3.
//!
//! The objects do not survive as objects — AC-3 has none — so the master is
//! rendered as `iamf` renders it, its bed channels routed to their speakers
//! and its objects panned on the room's cube, onto 7.1; and 7.1 is folded to
//! the layout asked for by the fold measured off shipped TrueHD streams
//! ([`hz_render::fold::presentation_matrix`]), the one this project's own
//! TrueHD presentations carry. The cube has no place for a 5.1 surround at
//! ±110°, and the fold is what a 5.1 listener of the same master hears from
//! the lossless stream. The loudness is measured on what is coded, before the
//! codec, and reported beside the dialnorm the stream states.
//!
//! With `--joc` the objects do survive: E-AC-3 carries them beside the 5.1
//! core by joint object coding ([`crate::joc`]).

use hz_ac3::{ChannelMode, DELAY, Encoder, FRAME_SAMPLES};
use hz_analysis::loudness::{ChannelWeight, Meter};
use hz_core::{Error, Result};
use hz_io::container::SampleFormat;
use hz_programme::follow::Following;
use hz_programme::progress::Progress;
use hz_programme::source::Source;
use hz_programme::{Part, parts_of};
use hz_render::fold::{FoldMode, presentation_matrix};
use hz_render::{Keyframe, Layout, Mixdown};
use std::io::Write;
use std::path::PathBuf;

/// What `ac3` was asked to do.
pub struct Config {
    pub input: PathBuf,
    pub out: PathBuf,
    pub frames: Option<u64>,
    pub mono_prefix: Option<PathBuf>,
    /// `5.1` or `2.0`.
    pub layout: Layout,
    /// kb/s; the encoder's default for the channel count when not given.
    pub bit_rate: Option<u32>,
    pub dialnorm: i8,
    pub coupling: bool,
    /// Write one independent E-AC-3 substream rather than AC-3.
    pub eac3: bool,
    /// With `eac3` and 5.1, carry the objects by joint object coding.
    pub joc: bool,
    pub progress: bool,
}

pub fn run(config: Config) -> Result<()> {
    let path = config.input.as_path();
    let mut source = Source::open(path, config.mono_prefix.as_deref())?;
    // Creating the output truncates it, and every file of the input is still
    // to be read: refuse one that is the input, before any work is done.
    hz_io::paths::refuse_inputs(source.files(), &config.out)?;
    let format = *source.pcm_format();
    if format.sample_format == SampleFormat::Float {
        return Err(Error::unsupported(
            path,
            "floating-point audio; the master formats this reads are integer PCM",
        ));
    }
    let input_scale = 1.0 / f64::from(1u32 << (format.bits_per_channel - 1));
    let sample_rate = source.sample_rate();
    let stride = source.channels();

    let layout = &config.layout;
    let width = layout.channels();
    let rendered = Layout::surround_7_1();
    let rendered_width = rendered.channels();
    let fold = presentation_matrix(layout, FoldMode::default())
        .map_err(|e| Error::unsupported(path, e.to_string()))?
        .expect("5.1 and 2.0 are not 7.1");
    let (mode, lfe) = match layout.name {
        "5.1" => (ChannelMode::ThreeTwo, true),
        "2.0" => (ChannelMode::Stereo, false),
        other => {
            return Err(Error::unsupported(
                path,
                format!("AC-3 to {other}; 5.1 or 2.0"),
            ));
        }
    };
    let lfe_speaker = rendered.index_of("LFE1");
    if config.joc && (!config.eac3 || layout.name != "5.1") {
        return Err(Error::unsupported(
            path,
            "joint object coding without E-AC-3 on 5.1; it rebuilds objects from a 5.1 core",
        ));
    }
    let mut elements = Vec::new();
    let mut master_lfe = false;

    // Every track to a source of the mixdown, as `iamf` routes them.
    let mut following = Following::new(Mixdown::new(&rendered)?);
    let (mut routed, mut placed, mut objects, mut dropped) = (0usize, 0, 0, 0);
    for part in parts_of(path, &source)? {
        match part {
            Part::Lfe { channel } => {
                following
                    .mixdown()
                    .speaker(channel, lfe_speaker.expect("7.1 has an LFE"));
                routed += 1;
                master_lfe = true;
                // Stereo has no LFE, and the fold leaves it out, as a decoder
                // folding 5.1 to stereo does.
                dropped += usize::from(!layout.has("LFE1"));
            }
            Part::Bed {
                channel,
                name,
                position,
            } => {
                let label = hz_core::speakers::by_master_name(name).map_or(name, |s| s.label);
                elements.push(crate::joc::Element::Bed { channel, position });
                let mixdown = following.mixdown();
                if let Some(speaker) = rendered.index_of(label) {
                    mixdown.speaker(channel, speaker);
                    routed += 1;
                } else {
                    let index = mixdown.object(channel);
                    mixdown.update(
                        index,
                        &Keyframe {
                            position,
                            ..Keyframe::default()
                        },
                    );
                    placed += 1;
                }
            }
            Part::Object { channel, keyframes } => {
                following.object(channel, &keyframes);
                elements.push(crate::joc::Element::Object {
                    channel,
                    path: hz_programme::path::Path::new(&keyframes),
                    hint: 0,
                });
                objects += 1;
            }
        }
    }
    if routed + placed + objects == 0 {
        return Err(Error::unsupported(path, "nothing in it to render"));
    }

    let codec = hz_ac3::Config {
        format: if config.eac3 {
            hz_ac3::Format::Eac3
        } else {
            hz_ac3::Format::Ac3
        },
        sample_rate,
        mode,
        lfe,
        bit_rate: config.bit_rate,
        coupling: config.coupling,
        metadata: hz_ac3::Metadata {
            dialnorm: config.dialnorm,
            ..hz_ac3::Metadata::default()
        },
    };
    let element_count = elements.len();
    let mut sink = if config.joc {
        Sink::Joc(Box::new(crate::joc::Writer::new(
            codec,
            elements,
            master_lfe,
            &config.out,
        )?))
    } else {
        Sink::Plain(Box::new(
            Encoder::new(codec).map_err(|e| Error::unsupported(&config.out, e.0))?,
        ))
    };
    let out_file = std::fs::File::create(&config.out).map_err(|e| Error::io(&config.out, e))?;
    let mut out = std::io::BufWriter::new(out_file);

    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    let mut progress = Progress::new(config.progress, total);

    let weights: Vec<ChannelWeight> = layout
        .speakers
        .iter()
        .map(|label| ChannelWeight::for_speaker_label(label))
        .collect();
    let mut meter = Meter::new(sample_rate, &weights);

    let mut raw = vec![0i32; FRAME_SAMPLES * stride];
    let mut input = vec![0f32; FRAME_SAMPLES * stride];
    let mut mixed = vec![0f64; FRAME_SAMPLES * rendered_width];
    let mut interleaved = vec![0f32; FRAME_SAMPLES * width];
    let mut planar = vec![vec![0f32; FRAME_SAMPLES]; width];
    let mut frame = Vec::new();
    let (mut clipped, mut peak) = (0u64, 0f64);

    let mut at = 0u64;
    let mut syncframes = 0u64;
    loop {
        let want = (total - at).min(FRAME_SAMPLES as u64) as usize;
        let got = if want == 0 {
            0
        } else {
            source.fill(&mut raw, want)?
        };
        for (x, &r) in input[..got * stride].iter_mut().zip(&raw[..got * stride]) {
            *x = (f64::from(r) * input_scale) as f32;
        }
        mixed.fill(0.0);
        if got > 0 {
            following.render(
                &input[..got * stride],
                stride,
                got,
                at,
                &mut mixed[..got * rendered_width],
            );
        }
        for (frame, out) in mixed
            .chunks_exact(rendered_width)
            .zip(interleaved.chunks_exact_mut(width))
        {
            for (target, f) in out.iter_mut().enumerate() {
                let m: f64 = frame
                    .iter()
                    .zip(&fold)
                    .map(|(x, row)| x * row[target])
                    .sum();
                peak = peak.max(m.abs());
                if m.abs() > 1.0 {
                    clipped += 1;
                }
                *f = m.clamp(-1.0, 1.0) as f32;
            }
        }
        meter.push(&interleaved[..got * width]);
        for (ch, plane) in planar.iter_mut().enumerate() {
            for (n, v) in plane.iter_mut().enumerate() {
                *v = interleaved[n * width + ch];
            }
        }

        frame.clear();
        let mut refs: [&[f32]; 6] = [&[]; 6];
        for (r, plane) in refs.iter_mut().zip(&planar) {
            *r = plane;
        }
        match &mut sink {
            Sink::Plain(encoder) => encoder
                .encode(&refs[..width], &mut frame)
                .map_err(|e| Error::refused(&config.out, e.0))?,
            Sink::Joc(writer) => writer.frame(
                &refs[..width],
                &input[..FRAME_SAMPLES * stride],
                stride,
                got,
                at,
                &mut frame,
            )?,
        }
        out.write_all(&frame)
            .map_err(|e| Error::io(&config.out, e))?;
        syncframes += 1;

        at += got as u64;
        if let Some(progress) = progress.as_mut() {
            progress.at(at);
        }
        // Past the input's end, the frames keep coming until the decoder's
        // output has caught up with it — see [`lag`].
        let ended = got < FRAME_SAMPLES;
        if ended && caught_up(syncframes, at, lag(config.joc)) {
            break;
        }
    }
    if let Sink::Joc(writer) = &mut sink {
        frame.clear();
        writer.finish(&mut frame)?;
        out.write_all(&frame)
            .map_err(|e| Error::io(&config.out, e))?;
    }
    out.flush().map_err(|e| Error::io(&config.out, e))?;
    if at == 0 {
        return Err(Error::unsupported(path, "no audio to encode"));
    }
    meter.flush();

    let mut parts = Vec::new();
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if objects > 0 {
        parts.push(plural(objects, "object", "objects"));
    }
    if routed > 0 {
        parts.push(format!(
            "{} routed to their speakers",
            plural(routed, "bed channel", "bed channels")
        ));
    }
    if placed > 0 {
        parts.push(format!(
            "{} placed where 7.1 has no speaker",
            plural(placed, "bed channel", "bed channels")
        ));
    }
    if dropped > 0 {
        parts.push("the LFE left out".to_string());
    }
    println!(
        "  programme    {}, rendered to 7.1 on the room's cube and folded to {}",
        parts.join(", "),
        layout.name
    );
    let (encoder, syncframes) = match &sink {
        Sink::Plain(encoder) => (encoder.as_ref(), syncframes),
        Sink::Joc(writer) => (writer.encoder(), writer.written()),
    };
    println!(
        "  encoded      {syncframes} syncframes, {at} samples, {} at {} kb/s{}",
        if config.eac3 { "E-AC-3" } else { "AC-3" },
        encoder.bit_rate(),
        if encoder.coupled() {
            ", high band coupled"
        } else {
            ""
        }
    );
    if let Sink::Joc(writer) = &sink {
        let carried = writer.elements();
        println!(
            "  objects      {carried} carried by joint object coding{}, fitted to the core a \
             decoder plays; every Evolution frame {} bytes{}",
            if carried < element_count {
                format!(", the master's {element_count} elements folded into them")
            } else {
                String::new()
            },
            writer.evolution_bytes(),
            match writer.squeezed() {
                0 => String::new(),
                n => format!(", {n} payloads quantised coarser to fit"),
            }
        );
        if let Some(folder) = writer.folder() {
            let cost = &folder.mixer.cost;
            let smoothing = folder.placer.smoothing();
            println!(
                "  fold         {:.4} mean over the presentations, {:.3} at its worst, as a \
                 fraction of the object's own gains; a block a syncframe, steadied by a hold \
                 over {} behind and {} ahead",
                cost.mean(),
                cost.worst,
                smoothing.behind,
                smoothing.ahead
            );
            println!(
                "  floor        {:.1} dBFS at the playback level a dialnorm of {} implies, \
                 {:.2} objects a block under it",
                folder.mixer.floor.threshold_dbfs(),
                config.dialnorm,
                cost.inaudible as f64 / cost.blocks.max(1) as f64
            );
        }
    }
    println!(
        "  loudness     {} on {}, the stream stating a dialnorm of {}",
        crate::iamf::lkfs(meter.integrated()),
        layout.name,
        config.dialnorm
    );
    println!(
        "  headroom     {clipped} samples clipped, the render peaking at {peak:.2} of full scale"
    );
    if clipped > 0 {
        eprintln!(
            "warning: the render clipped {clipped} samples; the master's objects sum past full \
             scale on {}",
            layout.name
        );
    }
    let bytes = std::fs::metadata(&config.out)
        .map_err(|e| Error::io(&config.out, e))?
        .len();
    println!("  wrote        {}, {bytes} bytes", config.out.display());
    Ok(())
}

/// Samples a decoder's last output is behind the input that went in: the
/// core's [`DELAY`], and with joint object coding the objects' own on top,
/// the QMF banks a decoder rebuilds them through. A final frame that is
/// nearly full has not that much silence in it to carry them out.
fn lag(joc: bool) -> u64 {
    (DELAY + if joc { hz_joc::qmf::DELAY } else { 0 }) as u64
}

/// Whether `syncframes` frames carry a decoder's output past the last of
/// `at` samples of input, `lag` behind.
fn caught_up(syncframes: u64, at: u64, lag: u64) -> bool {
    syncframes * FRAME_SAMPLES as u64 >= at + lag
}

/// Where each frame of core goes: an encoder, or the joint object coding
/// writer that drives two.
enum Sink {
    Plain(Box<Encoder>),
    Joc(Box<crate::joc::Writer>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream runs on until a decoder has played the input's last
    /// sample: a core's, the encoder's delay behind; an object's, the joint
    /// object coding latency behind that — 1000 samples take one syncframe
    /// for the core, and two once objects have to come out of it as well.
    #[test]
    fn a_stream_runs_on_until_the_objects_have_caught_up() {
        assert!(caught_up(1, 1000, lag(false)));
        assert!(!caught_up(1, 1000, lag(true)));
        assert!(caught_up(2, 1000, lag(true)));
        for at in [1u64, 1000, 1536, 1537, 3000, 48_000] {
            let mut syncframes = at.div_ceil(FRAME_SAMPLES as u64);
            while !caught_up(syncframes, at, lag(true)) {
                syncframes += 1;
            }
            assert!(syncframes * FRAME_SAMPLES as u64 >= at + 256 + 577, "{at}");
        }
    }
}
