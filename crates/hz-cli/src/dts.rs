//! `harlettizer dts`: a programme rendered to 5.1, 7.1 or 7.1.4 and written
//! as DTS-HD Master Audio — for 7.1.4, with the DTS:X extension's standard
//! profile.
//!
//! The objects do not survive as objects: the standard profile is a fixed
//! 7.1.4, and the profiles that carry objects are phase C6. So the master is
//! rendered as `iamf` renders it — its bed channels routed to their
//! speakers, its objects panned on the room's cube — onto the layout asked
//! for, and [`hz_dca::master`] writes it: the lossless bed beside the 5.1
//! core made from it, and for 7.1.4 each height folded into the corner below
//! it at −3 dB and carried beside the 7.1 that makes, in the extension block.
//! The cube has no place for a 5.1 surround at ±110°, so a 5.1 is rendered
//! on 7.1 and folded as `ac3` folds it
//! ([`hz_render::fold::presentation_matrix`]).
//!
//! A corner and its height together can pass full scale where neither does,
//! and a composite past 24 bits is one a decoder would clip and could not
//! take the height back out of. So a limiter, one gain for every channel as
//! an element's mix has ([`hz_cluster::mix::Limiter`]), watches the channels
//! — and for 7.1.4 the composite as well — and holds them under full scale
//! before the samples become integers: the headroom the fold needs, at the
//! cost of a gain that dips where the composite peaks. What it did is
//! reported.
//!
//! A 7.1.4 frame whose bed is too large for the navigation word to point
//! past ([`hz_dca::navigation::MAX_OFFSET`]) ends the encode with an error
//! naming it. The programme material measured fits — the bed reaches 7,572
//! bytes of the 8,188 on the four standard-profile reference streams' 7.1.4,
//! 7,716 on a rendered master — but white noise peaking at −48 dBFS in every
//! channel does not (`docs/dts.md`). What was encoded before it is left
//! written, and a Matroska file is closed around it.
//!
//! The output is a raw elementary stream — each frame its core, then its
//! extension substream — or, when `--out` ends in `.mka` or `.mkv`, the one
//! track of a Matroska file, `A_DTS`, those same frames a block each.

use hz_cluster::mix::Limiter;
use hz_core::{Error, Result};
use hz_dca::encoder::FRAME_SAMPLES;
use hz_dca::lossless::Layout as DcaLayout;
use hz_dca::master::{Config as DcaConfig, Encoder};
use hz_io::container::SampleFormat;
use hz_matroska::Muxer;
use hz_programme::follow::Following;
use hz_programme::progress::Progress;
use hz_programme::quantise::{Levels, Quantiser};
use hz_programme::source::Source;
use hz_programme::{Part, parts_of};
use hz_render::fold::{FoldMode, presentation_matrix};
use hz_render::{Keyframe, Layout, Mixdown};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// What `dts` was asked to do.
pub struct Config {
    pub input: PathBuf,
    pub out: PathBuf,
    pub frames: Option<u64>,
    pub mono_prefix: Option<PathBuf>,
    /// 5.1, 7.1, or 7.1.4 — the standard DTS:X profile.
    pub layout: DcaLayout,
    /// Bits of the lossless layer: 16 or 24, and 24 for 7.1.4.
    pub bits: u8,
    /// The core's subband prediction.
    pub adpcm: bool,
    pub progress: bool,
}

/// Samples rendered and limited at once: eight DTS frames, so that the
/// limiter sees a peak well ahead of it.
const BLOCK: usize = 8 * FRAME_SAMPLES;

/// 48 kHz: what the lossless layer beside a core takes.
const SAMPLE_RATE: u32 = 48_000;

/// The WAV channels a 7.1.4's heights fold into — L, R, Lsr, Rsr — at
/// −3 dB, and that gain.
const CORNERS: [usize; 4] = [0, 1, 4, 5];
const FOLD: f64 = 23170.0 / 32768.0;

