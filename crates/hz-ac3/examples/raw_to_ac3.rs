//! Encode raw interleaved 32-bit float PCM to an AC-3 elementary stream.
//!
//! ```text
//! cargo run --release -p hz-ac3 --example raw_to_ac3 -- in.f32 out.ac3 RATE CHANNELS KBPS
//! ```
//!
//! Channels in WAV order; 1 to 6 of them, the sixth being LFE for 5.1. A
//! development aid for comparing against other encoders, not the command
//! line's encoder.

use hz_ac3::{ChannelMode, Config, Encoder, FRAME_SAMPLES, Metadata};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output, rate, channels, kbps] = args.as_slice() else {
        eprintln!("usage: raw_to_ac3 in.f32 out.ac3 RATE CHANNELS KBPS");
        std::process::exit(2);
    };
    let channels: usize = channels.parse().unwrap();
    let (mode, lfe) = match channels {
        1 => (ChannelMode::Mono, false),
        2 => (ChannelMode::Stereo, false),
        3 => (ChannelMode::ThreeFront, false),
        4 => (ChannelMode::TwoTwo, false),
        5 => (ChannelMode::ThreeTwo, false),
        6 => (ChannelMode::ThreeTwo, true),
        _ => panic!("1 to 6 channels"),
    };
    let mut encoder = Encoder::new(Config {
        sample_rate: rate.parse().unwrap(),
        mode,
        lfe,
        bit_rate: Some(kbps.parse().unwrap()),
        metadata: Metadata::default(),
    })
    .unwrap();

    let bytes = std::fs::read(input).unwrap();
    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let frames = samples.len() / channels / FRAME_SAMPLES;
    let mut planar = vec![vec![0f32; FRAME_SAMPLES]; channels];
    let mut out = Vec::new();
    for f in 0..=frames {
        for (ch, plane) in planar.iter_mut().enumerate() {
            for (n, v) in plane.iter_mut().enumerate() {
                let i = (f * FRAME_SAMPLES + n) * channels + ch;
                *v = samples.get(i).copied().unwrap_or(0.0);
            }
        }
        let refs: Vec<&[f32]> = planar.iter().map(Vec::as_slice).collect();
        encoder.encode(&refs, &mut out).unwrap();
    }
    std::fs::write(output, out).unwrap();
}
