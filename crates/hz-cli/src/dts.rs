//! `harlettizer dts`: a programme rendered to 7.1.4 and written as DTS-HD
//! Master Audio with the DTS:X extension's standard profile.
//!
//! The objects do not survive as objects: the standard profile is a fixed
//! 7.1.4, and the profiles that carry objects are phase C6. So the master is
//! rendered as `iamf` renders it — its bed channels routed to their
//! speakers, its objects panned on the room's cube — onto 7.1.4, and
//! [`hz_dca::master`] writes it: each height folded into the corner below it
//! at −3 dB, the lossless 7.1 that makes, the 5.1 core of that, and the
//! heights in the extension block beside it.
//!
//! A corner and its height together can pass full scale where neither does,
//! and a composite past 24 bits is one a decoder would clip and could not
//! take the height back out of. So a limiter, one gain for all twelve
//! channels as an element's mix has ([`hz_cluster::mix::Limiter`]), watches
//! the composite as well as the channels and holds both under full scale
//! before the samples become integers: the headroom the fold needs, at the
//! cost of a gain that dips where the composite peaks. What it did is
//! reported.
//!
//! A frame whose bed is too large for the navigation word to point past
//! ([`hz_dca::navigation::MAX_OFFSET`]) ends the encode with an error naming
//! it. The programme material measured fits — the bed reaches 7,572 bytes of
//! the 8,188 on the four standard-profile reference streams' 7.1.4, 7,716 on
//! a rendered master — but white noise peaking at −48 dBFS in every channel
//! does not (`docs/dts.md`). A raw `.dts`
//! elementary stream is written; Matroska is phase C5.

use hz_cluster::mix::Limiter;
use hz_core::{Error, Result};
use hz_dca::encoder::FRAME_SAMPLES;
use hz_dca::lossless::Layout as DcaLayout;
use hz_dca::master::{Config as DcaConfig, Encoder};
use hz_io::container::SampleFormat;
use hz_programme::follow::Following;
use hz_programme::progress::Progress;
use hz_programme::quantise::{Levels, Quantiser};
use hz_programme::source::Source;
use hz_programme::{Part, parts_of};
use hz_render::{Keyframe, Layout, Mixdown};
use std::io::Write;
use std::path::PathBuf;

/// What `dts` was asked to do.
pub struct Config {
    pub input: PathBuf,
    pub out: PathBuf,
    pub frames: Option<u64>,
    pub mono_prefix: Option<PathBuf>,
    /// The core's subband prediction.
    pub adpcm: bool,
    pub progress: bool,
}

/// Samples rendered and limited at once: eight DTS frames, so that the
/// limiter sees a peak well ahead of it.
const BLOCK: usize = 8 * FRAME_SAMPLES;

/// The render's 7.1.4 (L R C LFE, the sides, the rears, the heights) in the
/// encoder's WAV order (L R C LFE, the rears, the sides, the heights): the
/// render channel each input channel is.
const WAV_FROM_RENDER: [usize; 12] = [0, 1, 2, 3, 6, 7, 4, 5, 8, 9, 10, 11];