/// The layout the render is made on, and what makes each channel the
/// encoder takes, in WAV order, of the render's: a row of gains over the
/// render's channels. The render's 7.1 has the sides before the rears (L R C
/// LFE Lss Rss Lrs Rrs); WAV, and the encoder, the rears first. A 5.1 is
/// the 7.1's fold.
fn render_layout(path: &Path, layout: DcaLayout) -> Result<(Layout, Vec<Vec<f64>>)> {
    let pick = |rendered: Layout, order: &[usize]| {
        let width = rendered.channels();
        let rows = order
            .iter()
            .map(|&from| (0..width).map(|r| f64::from(u8::from(r == from))).collect())
            .collect();
        (rendered, rows)
    };
    Ok(match layout {
        DcaLayout::FivePointOne => {
            let fold = presentation_matrix(&Layout::surround_5_1(), FoldMode::default())
                .map_err(|e| Error::unsupported(path, e.to_string()))?
                .expect("5.1 is not 7.1");
            let rendered = Layout::surround_7_1();
            let rows = (0..6)
                .map(|to| fold.iter().map(|row| row[to]).collect())
                .collect();
            (rendered, rows)
        }
        DcaLayout::SevenPointOne => pick(Layout::surround_7_1(), &[0, 1, 2, 3, 6, 7, 4, 5]),
        DcaLayout::SevenPointOneFour => pick(
            Layout::surround_7_1_4(),
            &[0, 1, 2, 3, 6, 7, 4, 5, 8, 9, 10, 11],
        ),
    })
}

/// Where the frames go.
enum Sink {
    /// One after the other: a raw elementary stream.
    Raw(BufWriter<File>),
    /// A block each, in the one track of a Matroska file.
    Matroska {
        out: BufWriter<File>,
        muxer: Muxer,
        frames: u64,
    },
}

impl Sink {
    fn create(path: &Path, layout: DcaLayout, bits: u8) -> Result<Self> {
        let file = File::create(path).map_err(|e| Error::io(path, e))?;
        let mut out = BufWriter::new(file);
        if !crate::is_matroska(path) {
            return Ok(Sink::Raw(out));
        }
        // `A_DTS` has no private data: every frame states its own shape, and
        // a player finds the stream's in the first. `Channels` is what a
        // decoder of the lossless layer presents: a 7.1.4's 7.1, which a
        // DTS:X decoder unfolds. The core has no delay to state: the layers
        // decode in step with the input.
        let description = format!("A_DTS {layout:?} {bits}");
        let track = hz_matroska::Track {
            codec_id: "A_DTS",
            codec_private: None,
            sample_rate: SAMPLE_RATE,
            frame: FRAME_SAMPLES,
            channels: layout.bed_channels() as u32,
            bits: Some(u32::from(bits)),
            codec_delay: None,
            seek_pre_roll: None,
            uid: hz_matroska::uid(description.as_bytes()),
            writing_app: "harlettizer",
        };
        let (muxer, _) = Muxer::start(&mut out, &track).map_err(|e| Error::io(path, e))?;
        Ok(Sink::Matroska {
            out,
            muxer,
            frames: 0,
        })
    }

    /// What one call of the encoder sent — a frame as a rule, none on the
    /// first call, and as many as a smoothing buffer releases at once — and
    /// how many frames that was; `end_trim` is the samples of padding at the
    /// end of the last, which Matroska states.
    fn frames(&mut self, stream: &[u8], end_trim: usize) -> std::io::Result<u64> {
        let mut cut = hz_dca::split::frames(stream).peekable();
        let mut sent = 0;
        while let Some(frame) = cut.next() {
            let frame = frame.map_err(|e| std::io::Error::other(e.0))?;
            let len = frame.core.map_or(0, <[u8]>::len) + frame.exss.map_or(0, <[u8]>::len);
            let bytes = &stream[frame.at..frame.at + len];
            match self {
                Sink::Raw(out) => out.write_all(bytes)?,
                Sink::Matroska { out, muxer, frames } => {
                    let trim = if cut.peek().is_none() { end_trim } else { 0 };
                    muxer.block(out, *frames, bytes, trim)?;
                    *frames += 1;
                }
            }
            sent += 1;
        }
        Ok(sent)
    }

