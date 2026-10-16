//! `harlettizer dts` end to end: a master set written here, encoded by the
//! command, and what it wrote read back.
//!
//! Every test reads the output with this workspace's own readers: the raw
//! stream and the Matroska track carry the same frames, every header
//! rewrites byte for byte, and a 7.1.4's navigation word finds its block on
//! every frame. Where FFmpeg is installed, one more decodes the `.mka` and
//! holds the lossless layer to the master, bit for bit; without it that
//! test says so and passes. The `dca` oracle is the harness's
//! (`cargo xtask dts --features dca-oracle`), which reads `.mka` too.

use hz_dca::core::CoreHeader;
use hz_dca::exss::ExssHeader;
use hz_dca::navigation::Navigation;
use hz_io::container::caf::CafWriter;
use hz_io::container::pcm::PcmFormat;
use hz_matroska::{id, read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: usize = 48_000;
const FRAME: usize = 512;

/// Ten frames and a bit: the last one padded.
const SAMPLES: usize = 10 * FRAME + 200;

/// The master-format bed channels of each layout, in WAV order: what routes
/// to the layout's speakers one for one.
const BED_7_1: [(&str, u32); 8] = [
    ("L", 0),
    ("R", 1),
    ("C", 2),
    ("LFE", 3),
    ("Lrs", 6),
    ("Rrs", 7),
    ("Lss", 4),
    ("Rss", 5),
];
const HEIGHTS: [(&str, u32); 4] = [("Lfh", 8), ("Rfh", 9), ("Lrh", 12), ("Rrh", 13)];

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("dts-cli")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A master set of bed channels only, `signal(channel, sample)` in 24-bit
/// integers each: its config's path.
fn master(
    dir: &Path,
    bed: &[(&str, u32)],
    samples: usize,
    signal: impl Fn(usize, usize) -> i32,
) -> PathBuf {
    let mut config = String::from(
        "language: eng\nversion: 0.5.1\npresentations:\n  - type: home\n    simplified: false\n    \
         metadata: m.atmos.metadata\n    audio: m.atmos.audio\n    offset: 0.0\n    fps: 24\n",
    );
    let ids: Vec<String> = bed.iter().map(|(_, id)| id.to_string()).collect();
    config.push_str(&format!(
        "    scBedConfiguration: [{}]\n    creationTool: test\n    creationToolVersion: 0.0.0\n    \
         bedInstances:\n      - channels:\n",
        ids.join(", ")
    ));
    for (name, id) in bed {
        config.push_str(&format!(
            "          - channel: {name}\n            ID: {id}\n"
        ));
    }
    config.push_str("    objects: []\n");
    let path = dir.join("m.atmos");
    std::fs::write(&path, config).unwrap();

    let mut events = String::from("sampleRate: 48000\nevents:\n");
    for (_, id) in bed {
        events.push_str(&format!(
            "  - ID: {id}\n    samplePos: 0\n    active: true\n    importance: 1.0\n    gain: 0\n    \
             rampLength: 0\n"
        ));
    }
    std::fs::write(dir.join("m.atmos.metadata"), events).unwrap();

    let width = bed.len();
    let mut writer = CafWriter::create(
        &dir.join("m.atmos.audio"),
        PcmFormat::master(RATE as f64, width as u32),
    )
    .unwrap();
    let pcm: Vec<i32> = (0..samples * width)
        .map(|i| signal(i % width, i / width))
        .collect();
    writer.write_frames(&pcm).unwrap();
    writer.finish().unwrap();
    path
}

/// A tone per channel at about −14 dBFS: what codes small enough for any
/// layout's navigation, and leaves the limiter nothing to do.
fn tones(channel: usize, n: usize) -> i32 {
    let hz = 110.0 * (channel + 1) as f64 * 1.37;
    let phase = std::f64::consts::TAU * hz * n as f64 / RATE as f64;
    (phase.sin() * f64::from(1 << 20)).round() as i32
}

fn harlettizer(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harlettizer"))
        .args(args)
        .output()
        .unwrap()
}

fn encode(input: &Path, out: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "dts",
        input.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let output = harlettizer(&args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// The frames of a raw stream, each checked: a core whose header rewrites
/// byte for byte, a substream header that does with a valid checksum, and
/// for a 7.1.4 a navigation word pointing at the extension block.
fn check_frames(stream: &[u8], heights: bool) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    for frame in hz_dca::split::frames(stream) {
        let frame = frame.unwrap();
        let core = frame.core.expect("every frame has its core");
        let header = CoreHeader::read(core).unwrap();
        let mut w = hz_core::bits::BitWriter::new();
        header.write(&mut w);
        assert_eq!(w.bytes(), &core[..header.len()], "core at {}", frame.at);
        assert_eq!(header.samples() as usize, FRAME);

        let exss = frame.exss.expect("every frame has its substream");
        let header = ExssHeader::read(exss).unwrap();
        assert!(ExssHeader::crc_is_valid(exss, header.header_size));
        assert_eq!(
            header.write().unwrap(),
            exss[..header.header_size as usize],
            "substream at {}",
            frame.at
        );
        let asset = &header.assets[0];
        assert!(asset.coding.xll().is_some(), "a lossless asset");
        let navigation = asset.tail.word64(2).and_then(Navigation::read);
        if heights {
            let navigation = navigation.unwrap_or_else(|| panic!("no word at {}", frame.at));
            let lossless = &exss[header.header_size as usize..];
            assert_eq!(
                lossless[navigation.offset..navigation.offset + 4],
                [0x02, 0x00, 0x08, 0x50],
                "the word points at the block, at {}",
                frame.at
            );
        } else {
            assert_eq!(navigation, None);
        }
        let len = core.len() + exss.len();
        frames.push(&stream[frame.at..frame.at + len]);
    }
    frames
}

/// The `.mka`'s one track: `A_DTS`, stating what it carries, its blocks the
/// raw stream's frames one for one, the last stating its padding.
fn check_matroska(file: &[u8], frames: &[&[u8]], channels: u64, bits: u64) {
    let entry = read::track(file, 1).unwrap();
    let codec = read::only(&entry, id::CODEC_ID).unwrap();
    assert_eq!(&file[codec.at..codec.end], b"A_DTS");
    assert!(entry.iter().all(|e| e.id != id::CODEC_PRIVATE));
    assert!(entry.iter().all(|e| e.id != id::CODEC_DELAY));
    let duration = read::only(&entry, id::DEFAULT_DURATION).unwrap();
    assert_eq!(read::uint(file, duration), 10_666_666);
    let audio = read::only(&entry, id::AUDIO).unwrap();
    let audio = read::children(file, audio.at, audio.end).unwrap();
    let rate = read::only(&audio, id::SAMPLING_FREQUENCY).unwrap();
    assert_eq!(read::float(file, rate).unwrap(), RATE as f64);
    assert_eq!(
        read::uint(file, read::only(&audio, id::CHANNELS).unwrap()),
        channels
    );
    assert_eq!(
        read::uint(file, read::only(&audio, id::BIT_DEPTH).unwrap()),
        bits
    );

    let (_, parts) = read::segment(file).unwrap();
    let info = read::only(&parts, id::INFO).unwrap();
    let info = read::children(file, info.at, info.end).unwrap();
    let seconds = read::float(file, read::only(&info, id::DURATION).unwrap()).unwrap();
    assert_eq!(seconds, SAMPLES as f64 * 1000.0 / RATE as f64);

    let blocks = read::blocks(file, 1).unwrap();
    assert_eq!(blocks.len(), frames.len());
    let padding = frames.len() * FRAME - SAMPLES;
    for (n, (block, frame)) in blocks.iter().zip(frames).enumerate() {
        assert_eq!(block.frame, *frame, "block {n}");
        assert!(block.key);
        let last = n + 1 == frames.len();
        let expected = last.then(|| (padding * 1_000_000_000 / RATE) as i64);
        assert_eq!(block.discard_padding, expected, "block {n}");
    }
}

/// Each layout, at each depth it takes, encodes to both containers: the same
/// frames, one more than the whole frames of the input, every header
/// rewriting byte for byte, the 7.1.4's navigation found on every frame and
/// the beds' absent, and the Matroska track stating what a decoder of the
/// lossless layer presents.
#[test]
fn every_layout_writes_the_same_checked_frames_raw_and_in_matroska() {
    let dir = scratch("layouts");
    let mut bed: Vec<(&str, u32)> = BED_7_1.to_vec();
    bed.extend_from_slice(&HEIGHTS);
    let input = master(&dir, &bed, SAMPLES, tones);
    for (layout, bits, channels) in [
        ("7.1.4", "24", 8),
        ("7.1", "24", 8),
        ("7.1", "16", 8),
        ("5.1", "24", 6),
        ("5.1", "16", 6),
    ] {
        let raw = dir.join(format!("{layout}-{bits}.dts"));
        let mka = dir.join(format!("{layout}-{bits}.mka"));
        let flags = ["--layout", layout, "--bits", bits];
        let said = encode(&input, &raw, &flags);
        let summary = String::from_utf8_lossy(&said.stdout).into_owned();
        assert!(summary.contains("a raw elementary stream"), "{summary}");
        let said = encode(&input, &mka, &flags);
        let summary = String::from_utf8_lossy(&said.stdout).into_owned();
        assert!(summary.contains("Matroska (A_DTS)"), "{summary}");
        assert!(summary.contains(&format!("{bits}-bit")), "{summary}");

        let stream = std::fs::read(&raw).unwrap();
        let frames = check_frames(&stream, layout == "7.1.4");
        assert_eq!(frames.len(), SAMPLES.div_ceil(FRAME), "{layout}");
        let file = std::fs::read(&mka).unwrap();
        check_matroska(&file, &frames, channels, bits.parse().unwrap());
    }
}

/// The 7.1 and 7.1.4 renders route a bed of their own speakers one for
/// one, so FFmpeg's decode of the `.mka` is the master: the 7.1 bit for bit,
/// and for 7.1.4 the composite — each height folded into its corner at
/// −3 dB, in integers. Exactly the master's length: FFmpeg applies the
/// padding the last block states.
#[test]
fn ffmpeg_decodes_the_matroska_track_to_the_master_bit_for_bit() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("ffmpeg is not installed: the decode is not checked");
        return;
    }
    let dir = scratch("ffmpeg");
    for layout in ["7.1", "7.1.4"] {
        // A 7.1 has no speaker for a height, which it would pan.
        let mut bed: Vec<(&str, u32)> = BED_7_1.to_vec();
        if layout == "7.1.4" {
            bed.extend_from_slice(&HEIGHTS);
        }
        let set = dir.join(layout);
        std::fs::create_dir_all(&set).unwrap();
        let input = master(&set, &bed, SAMPLES, tones);
        let mka = dir.join(format!("{layout}.mka"));
        encode(&input, &mka, &["--layout", layout]);
        let decoded = Command::new("ffmpeg")
            .args(["-v", "error", "-err_detect", "crccheck", "-i"])
            .arg(&mka)
            .args(["-f", "s32le", "-"])
            .output()
            .unwrap();
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        let pcm: Vec<i32> = decoded
            .stdout
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(pcm.len(), SAMPLES * 8, "{layout}: the master's length");
        let mut differ = 0;
        for n in 0..SAMPLES {
            for ch in 0..8 {
                let mut expected = tones(ch, n);
                if layout == "7.1.4" {
                    // L, R, Lrs, Rrs take Lfh, Rfh, Lrh, Rrh.
                    if let Some(h) = [0, 1, 4, 5].iter().position(|&c| c == ch) {
                        expected += hz_dca::dtsx::fold(tones(8 + h, n), 23170);
                    }
                }
                differ += usize::from(pcm[n * 8 + ch] != expected << 8);
            }
        }
        assert_eq!(differ, 0, "{layout}: samples differing from the master");
    }
}

