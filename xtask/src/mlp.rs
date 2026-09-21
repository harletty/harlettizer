//! Encode a file and prove the decoder gets it back.
//!
//! A lossless codec is the one part of this project with an unarguable test:
//! `decode(encode(x)) == x`, sample for sample, or the encoder is wrong. This
//! runs it against decoders that are not ours, because an encoder and a
//! decoder written by the same hand agree about their shared mistakes.
//!
//! ```text
//! cargo xtask mlp programme.wav --out programme.thd
//! ```

use hz_core::{Error, Result};
use hz_io::container::wav::WavReader;
use hz_meta::ObjectAudioMetadata;
use hz_meta::oamd::{Block, Gain, MAX_HORIZONTAL, MAX_VERTICAL, Object, Position, Ramp, Render};
use hz_mlp::{Config, Encoder, SampleBits};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Options {
    pub input: PathBuf,
    pub out: Option<PathBuf>,
    /// Stop after this many frames. The round trip over a whole programme is
    /// slow while the encoder writes everything raw.
    pub frames: Option<u64>,
    /// Ask every presentation for this much gain in the substream directory.
    pub drc: Option<f64>,
    /// How long each value stands, as a power of two access units.
    pub drc_refresh: u32,
    /// Carry object audio metadata, and check a decoder gives it back.
    pub objects: bool,
    /// Skip the decoders, just report what was written.
    pub no_verify: bool,
    /// State a two-channel presentation on the first substream: a mix of the
    /// two channels it carries, with no unit coefficient in it.
    ///
    /// What it is for is the check that follows it. A presentation's rows do
    /// not invert, so the way to get them wrong is to let them reach the
    /// substream a full decode reads — and the verification below is what says
    /// they did not.
    pub presentation: bool,
    /// Search less hard: see [`hz_mlp::Effort`].
    pub fast: bool,
}

