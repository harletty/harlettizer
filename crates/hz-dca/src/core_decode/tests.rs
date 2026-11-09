use super::*;
use crate::encoder::tests::fixture;
use crate::encoder::{Config, Encoder};

fn fnv1a(values: impl Iterator<Item = i32>) -> u64 {
    values
        .flat_map(i32::to_le_bytes)
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        })
}

/// Encode `pcm` and reconstruct each core frame as it is written: the
/// stream, and the reconstruction interleaved in WAV order.
fn encode_and_reconstruct(adpcm: bool, pcm: &[i32]) -> (Vec<u8>, Vec<i32>) {
    let mut encoder = Encoder::new(Config {
        adpcm,
        ..Config::reference_five_one()
    })
    .unwrap();
    let mut decoder = CoreDecoder::new();
    let mut stream = Vec::new();
    let mut out = Vec::new();
    for frame in pcm.chunks_exact(FRAME_SAMPLES * 6) {
        encoder.encode(frame, &mut stream);
        decoder.decode(&encoder);
        for n in 0..FRAME_SAMPLES {
            out.extend((0..6).map(|ch| decoder.channel(ch)[n]));
        }
    }
    (stream, out)
}

/// Check `ours` against FFmpeg's fixed-point decode of `stream`, sample for
/// sample, where FFmpeg is installed.
fn check_against_ffmpeg(stream: &[u8], ours: &[i32], what: &str) {
    let Some(theirs) = ffmpeg_fixed_point(stream) else {
        eprintln!("no ffmpeg: {what} not decoded");
        return;
    };
    assert_eq!(theirs.len(), ours.len(), "{what}");
    if let Some(at) = theirs.iter().zip(ours).position(|(a, b)| a != b) {
        panic!(
            "{what}: frame {}, channel {}, sample {}: FFmpeg {}, ours {}",
            at / 6 / FRAME_SAMPLES,
            at % 6,
            at / 6 % FRAME_SAMPLES,
            theirs[at],
            ours[at]
        );
    }
}

/// FFmpeg's fixed-point core decode — `ffmpeg -flags +bitexact -i core.dts
/// -f s32le -`, its output shifted down by eight — of the streams
/// [`fixture`] encodes to, hashed: the reconstruction is FFmpeg's to the
/// last bit, every channel, LFE included, with ADPCM and without.
#[test]
fn the_reconstruction_is_ffmpegs() {
    for (adpcm, want) in [
        (false, 0xdebc_6003_9487_969eu64),
        (true, 0xde3e_b4d5_b49a_b7de),
    ] {
        let (stream, pcm) = encode_and_reconstruct(adpcm, &fixture(12));
        check_against_ffmpeg(&stream, &pcm, &format!("adpcm {adpcm}"));
        assert_eq!(fnv1a(pcm.iter().copied()), want, "adpcm {adpcm}");
    }
}

/// Full-scale noise in every channel, then full-scale squares: where the
/// decoder's clips bite and its cosine modulation scales its input down.
#[test]
fn the_extremes_reconstruct_as_ffmpeg_does() {
    let mut state = 0x9e37_79b9u32;
    let mut pcm = Vec::with_capacity(8 * FRAME_SAMPLES * 6);
    for n in 0..8 * FRAME_SAMPLES {
        for ch in 0..6 {
            pcm.push(if n < 4 * FRAME_SAMPLES {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as i32 & !0xff
            } else if (n / (16 + 8 * ch)) % 2 == 0 {
                i32::MAX & !0xff
            } else {
                i32::MIN
            });
        }
    }
    for adpcm in [false, true] {
        let (stream, ours) = encode_and_reconstruct(adpcm, &pcm);
        check_against_ffmpeg(&stream, &ours, &format!("extremes, adpcm {adpcm}"));
    }
}

/// The stream decoded by FFmpeg's fixed-point core path, in its 24-bit
/// range; `None` where FFmpeg is not installed.
fn ffmpeg_fixed_point(stream: &[u8]) -> Option<Vec<i32>> {
    // One file per call: the tests run in parallel.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hz-core-decode-{}-{call}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("core.dts");
    std::fs::write(&path, stream).unwrap();
    let output = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-flags", "+bitexact", "-f", "dts", "-i"])
        .arg(&path)
        .args(["-f", "s32le", "-"])
        .output();
    std::fs::remove_dir_all(&dir).unwrap();
    let output = output.ok()?;
    assert!(output.status.success(), "FFmpeg refuses the stream");
    Some(
        output
            .stdout
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 8)
            .collect(),
    )
}