/// A 7.1.4 whose bed the navigation word cannot point past is refused at
/// the frame that cannot be navigated, by name, and nothing is cut to fit;
/// the Matroska file is closed around what came before, which is nothing.
#[test]
fn a_frame_the_navigation_word_cannot_reach_is_refused_by_name() {
    let dir = scratch("navigation");
    let mut bed: Vec<(&str, u32)> = BED_7_1.to_vec();
    bed.extend_from_slice(&HEIGHTS);
    // White noise at −6 dBFS in every channel: refused from −48 (docs/dts.md).
    let input = master(&dir, &bed, SAMPLES, |channel, n| {
        let mut x = (n as u64 * 12 + channel as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        x ^= x >> 29;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^= x >> 32;
        ((x as i64 % (1 << 22)) as i32).clamp(-(1 << 22), (1 << 22) - 1)
    });
    let mka = dir.join("noise.mka");
    let output = harlettizer(&[
        "dts",
        input.to_str().unwrap(),
        "--out",
        mka.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("refused"), "{said}");
    assert!(said.contains("frame 0 (from sample 0)"), "{said}");
    assert!(said.contains("not cut to fit"), "{said}");
    let file = std::fs::read(&mka).unwrap();
    assert!(read::blocks(&file, 1).unwrap().is_empty());
}

/// An output that is a file of the input is refused before anything is
/// written, the input left as it was.
#[test]
fn an_output_that_is_the_input_is_refused_untouched() {
    let dir = scratch("alias");
    let input = master(&dir, &BED_7_1, SAMPLES, tones);
    let audio = dir.join("m.atmos.audio");
    let before = std::fs::read(&audio).unwrap();
    for out in [&audio, &input] {
        let output = harlettizer(&[
            "dts",
            input.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]);
        assert!(!output.status.success());
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.contains("refused"), "{said}");
    }
    assert_eq!(std::fs::read(&audio).unwrap(), before);
}

/// What the profile cannot carry is refused before the encode starts.
#[test]
fn a_layout_or_depth_the_profile_cannot_carry_is_refused() {
    let dir = scratch("options");
    let input = master(&dir, &BED_7_1, FRAME, tones);
    let out = dir.join("x.dts");
    for (flags, says) in [
        (["--layout", "7.1.4", "--bits", "16"], "16 bits for 7.1.4"),
        (["--layout", "5.1", "--bits", "20"], "20 bits"),
        (["--layout", "2.0", "--bits", "24"], "`2.0` for the layout"),
    ] {
        let mut args = vec![
            "dts",
            input.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ];
        args.extend_from_slice(&flags);
        let output = harlettizer(&args);
        assert!(!output.status.success(), "{flags:?}");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.contains(says), "{said}");
    }
    assert!(!out.exists());
}

/// `--progress` reports on standard error as every other encode does, a
/// line each block, climbing to everything, never twice the same.
#[test]
fn progress_climbs_to_everything_on_standard_error() {
    let dir = scratch("progress");
    let input = master(&dir, &BED_7_1, SAMPLES, tones);
    let out = dir.join("p.mka");
    let output = encode(&input, &out, &["--layout", "7.1", "--progress"]);
    let said = String::from_utf8_lossy(&output.stderr);
    let percents: Vec<u32> = said
        .lines()
        .filter_map(|l| l.strip_prefix("progress ")?.strip_suffix('%')?.parse().ok())
        .collect();
    // Two blocks of eight frames: 4,096 samples of 5,320, then the rest.
    assert_eq!(percents, [76, 100], "{said}");
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(!summary.lines().any(|l| l.starts_with("progress ")));
}