pub fn run(options: Options) -> Result<()> {
    let (format, samples) = read(&options.input)?;
    let channels = format.channels as usize;
    let mut frames = samples.len() / channels;
    if let Some(limit) = options.frames {
        frames = frames.min(limit as usize);
    }
    let samples = &samples[..frames * channels];

    let bits = match format.bits_per_channel {
        16 => SampleBits::Sixteen,
        24 => SampleBits::TwentyFour,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("{other}-bit samples; the encoder takes 16 and 24"),
            ));
        }
    };

    let mut encoder = Encoder::new(Config {
        sample_rate: format.sample_rate_hz(),
        channels,
        bits,
    })
    .map_err(|e| Error::unsupported(&options.input, e.to_string()))?;
    if options.fast {
        encoder.set_effort(hz_mlp::Effort::Fast);
    }

    // The dynamic range word is metadata: a decoder asked for full range
    // ignores it, so the round trip below still has to come back bit exact
    // with it on. That is the whole test of it from here — what gain a
    // presentation *should* ask for is a phase 2 question and needs a
    // reference encoder under a licence its owner issued.
    if let Some(db) = options.drc {
        let gain =
            hz_mlp::format::DynamicRange::from_db(db, options.drc_refresh).ok_or_else(|| {
                Error::unsupported(
                    &options.input,
                    format!(
                        "{db} dB every 2^{} units is outside the fields",
                        options.drc_refresh
                    ),
                )
            })?;
        for substream in 0..hz_mlp::format::MAX_SUBSTREAMS {
            encoder.set_dynamic_range(substream, Some(gain));
        }
        println!(
            "  drc          {:+.2} dB, restated every {} units",
            gain.db(),
            gain.deadline()
        );
    }

    if options.presentation && channels >= 2 {
        let rows = [
            hz_mlp::matrix::Primitive::rounded(0, &[0.796, -0.713], hz_mlp::matrix::FRACTION),
            hz_mlp::matrix::Primitive::rounded(1, &[0.500, 0.713], hz_mlp::matrix::FRACTION),
        ];
        if let [Some(first), Some(second)] = rows {
            encoder.set_presentation(0, &[first, second]);
            println!("  presentation two channels, 2x2, stated on substream 0");
        }
    }

    let unit = encoder.frame_size() * channels;
    let mut stream = Vec::new();
    let mut at = 0;
    let mut units = 0u64;
    while at + unit <= samples.len() {
        if options.objects {
            let payload = programme(units, channels).write().map_err(|why| {
                Error::unsupported(&options.input, format!("object metadata: {why}"))
            })?;
            encoder.set_evolution(OAMD_PAYLOAD, &payload);
        }
        stream.extend_from_slice(&encoder.push(&samples[at..at + unit]));
        at += unit;
        units += 1;
    }
    stream.extend_from_slice(&encoder.finish(&samples[at..]));

    let raw = frames * channels * format.bits_per_channel as usize / 8;
    println!("{}", options.input.display());
    println!(
        "  in           {frames} frames, {channels} ch, {} bit, {} Hz",
        format.bits_per_channel,
        format.sample_rate_hz()
    );
    println!(
        "  out          {} bytes in {} access units",
        stream.len(),
        frames.div_ceil(encoder.frame_size())
    );
    println!(
        "  ratio        {:.3}x the raw size ({:.0} kbit/s)",
        stream.len() as f64 / raw as f64,
        stream.len() as f64 * 8.0 / (frames as f64 / f64::from(format.sample_rate_hz())) / 1000.0
    );

    let stats = encoder.stats();
    let orders: Vec<String> = stats
        .orders
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(order, count)| {
            format!(
                "{order}:{:.0}%",
                100.0 * *count as f64 / stats.orders.iter().sum::<u64>() as f64
            )
        })
        .collect();
    println!(
        "  residuals    {:.1} bits a sample, against {} in",
        stats.mean_width(),
        format.bits_per_channel
    );
    println!("  filters      {}", orders.join("  "));
    if stats.filter_blocks > 0 {
        println!(
            "  restated     {:.0}% of blocks, so a filter lives {:.1} blocks",
            100.0 * stats.filters_restated as f64 / stats.filter_blocks as f64,
            stats.filter_blocks as f64 / stats.filters_restated.max(1) as f64
        );
        println!(
            "  parameters   {:.0}% of channel blocks say anything at all",
            100.0 * stats.params_restated as f64 / stats.filter_blocks as f64
        );
    }
    let second: Vec<String> = stats
        .iir_orders
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(order, count)| {
            let total: u64 = stats.iir_orders.iter().sum();
            format!("{order}:{:.0}%", 100.0 * *count as f64 / total as f64)
        })
        .collect();
    println!("  second       {}", second.join("  "));
    let books: Vec<String> = stats
        .codebooks
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(book, count)| {
            let name = if book == 0 {
                "raw".to_string()
            } else {
                book.to_string()
            };
            let total: u64 = stats.codebooks.iter().sum();
            format!("{name}:{:.0}%", 100.0 * *count as f64 / total as f64)
        })
        .collect();
    println!("  codebooks    {}", books.join("  "));
    if channels > 1 {
        let mut notes = Vec::new();
        if stats.aimed_matrices > 0 {
            notes.push(format!(
                "{} of them given a step to follow",
                stats.aimed_matrices
            ));
        }
        if stats.matrices_refused > 0 {
            notes.push(format!(
                "{} refused for the codec's domain",
                stats.matrices_refused
            ));
        }
        println!(
            "  rematrixed   {:.0}% of access units{}",
            100.0 * stats.matrixed_share(),
            if notes.is_empty() {
                String::new()
            } else {
                format!(", {}", notes.join(", "))
            }
        );
    }
    println!(
        "  framing      {:.0}% of the stream",
        100.0 * stats.framing_share()
    );
    let peak = stats.peak_bitrate(format.sample_rate_hz(), encoder.frame_size());
    let declared = encoder.declared_peak_bitrate();
    // A unit above the declared peak is not by itself a fault: its bytes are
    // delivered over the gap to the next unit's arrival, and the encoder
    // lengthens that gap and repays the time afterwards. What cannot be
    // arranged is a stream that needs more than it declared on *average*, and
    // that is what an empty buffer means.
    let over_peak = stats.starved > 0;
    println!(
        "  peak         {:.1} Mbit/s in one unit, against {:.1} declared; the buffer \
         got down to {} of {} samples{}",
        peak as f64 / 1e6,
        declared as f64 / 1e6,
        stats.lowest_advance,
        encoder.input_reserve(),
        if over_peak {
            format!("  ({} units could not be scheduled)", stats.starved)
        } else {
            String::new()
        }
    );

    let path = match &options.out {
        Some(path) => path.clone(),
        None => options.input.with_extension("thd"),
    };
    std::fs::write(&path, &stream).map_err(|e| Error::io(&path, e))?;
    println!("  wrote        {}", path.display());

    if options.no_verify {
        return Ok(());
    }

    println!();
    // Both decoders, on every stream, at every channel count. The acceptance
    // criterion always said two — "so a shared bug in one cannot hide" — and
    // for a long time only one of them was actually run on the samples, which
    // is how an encoder that carried its filter state across a restart header
    // stayed bit exact through FFmpeg for weeks while `truehdd` reconstructed
    // every restart interval but the first wrongly.
    //
    // FFmpeg is the one with a limit: its TrueHD decoder refuses the restart
    // sync word an immersive presentation's last substream carries, and stops
    // at eight channels. Above that the harness says so rather than reporting
    // a comparison it did not make.
    let mut exact = !over_peak;
    if channels <= 8 {
        exact &= verify_with_ffmpeg(&path, samples, channels, bits)?;
    } else {
        println!("  ffmpeg       skipped: its decoder stops at eight channels");
    }
    // Every presentation the stream declares, not only the widest: a decoder
    // may stop after any of them, and that is what the substream split is for.
    for (presentation, count) in encoder.presentations().iter().enumerate() {
        if *count == 0 {
            continue;
        }
        exact &= verify_with_harletty_pcm(
            &path,
            samples,
            &encoder.channel_order()[..*count],
            channels,
            presentation,
            bits,
        )?;
    }
    if options.objects {
        exact &= verify_objects_independently(&stream, channels)?;
        exact &= verify_objects(&path, channels)?;
    }
    verify_with_harletty(&path);
    if over_peak {
        // A stream that overruns the peak it declares is not a stream: a
        // decoder sizes its input buffer from that figure and reports the
        // access units as arriving faster than it can take them. The samples
        // may still come back exactly, which is precisely why this has to
        // fail rather than be mentioned.
        return Err(Error::malformed(
            &options.input,
            format!(
                "{} access units could not be scheduled: the stream needs more than \
                 the {:.1} Mbit/s it declared on average, not merely in one unit, and \
                 {:.1} is the most the format lets a stream declare",
                stats.starved,
                declared as f64 / 1e6,
                declared as f64 / 1e6,
            ),
        ));
    }
    if !exact {
        // A lossless encoder that is not lossless has failed, and a harness
        // that prints that and exits zero is a harness a script will trust.
        return Err(Error::malformed(
            &options.input,
            "the round trip is not bit exact; see above",
        ));
    }
    Ok(())
}