#[test]
fn silence_reconstructs_as_silence() {
    let mut encoder = Encoder::new(Config::reference_five_one()).unwrap();
    let mut decoder = CoreDecoder::new();
    let mut stream = Vec::new();
    for _ in 0..3 {
        encoder.encode(&[0; FRAME_SAMPLES * 6], &mut stream);
        decoder.decode(&encoder);
        for ch in 0..6 {
            assert!(decoder.channel(ch).iter().all(|&s| s == 0));
        }
    }
}

/// The reconstruction follows the input 512 samples later, as a decoder's
/// output does: a tone comes back close to itself, one frame on.
#[test]
fn the_reconstruction_is_a_frame_behind() {
    let (_, pcm) = encode_and_reconstruct(false, &fixture(6));
    let input = fixture(6);
    // Left, the 440 Hz tone: input and reconstruction in the 24-bit range.
    let (mut signal, mut error) = (0f64, 0f64);
    for n in FRAME_SAMPLES * 2..FRAME_SAMPLES * 6 {
        let x = f64::from(input[(n - FRAME_SAMPLES) * 6] >> 8);
        let y = f64::from(pcm[n * 6]);
        signal += x * x;
        error += (x - y) * (x - y);
    }
    let snr = 10.0 * (signal / error).log10();
    assert!(snr > 60.0, "{snr:.1} dB");
}

/// The synthesis filter as it was, on 64-bit integers: the window taken
/// in `f64` makes the same samples and the same overlap, block after block,
/// from silence to inputs that clip.
#[test]
fn the_window_in_f64_is_the_integer_window() {
    struct Before {
        buffer: [i32; 512],
        offset: usize,
        overlap: [i32; 32],
    }
    impl Before {
        fn filter(&mut self, input: &[i32; 32], out: &mut [i32; 32]) {
            let window = &FIR_32BANDS_NONPERFECT_FIXED;
            let at = self.offset;
            imdct_half_32(
                (&mut self.buffer[at..at + 32])
                    .try_into()
                    .expect("32 samples"),
                input,
            );
            let buf = &self.buffer;
            for i in 0..16 {
                let mut a = i64::from(self.overlap[i]) << 21;
                let mut b = i64::from(self.overlap[i + 16]) << 21;
                let mut c = 0i64;
                let mut d = 0i64;
                let mut tap = |j: usize, base: usize| {
                    a += i64::from(window[i + j]) * i64::from(buf[base + i]);
                    b += i64::from(window[i + j + 16]) * i64::from(buf[base + 15 - i]);
                    c += i64::from(window[i + j + 32]) * i64::from(buf[base + 16 + i]);
                    d += i64::from(window[i + j + 48]) * i64::from(buf[base + 31 - i]);
                };
                let mut j = 0;
                while j < 512 - at {
                    tap(j, at + j);
                    j += 64;
                }
                while j < 512 {
                    tap(j, at + j - 512);
                    j += 64;
                }
                out[i] = clip23(norm(a, 21));
                out[i + 16] = clip23(norm(b, 21));
                self.overlap[i] = norm(c, 21);
                self.overlap[i + 16] = norm(d, 21);
            }
            self.offset = (at + 512 - 32) & 511;
        }
    }
    let mut before = Before {
        buffer: [0; 512],
        offset: 0,
        overlap: [0; 32],
    };
    let mut now = CoreDecoder::new().synthesis[0].clone();
    let mut seed = 0x2545_f491u32;
    for block in 0..6000 {
        let mut input = [0i32; 32];
        for x in &mut input {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = seed as i32;
            *x = match block / 500 % 6 {
                0 => 0,
                1 => clip23(noise >> 8),
                2 => noise >> (seed >> 27),
                3 => noise,
                4 => {
                    if block % 2 == 0 {
                        (1 << 23) - 1
                    } else {
                        -(1 << 23)
                    }
                }
                _ => clip23(noise >> 12),
            };
        }
        let (mut ours, mut theirs) = ([0; 32], [0; 32]);
        now.filter(&input, &mut ours);
        before.filter(&input, &mut theirs);
        assert_eq!(ours, theirs, "block {block}");
        assert_eq!(now.overlap, before.overlap, "block {block}");
        assert_eq!(now.offset, before.offset);
    }
}
