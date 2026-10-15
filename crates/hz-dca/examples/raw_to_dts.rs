//! Encode raw interleaved 32-bit PCM to a DTS core stream.
//!
//! ```text
//! cargo run --release -p hz-dca --example raw_to_dts -- in.s32 out.dts CHANNELS RATE BITRATE
//! ```
//!
//! Signed 32-bit little-endian samples in WAV order — L, R, C, LFE, Ls, Rs
//! for 5.1 — what `ffmpeg -f s32le` writes; 1, 2, 4, 5 or 6 channels. The
//! last frame is padded with silence, as FFmpeg pads it. `ADPCM=1` turns on
//! subband ADPCM; `FFMPEG=1` reproduces FFmpeg's LFE history, so that the
//! stream can be compared with `ffmpeg -c:a dca` byte for byte. A
//! development aid, not the command line's encoder.

use hz_dca::encoder::{Config, Encoder, FRAME_SAMPLES, Layout};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, channels, rate, bit_rate] = args.as_slice() else {
        eprintln!("usage: raw_to_dts in.s32 out.dts CHANNELS RATE BITRATE");
        std::process::exit(2);
    };
    let layout = match channels.parse::<usize>().unwrap() {
        1 => Layout::Mono,
        2 => Layout::Stereo,
        4 => Layout::TwoTwo,
        5 => Layout::FivePointZero,
        6 => Layout::FivePointOne,
        n => panic!("{n} channels: 1, 2, 4, 5 or 6"),
    };
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
    let mut encoder = Encoder::new(Config {
        layout,
        sample_rate: rate.parse().unwrap(),
        bit_rate: bit_rate.parse().unwrap(),
        adpcm: flag("ADPCM"),
        pcm_resolution_code: 0,
        lfe_history_quirk: flag("FFMPEG"),
    })
    .unwrap();

    let bytes = std::fs::read(input).unwrap();
    let samples: Vec<i32> = bytes
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let per_frame = FRAME_SAMPLES * layout.channels();
    let frames = samples.len().div_ceil(per_frame);
    let mut frame = vec![0i32; per_frame];
    let mut out = Vec::with_capacity(frames * encoder.frame_size());
    for f in 0..frames {
        let chunk = &samples[f * per_frame..samples.len().min((f + 1) * per_frame)];
        frame[..chunk.len()].copy_from_slice(chunk);
        frame[chunk.len()..].fill(0);
        encoder.encode(&frame, &mut out);
    }
    std::fs::write(output, out).unwrap();
}