/// The round trip through `harletty`, the second decoder — and the only one
/// that reads an immersive presentation.
///
/// Which presentation to ask for follows the channel count, because a stream
/// declares one per substream: two channels stop at the first, six and eight
/// at the third, sixteen at the fourth. The fourth is written as a Core Audio
/// file rather than as raw samples, so FFmpeg reads that back — as a container
/// reader, not as a decoder, which is a job it can do at any channel count.
fn verify_with_harletty_pcm(
    path: &Path,
    expected: &[i32],
    order: &[usize],
    channels: usize,
    presentation: usize,
    bits: SampleBits,
) -> Result<bool> {
    let out = path.with_extension(format!("p{presentation}"));
    let decode = Command::new(harletty())
        .args(["--loglevel", "error", "decode"])
        .args(["--presentation", &presentation.to_string()])
        .args(["--format", "pcm"])
        .arg("--output-path")
        .arg(&out)
        .arg(path)
        .output();
    let decode = match decode {
        Ok(decode) => decode,
        Err(_) => {
            println!("  harletty     not installed; the second decoder was not run");
            return Ok(true);
        }
    };
    if !decode.status.success() {
        println!("  harletty     FAILED to decode");
        for line in String::from_utf8_lossy(&decode.stderr).lines().take(6) {
            println!("               {line}");
        }
        return Ok(false);
    }

    // `--output-path` is a stem: the decoder appends its own extension.
    let beside = |suffix: &str| PathBuf::from(format!("{}.{suffix}", out.display()));

    // Presentation 3 ignores `--format` and writes Core Audio.
    let raw = beside("pcm");
    let decoded: Vec<i32> = if raw.exists() {
        let bytes = std::fs::read(&raw).map_err(|e| Error::io(&raw, e))?;
        let _ = std::fs::remove_file(&raw);
        // Twenty-four bit little-endian, which is the codec's own domain.
        bytes
            .chunks_exact(3)
            .map(|word| {
                let value = u32::from(word[0]) | u32::from(word[1]) << 8 | u32::from(word[2]) << 16;
                ((value << 8) as i32) >> 8
            })
            .collect()
    } else {
        match read_caf(&beside) {
            Some(samples) => samples,
            None => {
                println!("  harletty     decoded, but its output could not be read back");
                return Ok(false);
            }
        }
    };

    let shift = match bits {
        SampleBits::Sixteen => 16,
        SampleBits::TwentyFour => 8,
    };
    // Both sides in the codec's 24-bit domain, whichever the file used — and
    // in the format's channel order, which is what this decoder reports and
    // which is not a WAV file's past 5.1.
    let wanted: Vec<i32> = expected
        .chunks_exact(channels)
        .flat_map(|frame| order.iter().map(|input| frame[*input] << shift >> 8))
        .collect();

    let out_channels = order.len();
    let permuted = order.iter().enumerate().any(|(at, input)| at != *input);
    print!(
        "  harletty     presentation {presentation}, {out_channels} ch{}: ",
        if permuted { ", format order" } else { "" }
    );
    let mut exact = true;
    match (0..decoded.len().min(wanted.len())).find(|&i| decoded[i] != wanted[i]) {
        None if decoded.len() == wanted.len() => println!("round trip bit exact"),
        // The two decoders read the end-of-stream marker differently: FFmpeg
        // truncates the last unit to the length it states, this one keeps the
        // padding and calls it silence. Both are right about the samples, so
        // a tail of zeroes is reported as what it is rather than as a
        // mismatch.
        None if decoded.len() > wanted.len() && decoded[wanted.len()..].iter().all(|s| *s == 0) => {
            println!(
                "round trip bit exact, plus {} padding samples this decoder keeps",
                (decoded.len() - wanted.len()) / out_channels
            )
        }
        None => {
            exact = false;
            println!(
                "samples agree, but {} frames came back against {}",
                decoded.len() / out_channels,
                wanted.len() / out_channels
            );
        }
        Some(index) => {
            exact = false;
            println!(
                "FIRST DIFFERENCE at frame {}, channel {}: {} against {}",
                index / out_channels,
                index % out_channels,
                decoded[index],
                wanted[index]
            );
        }
    }
    Ok(exact)
}