    /// Close the output; `presented` is the samples a channel a decoder
    /// hands out, after the padding.
    fn finish(&mut self, presented: u64) -> std::io::Result<()> {
        match self {
            Sink::Raw(out) => out.flush(),
            Sink::Matroska { out, muxer, .. } => {
                muxer.finish(out, presented)?;
                out.flush()
            }
        }
    }
}

pub fn run(config: Config) -> Result<()> {
    // The lossless layer on every thread but one, the core on two more, and
    // the render on this one, which mostly waits for them: the same stream,
    // to the byte, several times sooner.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1));
    encode_on(config, threads)
}

/// [`run`], the lossless layer on `threads` threads of its own — nought for
/// everything on this one.
fn encode_on(config: Config, threads: usize) -> Result<()> {
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
    if source.sample_rate() != SAMPLE_RATE {
        return Err(Error::unsupported(
            path,
            format!(
                "{} Hz; the DTS-HD Master Audio encoder takes 48 kHz",
                source.sample_rate()
            ),
        ));
    }
    let input_scale = 1.0 / f64::from(1u32 << (format.bits_per_channel - 1));
    let stride = source.channels();

    let (layout, to_wav) = render_layout(path, config.layout)?;
    let rendered = layout.channels();
    let width = config.layout.channels();
    let heights = config.layout == DcaLayout::SevenPointOneFour;
    let lfe = layout
        .index_of("LFE1")
        .expect("every layout here has an LFE");

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

    let mut dca = DcaConfig::reference(config.layout, config.bits);
    dca.core.adpcm = config.adpcm;
    let mut encoder = Encoder::new(dca)
        .map_err(|e| Error::unsupported(&config.out, e.0))?
        .with_threads(threads);
    let mut sink = Sink::create(&config.out, config.layout, config.bits)?;

    let total = config
        .frames
        .map_or(source.frames(), |limit| limit.min(source.frames()));
    let mut progress = Progress::new(config.progress, total);

    let quantiser = Quantiser::bits(u32::from(config.bits));
    let shift = 32 - u32::from(config.bits);
    // A few steps under the top, for the rounding of the channels and of the
    // fold between the limiter's floats and the encoder's integers.
    let ceiling = (quantiser.full_scale() - 8.0) / quantiser.full_scale();
    let mut levels = Levels::default();
    let mut limiter = Limiter::new();

    let mut raw = vec![0i32; BLOCK * stride];
    let mut input = vec![0f32; BLOCK * stride];
    let mut mixed = vec![0f64; BLOCK * rendered];
    // The channels in WAV order, then for 7.1.4 the eight of the composite
    // the encoder will make of them: the limiter holds them all.
    let mut planes = vec![vec![0f32; BLOCK]; width + if heights { 8 } else { 0 }];
    let mut pcm = vec![0i32; BLOCK * width];
    let mut stream = Vec::new();

    let mut at = 0u64;
    let mut sent = 0u64;
    let mut refused = None;
    'blocks: while at < total {
        let want = (total - at).min(BLOCK as u64) as usize;
        let got = match source.fill(&mut raw, want) {
            Ok(got) => got,
            // The frames before stay where a single thread would have left
            // them: those the threads still have are coded and written
            // first — or the refusal one of them meets, which a single
            // thread would have met before this read, ends the stream as it
            // would have.
            Err(e) => {
                stream.clear();
                let stopped = encoder.stop(&mut stream);
                sent += sink
                    .frames(&stream, 0)
                    .map_err(|e| Error::io(&config.out, e))?;
                match stopped {
                    Ok(()) => return Err(e),
                    Err(e) => {
                        refused = Some(e);
                        break 'blocks;
                    }
                }
            }
        };
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
            &mut mixed[..got * rendered],
        );
        // The block whole, padded with silence to the frame.
        let frames = got.div_ceil(FRAME_SAMPLES);
        let samples = frames * FRAME_SAMPLES;
        for plane in planes.iter_mut() {
            plane.resize(samples, 0.0);
        }
        for n in 0..samples {
            let frame = &mixed[n * rendered..(n + 1) * rendered];
            let channel =
                |wav: usize| -> f64 { frame.iter().zip(&to_wav[wav]).map(|(x, g)| x * g).sum() };
            for (wav, plane) in planes[..width].iter_mut().enumerate() {
                plane[n] = channel(wav) as f32;
            }
            if heights {
                for ch in 0..8 {
                    let height = CORNERS.iter().position(|&c| c == ch);
                    planes[12 + ch][n] = match height {
                        Some(h) => (channel(ch) + FOLD * channel(8 + h)) as f32,
                        None => channel(ch) as f32,
                    };
                }
            }
        }
        limiter.apply(&mut planes, ceiling);
        for n in 0..samples {
            for ch in 0..width {
                pcm[n * width + ch] =
                    quantiser.code(f64::from(planes[ch][n]), &mut levels) << shift;
            }
        }
        for frame in pcm[..samples * width].chunks_exact(FRAME_SAMPLES * width) {
            stream.clear();
            let encoded = encoder.encode(frame, &mut stream);
            // The first call sends nothing: the core runs a frame ahead. A
            // refusal still hands back the frames before it that the threads
            // had not yet given out.
            sent += sink
                .frames(&stream, 0)
                .map_err(|e| Error::io(&config.out, e))?;
            if let Err(e) = encoded {
                refused = Some(e);
                break 'blocks;
            }
        }
        at += got as u64;
        if let Some(progress) = progress.as_mut() {
            progress.at(at);
        }
        if got < want {
            break;
        }
    }
    if refused.is_none() && at == 0 {
        sink.finish(0).map_err(|e| Error::io(&config.out, e))?;
        return Err(Error::unsupported(path, "no audio to encode"));
    }
    if refused.is_none() {
        stream.clear();
        match encoder.finish(&mut stream) {
            Ok(()) => {
                // The last frame is the one padded with silence: the stream
                // has a frame for every frame of input begun.
                let padding = at.div_ceil(FRAME_SAMPLES as u64) * FRAME_SAMPLES as u64 - at;
                sent += sink
                    .frames(&stream, padding as usize)
                    .map_err(|e| Error::io(&config.out, e))?;
            }
            Err(e) => {
                // The frames before the refused one, which the threads still
                // had in hand.
                sent += sink
                    .frames(&stream, 0)
                    .map_err(|e| Error::io(&config.out, e))?;
                refused = Some(e);
            }
        }
    }
    // What was sent before a refusal is left written, a Matroska file
    // closed around it.
    let presented = match refused {
        None => at,
        Some(_) => sent * FRAME_SAMPLES as u64,
    };
    sink.finish(presented)
        .map_err(|e| Error::io(&config.out, e))?;
    if let Some(e) = refused {
        return Err(Error::refused(&config.out, e.to_string()));
    }

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
            "{} placed where {} has no speaker",
            plural(placed, "bed channel", "bed channels"),
            layout.name
        ));
    }
    println!(
        "  programme    {}, rendered to {} on the room's cube{}",
        parts.join(", "),
        layout.name,
        if config.layout == DcaLayout::FivePointOne {
            " and folded to 5.1"
        } else {
            ""
        }
    );
    let seconds = at as f64 / f64::from(SAMPLE_RATE);
    let bits = config.bits;
    let what = match config.layout {
        DcaLayout::SevenPointOneFour => format!(
            "DTS-HD MA + DTS:X, the 7.1.4 heights folded into a lossless {bits}-bit 7.1 at −3 dB"
        ),
        DcaLayout::SevenPointOne => {
            format!("DTS-HD MA, a lossless {bits}-bit 7.1 with its 5.1 fold embedded")
        }
        DcaLayout::FivePointOne => format!("DTS-HD MA, a lossless {bits}-bit 5.1"),
    };
    println!(
        "  encoded      {sent} frames, {at} samples: {what}, a {}-byte 5.1 core; lossless \
         layer {:.0} kb/s",
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
        "  headroom     {} peaking at {:.2} of full scale before the limiter, held under it in \
         {} blocks of {BLOCK}",
        if heights {
            "the composite"
        } else {
            "the render"
        },
        limiter.peak * ceiling,
        limiter.limited
    );
    let bytes = std::fs::metadata(&config.out)
        .map_err(|e| Error::io(&config.out, e))?
        .len();
    println!(
        "  wrote        {}, {}, {bytes} bytes, {:.0} kbit/s",
        config.out.display(),
        if crate::is_matroska(&config.out) {
            "Matroska (A_DTS)"
        } else {
            "a raw elementary stream"
        },
        bytes as f64 * 8.0 / seconds / 1000.0
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Forty frames and some.
    const SAMPLES: usize = 40 * FRAME_SAMPLES + 300;

    /// Audio that ends before its header says, in the thirtieth frame: the
    /// fourth block's read fails, after 24 frames handed to the encoder.
    const CUT: usize = 29 * FRAME_SAMPLES + 100;

    /// The master at `input`, of `channels` 24-bit channels, cut at [`CUT`].
    fn cut(input: &Path, channels: usize) {
        let audio = input.with_extension("atmos.audio");
        let length = std::fs::metadata(&audio).unwrap().len();
        let missing = ((SAMPLES - CUT) * channels * 3) as u64;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&audio)
            .unwrap()
            .set_len(length - missing)
            .unwrap();
    }

    /// A 7.1.4 bed in `dir`, [`SAMPLES`] long: tones, then from frame 22
    /// on white noise at −6 dBFS in every channel, which no navigation word
    /// can reach past (docs/dts.md): its config's path.
    fn noise_from_frame_22(dir: &Path) -> PathBuf {
        const BED: [(&str, u32); 12] = [
            ("L", 0),
            ("R", 1),
            ("C", 2),
            ("LFE", 3),
            ("Lrs", 6),
            ("Rrs", 7),
            ("Lss", 4),
            ("Rss", 5),
            ("Lfh", 8),
            ("Rfh", 9),
            ("Lrh", 12),
            ("Rrh", 13),
        ];
        let ids: Vec<String> = BED.iter().map(|(_, id)| id.to_string()).collect();
        let mut config = format!(
            "language: eng\nversion: 0.5.1\npresentations:\n  - type: home\n    simplified: \
             false\n    metadata: m.atmos.metadata\n    audio: m.atmos.audio\n    offset: 0.0\n    \
             fps: 24\n    scBedConfiguration: [{}]\n    creationTool: test\n    \
             creationToolVersion: 0.0.0\n    bedInstances:\n      - channels:\n",
            ids.join(", ")
        );
        let mut events = String::from("sampleRate: 48000\nevents:\n");
        for (name, id) in BED {
            config.push_str(&format!(
                "          - channel: {name}\n            ID: {id}\n"
            ));
            events.push_str(&format!(
                "  - ID: {id}\n    samplePos: 0\n    active: true\n    importance: 1.0\n    \
                 gain: 0\n    rampLength: 0\n"
            ));
        }
        config.push_str("    objects: []\n");
        let path = dir.join("m.atmos");
        std::fs::write(&path, config).unwrap();
        std::fs::write(dir.join("m.atmos.metadata"), events).unwrap();
        let mut audio = hz_io::container::caf::CafWriter::create(
            &dir.join("m.atmos.audio"),
            hz_io::container::pcm::PcmFormat::master(48_000.0, BED.len() as u32),
        )
        .unwrap();
        let pcm: Vec<i32> = (0..SAMPLES * BED.len())
            .map(|i| {
                let (channel, n) = (i % BED.len(), i / BED.len());
                if n < 22 * FRAME_SAMPLES {
                    let hz = 110.0 * (channel + 1) as f64 * 1.37;
                    let phase = std::f64::consts::TAU * hz * n as f64 / 48_000.0;
                    (phase.sin() * f64::from(1 << 20)).round() as i32
                } else {
                    let mut x = (i as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                    x ^= x >> 29;
                    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
                    x ^= x >> 32;
                    ((x as i64 % (1 << 22)) as i32).clamp(-(1 << 22), (1 << 22) - 1)
                }
            })
            .collect();
        audio.write_frames(&pcm).unwrap();
        audio.finish().unwrap();
        path
    }

    /// An encode the cut audio stops: the same error, on threads or not,
    /// and the same stream left — raw or in Matroska, ADPCM or not —
    /// however many threads, and however far they got: a few runs each to
    /// show it.
    ///
    /// From the tones, the read fails: one thread has sent 23 frames, the
    /// 24th waiting for the core that decodes to it, and the threads' are
    /// sent when the read fails. From the noise, frame 22 is refused: one
    /// thread meets the refusal sending it, before the read; on threads
    /// the read can fail first, and the refusal comes back from the frames
    /// the threads still have, the stream ending at it as on one thread.
    #[test]
    fn a_read_that_fails_stops_the_encode_where_one_thread_stops() {
        let tones = crate::fixtures::scratch("dts", "cut-tones");
        let tones = {
            let input = crate::fixtures::master(&tones, SAMPLES);
            cut(&input, 10);
            input
        };
        let noise = crate::fixtures::scratch("dts", "cut-noise");
        let noise = {
            let input = noise_from_frame_22(&noise);
            cut(&input, 12);
            input
        };
        let every = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1));
        for (input, layout, mka, adpcm, sent, said) in [
            (
                &tones,
                DcaLayout::FivePointOne,
                false,
                false,
                Some(23),
                "failed to fill",
            ),
            (
                &tones,
                DcaLayout::SevenPointOneFour,
                false,
                false,
                Some(23),
                "failed to fill",
            ),
            (
                &tones,
                DcaLayout::SevenPointOneFour,
                true,
                false,
                None,
                "failed to fill",
            ),
            (
                &tones,
                DcaLayout::FivePointOne,
                false,
                true,
                Some(23),
                "failed to fill",
            ),
            (
                &tones,
                DcaLayout::FivePointOne,
                true,
                true,
                None,
                "failed to fill",
            ),
            (
                &noise,
                DcaLayout::SevenPointOneFour,
                false,
                false,
                Some(22),
                "frame 22",
            ),
            (
                &noise,
                DcaLayout::SevenPointOneFour,
                true,
                false,
                None,
                "frame 22",
            ),
        ] {
            let out = input.with_file_name(if mka { "cut.mka" } else { "cut.dts" });
            let coded = |threads: usize| {
                let config = Config {
                    input: input.clone(),
                    out: out.clone(),
                    frames: None,
                    mono_prefix: None,
                    layout,
                    bits: 24,
                    adpcm,
                    progress: false,
                };
                let ended = encode_on(config, threads).map_err(|e| e.to_string());
                (ended, std::fs::read(&out).unwrap())
            };
            let case = format!("{layout:?}, mka {mka}, adpcm {adpcm}, {said}");
            let (inline, alone) = coded(0);
            let error = inline.as_ref().expect_err("the encode stops short");
            assert!(error.contains(said), "{case}: {error}");
            if let Some(sent) = sent {
                let frames: Vec<_> = hz_dca::split::frames(&alone)
                    .collect::<std::result::Result<_, _>>()
                    .unwrap();
                assert_eq!(frames.len(), sent, "{case}: the frames sent");
            }
            for threads in [1, 2, 3, 8, every] {
                let runs = match (threads == every, sent.is_some() && !adpcm) {
                    (true, true) => 8,
                    (true, false) => 3,
                    (false, _) => 1,
                };
                for run in 0..runs {
                    let (threaded, beside) = coded(threads);
                    let at = format!("{case}, {threads} threads, run {run}");
                    assert_eq!(threaded, inline, "{at}: said the same way");
                    assert!(alone == beside, "{at}: the same stream left");
                }
            }
        }
    }
}