/// The WAV channels the heights fold into — L, R, Lsr, Rsr — at −3 dB, and
/// that gain.
const CORNERS: [usize; 4] = [0, 1, 4, 5];
const FOLD: f64 = 23170.0 / 32768.0;

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
    if source.sample_rate() != 48000 {
        return Err(Error::unsupported(
            path,
            format!(
                "{} Hz; the DTS:X encoder takes 48 kHz",
                source.sample_rate()
            ),
        ));
    }
    let input_scale = 1.0 / f64::from(1u32 << (format.bits_per_channel - 1));
    let stride = source.channels();

    let layout = Layout::surround_7_1_4();
    let width = layout.channels();
    let lfe = layout.index_of("LFE1").expect("7.1.4 has an LFE");

    // Every track to a source of the mixdown, as `iamf` routes them.
    let mut following = Following::new(Mixdown::new(&layout)?);
    let (mut routed, mut placed, mut objects) = (0usize, 0usize, 0usize);
    for part in parts_of(path, &source)? {
        match part {
            Part::Lfe { channel } => {
                following.mixdown().speaker(channel, lfe);
                routed += 1;
            }
            Part::Bed {
                channel,
                name,
                position,
            } => {
                let label = hz_core::speakers::by_master_name(name).map_or(name, |s| s.label);
                let mixdown = following.mixdown();
                if let Some(speaker) = layout.index_of(label) {
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
                objects += 1;
            }
        }
    }
    if routed + placed + objects == 0 {
        return Err(Error::unsupported(path, "nothing in it to render"));
    }

    let mut dca = DcaConfig::reference(DcaLayout::SevenPointOneFour, 24);
    dca.core.adpcm = config.adpcm;
    let mut encoder = Encoder::new(dca).map_err(|e| Error::unsupported(&config.out, e.0))?;
    let out_file = std::fs::File::create(&config.out).map_err(|e| Error::io(&config.out, e))?;
    let mut out = std::io::BufWriter::new(out_file);

    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    let mut progress = Progress::new(config.progress, total);

    let quantiser = Quantiser::bits(24);
    // A few steps under the top, for the rounding of twelve samples and of
    // the fold between the limiter's floats and the encoder's integers.
    let ceiling = (quantiser.full_scale() - 8.0) / quantiser.full_scale();
    let mut levels = Levels::default();
    let mut limiter = Limiter::new();

    let mut raw = vec![0i32; BLOCK * stride];
    let mut input = vec![0f32; BLOCK * stride];
    let mut mixed = vec![0f64; BLOCK * width];
    // The twelve channels in WAV order, then the eight of the composite the
    // encoder will make of them: the limiter holds them all.
    let mut planes = vec![vec![0f32; BLOCK]; 12 + 8];
    let mut pcm = vec![0i32; BLOCK * 12];
    let mut stream = Vec::new();

    let mut at = 0u64;
    let mut sent = 0u64;
    while at < total {
        let want = (total - at).min(BLOCK as u64) as usize;
        let got = source.fill(&mut raw, want)?;
        if got == 0 {
            break;
        }
        for (x, &r) in input[..got * stride].iter_mut().zip(&raw[..got * stride]) {
            *x = (f64::from(r) * input_scale) as f32;
        }
        mixed.fill(0.0);
        following.render(
            &input[..got * stride],
            stride,
            got,
            at,
            &mut mixed[..got * width],
        );
        // The block whole, padded with silence to the frame.
        let frames = got.div_ceil(FRAME_SAMPLES);
        let samples = frames * FRAME_SAMPLES;
        for plane in planes.iter_mut() {
            plane.resize(samples, 0.0);
        }
        for n in 0..samples {
            let frame = &mixed[n * width..(n + 1) * width];
            for (wav, &render) in WAV_FROM_RENDER.iter().enumerate() {
                planes[wav][n] = frame[render] as f32;
            }
            for ch in 0..8 {
                let height = CORNERS.iter().position(|&c| c == ch);
                planes[12 + ch][n] = match height {
                    Some(h) => (frame[WAV_FROM_RENDER[ch]] + FOLD * frame[8 + h]) as f32,
                    None => frame[WAV_FROM_RENDER[ch]] as f32,
                };
            }
        }
        limiter.apply(&mut planes, ceiling);
        for n in 0..samples {
            for ch in 0..12 {
                pcm[n * 12 + ch] = quantiser.code(f64::from(planes[ch][n]), &mut levels) << 8;
            }
        }
        for frame in pcm[..samples * 12].chunks_exact(FRAME_SAMPLES * 12) {
            stream.clear();
            encoder
                .encode(frame, &mut stream)
                .map_err(|e| Error::refused(&config.out, e.to_string()))?;
            out.write_all(&stream)
                .map_err(|e| Error::io(&config.out, e))?;
            sent += 1;
        }
        at += got as u64;
        if let Some(progress) = progress.as_mut() {
            progress.at(at);
        }
        if got < want {
            break;
        }
    }
    if at == 0 {
        return Err(Error::unsupported(path, "no audio to encode"));
    }
    stream.clear();
    encoder
        .finish(&mut stream)
        .map_err(|e| Error::refused(&config.out, e.to_string()))?;
    out.write_all(&stream)
        .map_err(|e| Error::io(&config.out, e))?;
    out.flush().map_err(|e| Error::io(&config.out, e))?;

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
            "{} placed where 7.1.4 has no speaker",
            plural(placed, "bed channel", "bed channels")
        ));
    }
    println!(
        "  programme    {}, rendered to 7.1.4 on the room's cube",
        parts.join(", ")
    );
    let seconds = at as f64 / 48000.0;
    println!(
        "  encoded      {sent} frames, {at} samples: DTS-HD MA + DTS:X, the 7.1.4 heights folded \
         into a lossless 24-bit 7.1 at −3 dB, a {}-byte 5.1 core; lossless layer {:.0} kb/s",
        encoder.core_frame_size(),
        encoder.lossless_bytes() as f64 * 8.0 / seconds / 1000.0
    );
    if let Some((offset, size)) = encoder.extension_reach() {
        println!(
            "  navigation   the extension at most {offset} bytes into a frame of the {} the word \
             reaches, {size} bytes long at most",
            hz_dca::navigation::MAX_OFFSET
        );
    }
    println!(
        "  headroom     the composite peaking at {:.2} of full scale before the limiter, held \
         under it in {} blocks of {BLOCK}",
        limiter.peak * ceiling,
        limiter.limited
    );
    let bytes = std::fs::metadata(&config.out)
        .map_err(|e| Error::io(&config.out, e))?
        .len();
    println!(
        "  wrote        {}, {bytes} bytes, {:.0} kbit/s",
        config.out.display(),
        bytes as f64 * 8.0 / seconds / 1000.0
    );
    Ok(())
}