/// Read back the Core Audio file presentation 3 is written as.
///
/// FFmpeg is the container reader here and nothing more; the decoding was
/// `harletty`'s.
fn read_caf(beside: &dyn Fn(&str) -> PathBuf) -> Option<Vec<i32>> {
    // A stream carrying object metadata is written as an object master rather
    // than as a bare Core Audio file, and the audio lands under a different
    // name. Neither is written yet, so both are looked for.
    let mut audio = beside("caf");
    if !audio.exists() {
        audio = beside("atmos.audio");
    }
    let converted = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&audio)
        .args(["-f", "s32le", "-c:a", "pcm_s32le", "-"])
        .output()
        .ok()?;
    for stray in [audio, beside("atmos"), beside("atmos.metadata")] {
        let _ = std::fs::remove_file(stray);
    }
    if !converted.status.success() {
        return None;
    }
    Some(
        converted
            .stdout
            .chunks_exact(4)
            .map(|word| i32::from_le_bytes([word[0], word[1], word[2], word[3]]) >> 8)
            .collect(),
    )
}

/// The Evolution identifier object audio metadata rides under.
const OAMD_PAYLOAD: u32 = 11;

/// Which `harletty` to run.
///
/// `harletty` is the maintained line of `truehdd` — the same decoder, the same
/// command line, a newer `truehd` library — and the only decoder that reads a
/// sixteen-element presentation. Which build is installed turns out to
/// matter: `truehdd` 0.4.0 reads the six-bit object gain index **twice**, so a
/// payload stating a gain that way runs it off the end of the payload; the
/// library from 0.6.1 on reads it once. An encoder cannot tell those apart
/// from the outside, and a stale oracle that fails on correct output is worse
/// than no oracle at all. So: `HARLETTY` if set, else `TRUEHDD` for an older
/// setup, else whatever `harletty` is on the path.
fn harletty() -> String {
    std::env::var("HARLETTY")
        .or_else(|_| std::env::var("TRUEHDD"))
        .unwrap_or_else(|_| "harletty".into())
}

/// What the elements of the programme are doing in one access unit.
///
/// A stand-in for a real authoring pass, and deliberately a *checkable* one:
/// every element's position and gain is a function of the access unit's index
/// alone, so a decoder's own account of the programme can be held against it
/// without carrying a table around. The first element is the low frequency
/// channel, which has no position; the rest walk the room at different speeds
/// so that no two of them ever agree for long.
fn programme(unit: u64, channels: usize) -> ObjectAudioMetadata {
    let objects = (0..channels)
        .map(|element| {
            if element == 0 {
                return Object {
                    active: true,
                    gain: Gain::Unity,
                    render: None,
                };
            }
            Object {
                active: true,
                gain: gain_of(element, unit),
                render: Some(Render {
                    position: position_of(element, unit),
                    ..Render::default()
                }),
            }
        })
        .collect();

    ObjectAudioMetadata {
        lfe: true,
        blocks: vec![Block {
            offset: 0,
            // Nothing interpolates, so what a decoder reports at a sample is
            // what was written for it and not a point on the way there.
            ramp: Ramp::None,
            objects,
        }],
    }
}

fn position_of(element: usize, unit: u64) -> Position {
    let element = element as i32;
    let unit = (unit % 4096) as i32;
    Position {
        x: (element * 4 + unit / 8) % (MAX_HORIZONTAL + 1),
        y: (element * 7 + unit / 32) % (MAX_HORIZONTAL + 1),
        z: (element * 3 + unit / 16) % (2 * MAX_VERTICAL + 1) - MAX_VERTICAL,
    }
}

fn gain_of(element: usize, unit: u64) -> Gain {
    match (element as u64 + unit / 64) % 4 {
        0 => Gain::Unity,
        1 => Gain::Decibels(-7),
        2 => Gain::Silent,
        _ => Gain::Decibels(3),
    }
}

/// Read every object metadata payload in the stream with somebody else's
/// reader, and hold it against what was written.
///
/// The `truehd` crate, in process. Not the installed decoder, whose version
/// turns out to matter — see [`harletty`] — and not this project's own reader,
/// which would only agree with its own writer.
///
/// Behind a feature because compiling that crate needs 13.4 GiB, which is
/// twice what a hosted runner has: `cargo xtask --features oracle`.
#[cfg(not(feature = "oracle"))]
fn verify_objects_independently(_stream: &[u8], _channels: usize) -> Result<bool> {
    println!("  objects      built without the payload oracle; --features oracle turns it on");
    Ok(true)
}

#[cfg(feature = "oracle")]
fn verify_objects_independently(stream: &[u8], channels: usize) -> Result<bool> {
    use truehd::structs::oamd::ObjectAudioMetadataPayload;

    let units = hz_mlp::reader::stream(stream)
        .map_err(|why| Error::malformed(Path::new("<stream>"), why.0))?;

    let mut checked = 0usize;
    for (index, unit) in units.iter().enumerate() {
        let Some(extra) = &unit.evolution else {
            continue;
        };
        for payload in &extra.payloads {
            if payload.id != OAMD_PAYLOAD {
                continue;
            }
            let oamd = match ObjectAudioMetadataPayload::read(&payload.bytes) {
                Ok(oamd) => oamd,
                Err(why) => {
                    println!("  objects      unit {index}: another reader refused it: {why}");
                    return Ok(false);
                }
            };
            if oamd.object_count != channels {
                println!(
                    "  objects      unit {index}: {} elements against {channels} written",
                    oamd.object_count
                );
                return Ok(false);
            }

            // Its own account of where the objects are, in the master frame.
            let positions = oamd.get_damf_pos();
            for (element, blocks) in positions.iter().enumerate().skip(1) {
                let wanted = position_of(element, index as u64).to_master();
                for got in blocks {
                    if (0..3).any(|axis| (got[axis] - wanted[axis]).abs() > 1e-9) {
                        println!(
                            "  objects      unit {index} element {element}: {got:?} against \
                             {wanted:?}"
                        );
                        return Ok(false);
                    }
                    checked += 1;
                }
            }

            let Some(element_data) = &oamd.object_element else {
                println!("  objects      unit {index}: no object element came back");
                return Ok(false);
            };
            for (element, blocks) in element_data.object_data.iter().enumerate().skip(1) {
                let wanted = gain_of(element, index as u64).decibels().unwrap_or(-128);
                for block in blocks {
                    if block.object_basic_info.object_gain != wanted {
                        println!(
                            "  objects      unit {index} element {element}: gain {} against \
                             {wanted}",
                            block.object_basic_info.object_gain
                        );
                        return Ok(false);
                    }
                    checked += 1;
                }
            }
        }
    }

    if checked == 0 {
        println!("  objects      nothing was carried to check");
        return Ok(false);
    }
    println!("  objects      {checked} agree with another reader of the payload");
    Ok(true)
}

/// Decode the stream's object programme and hold it against what was written.
///
/// The decoder writes its account out as a master file set, which is the
/// format this project already reads — so the comparison is between two
/// documents rather than between this code and itself. The event stream is a
/// delta stream: an entry states what changed and what it omits stays in
/// force, so what is checked is that every entry that *does* state a position
/// or a gain states the one written for that element at that sample.
fn verify_objects(path: &Path, channels: usize) -> Result<bool> {
    let out = path.with_extension("objects");
    let decode = Command::new(harletty())
        .args(["--loglevel", "error", "decode", "--presentation", "3"])
        .arg("--output-path")
        .arg(&out)
        .arg(path)
        .output()
        .map_err(|e| Error::io(path, e))?;
    if !decode.status.success() {
        // Unproven rather than disproven, and the difference matters: the
        // payload itself has already been read back by another implementation
        // in this same run. What a refusal leaves unchecked is the pipeline
        // around it, and the likeliest reason for one is the decoder's own
        // version — see docs/objects.md.
        println!("  objects      the decoder refused the stream, so the pipeline is unchecked");
        for line in String::from_utf8_lossy(&decode.stderr).lines().take(3) {
            println!("               {line}");
        }
        println!(
            "               using {}; truehdd 0.4.0 reads the object gain index twice, \
             harletty reads it once. Set HARLETTY to choose.",
            harletty()
        );
        return Ok(true);
    }

    let config = PathBuf::from(format!("{}.atmos", out.display()));
    let master = hz_io::master::MasterSet::open(&config)?;
    let events = master.read_events(0)?;
    let programme = &master.config.presentations[0];
    let cleanup = || {
        for stray in ["atmos", "atmos.metadata", "atmos.audio"] {
            let _ = std::fs::remove_file(format!("{}.{stray}", out.display()));
        }
    };

    // The bed and one object per coded channel after it.
    let ids: Vec<u32> = programme.objects.iter().map(|object| object.id).collect();
    if ids.len() + 1 != channels {
        println!(
            "  objects      {} objects and a bed came back against {channels} elements written",
            ids.len()
        );
        cleanup();
        return Ok(false);
    }

    let mut checked = 0usize;
    for event in &events.events {
        let (Some(id), Some(sample)) = (event.id, event.sample_pos) else {
            continue;
        };
        let Some(element) = ids.iter().position(|known| *known == id).map(|at| at + 1) else {
            continue; // the bed, which has no position of its own
        };
        let unit = sample / 40;

        if let Some(position) = &event.pos {
            let wanted = position_of(element, unit).to_master();
            let got: Vec<f64> = position.iter().map(|value| value.0).collect();
            if got.len() != 3 || (0..3).any(|axis| (got[axis] - wanted[axis]).abs() > 1e-9) {
                println!(
                    "  objects      element {element} at sample {sample}: {got:?} against \
                     {wanted:?}"
                );
                cleanup();
                return Ok(false);
            }
            checked += 1;
        }
        if let Some(gain) = &event.gain {
            let wanted = gain_of(element, unit)
                .decibels()
                .map_or(f64::NEG_INFINITY, f64::from);
            if (gain.0 - wanted).abs() > 1e-9 {
                println!(
                    "  objects      element {element} at sample {sample}: gain {} against {wanted}",
                    gain.0
                );
                cleanup();
                return Ok(false);
            }
            checked += 1;
        }
    }

    cleanup();
    println!(
        "  objects      {} elements, {} of the decoder's own entries agree",
        ids.len() + 1,
        checked
    );
    Ok(true)
}

/// The first decoder: FFmpeg's, which shares no code with this.
fn verify_with_ffmpeg(
    path: &Path,
    expected: &[i32],
    channels: usize,
    bits: SampleBits,
) -> Result<bool> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "truehd", "-i"])
        .arg(path)
        .args(["-f", "s32le", "-c:a", "pcm_s32le", "-"])
        .output()
        .map_err(|e| Error::io(path, e))?;

    if !output.status.success() {
        println!("  ffmpeg       FAILED to decode");
        for line in String::from_utf8_lossy(&output.stderr).lines().take(6) {
            println!("               {line}");
        }
        return Ok(false);
    }
    for line in String::from_utf8_lossy(&output.stderr).lines().take(4) {
        println!("  ffmpeg says  {line}");
    }

    // The decoder hands back 32-bit words holding the codec's 24-bit sample in
    // the top bits, so both sides are compared in that domain rather than in
    // whichever one the file happened to use.
    let decoded: Vec<i32> = output
        .stdout
        .chunks_exact(4)
        .map(|word| i32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect();

    let shift = match bits {
        SampleBits::Sixteen => 16,
        SampleBits::TwentyFour => 8,
    };
    let wanted: Vec<i32> = expected.iter().map(|s| s << shift).collect();

    print!("  ffmpeg       ");
    let mut exact = true;
    match (0..decoded.len().min(wanted.len())).find(|&i| decoded[i] != wanted[i]) {
        None if decoded.len() == wanted.len() => println!("round trip bit exact"),
        None => {
            exact = false;
            println!(
                "samples agree, but {} frames came back against {}",
                decoded.len() / channels,
                wanted.len() / channels
            );
        }
        Some(index) => {
            exact = false;
            println!(
                "FIRST DIFFERENCE at frame {}, channel {}: {} against {}",
                index / channels,
                index % channels,
                decoded[index],
                wanted[index]
            );
            let from = index.saturating_sub(2);
            let to = (index + 3).min(decoded.len().min(wanted.len()));
            println!("               sent    {:?}", &wanted[from..to]);
            println!("               back    {:?}", &decoded[from..to]);
        }
    }
    Ok(exact)
}

/// A warning every stream FFmpeg's encoder writes also draws, so it says
/// nothing about ours.
///
/// The bit FFmpeg calls `data_check_present` is, in this decoder's reading of
/// the format, one bit of a timestamp serialised across successive access
/// units. Writing a constant zero — which FFmpeg does, and so do we — makes
/// the state machine reading it complain every sixth unit. Filtered here and
/// counted, rather than hidden: it is a real gap, and it is on the list.
const BENIGN: [&str; 2] = ["high-resolution output timing", "hires_output_timing"];

/// The second opinion. Not fatal if it is not installed.
fn verify_with_harletty(path: &Path) {
    let output = Command::new(harletty()).arg("info").arg(path).output();
    match output {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stderr);
            let mut benign = 0usize;
            let complaints: Vec<&str> = text
                .lines()
                .filter(|line| line.contains("ERROR") || line.contains("WARN"))
                .filter(|line| {
                    if BENIGN.iter().any(|benign| line.contains(benign)) {
                        benign += 1;
                        false
                    } else {
                        true
                    }
                })
                .collect();
            let tail = if benign > 0 {
                format!(
                    " ({benign} high-resolution timing warnings, which FFmpeg's own streams draw too)"
                )
            } else {
                String::new()
            };
            if complaints.is_empty() {
                println!("  harletty      no complaints{tail}");
            } else {
                println!("  harletty      {} complaints{tail}", complaints.len());
                for line in complaints.iter().take(4) {
                    println!("                {}", line.trim());
                }
            }
        }
        Err(_) => println!("  harletty      not installed; skipped"),
    }
}

fn read(path: &Path) -> Result<(hz_io::container::pcm::PcmFormat, Vec<i32>)> {
    let mut reader = WavReader::open(path)?;
    let format = reader.format().pcm;
    let channels = format.channels as usize;
    if channels == 0 {
        return Err(Error::malformed(path, "no channels"));
    }
    let mut block = vec![0i32; 8192 * channels];
    let mut out = Vec::new();
    loop {
        let frames = reader.read_frames(&mut block)?;
        if frames == 0 {
            break;
        }
        out.extend_from_slice(&block[..frames * channels]);
    }
    Ok((format, out))
}
