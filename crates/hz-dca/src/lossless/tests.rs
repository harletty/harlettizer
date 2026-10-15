use super::*;
use crate::xll::{Headers, unzigzag};
use hz_core::bits_read::BitReader;

/// Samples of every speaker of a frame, by DTS speaker mask bit.
type Decoded = Vec<(u32, Vec<i32>)>;

/// The frame decoded as FFmpeg's `dca_xll.c` decodes it, every channel set
/// active: the test's own reader, so that a round trip is checked where the
/// oracles are not installed. Band 0 only, no LSB section; anything else
/// panics.
fn decode(frame: &[u8]) -> Decoded {
    let headers = Headers::read(frame, true).unwrap();
    assert!(headers.checksums(frame).all(), "a checksum fails");
    let common = &headers.common;
    assert_eq!(common.frame_size as usize, frame.len());
    assert!(frame.len().is_multiple_of(4), "frames end on a 32-bit word");
    let segment_samples = common.segment_samples();
    let frame_samples = common.frame_samples();
    let sets = &headers.channel_sets;
    let mut buffers: Vec<Vec<Vec<i32>>> = sets
        .iter()
        .map(|s| vec![vec![0i32; frame_samples]; usize::from(s.channels)])
        .collect();
    let mut state: Vec<Parameters> = vec![Parameters::default(); sets.len()];
    let mut at = headers.len() * 8;
    let mut sizes = headers.navigation.sizes.iter();
    for segment in 0..common.segments() {
        for (index, set) in sets.iter().enumerate() {
            let end = at + *sizes.next().unwrap() as usize * 8;
            let mut r = BitReader::at_bit(frame, at);
            let mut nabits = if set.storage_bit_res > 16 { 5 } else { 4 };
            if sets.len() > 1 && nabits < 5 {
                nabits += 1;
            }
            read_segment(
                &mut r,
                set,
                segment,
                segment_samples,
                nabits,
                &mut state[index],
                &mut buffers[index],
            );
            assert!(r.bit_position() <= end, "a segment reads past its size");
            assert!(end - r.bit_position() < 8, "a segment ends early");
            at = end;
        }
    }
    assert_eq!(at, headers.len() * 8 + headers.band_data_len() * 8);
    assert!(
        frame[at / 8..].iter().all(|&b| b == 0),
        "the frame's tail is padding"
    );
    // Inverse prediction, then pairs, then the original order.
    for (set, channels) in sets.iter().zip(&mut buffers) {
        let band = &set.bands[0];
        for (ch, buf) in channels.iter_mut().enumerate() {
            let order = usize::from(band.adaptive_orders[ch]);
            if order > 0 {
                let mut coeff = [0i32; MAX_ORDER];
                for j in 0..order {
                    let k = band.reflection[ch][j];
                    let magnitude = i32::from(REFLECTION_COEFFS[usize::from(k.unsigned_abs())]);
                    let rc = if k < 0 { -magnitude } else { magnitude };
                    for m in 0..j.div_ceil(2) {
                        let (t1, t2) = (coeff[m], coeff[j - m - 1]);
                        coeff[m] = t1 + mul(rc, t2, 16);
                        coeff[j - m - 1] = t2 + mul(rc, t1, 16);
                    }
                    coeff[j] = rc;
                }
                for j in 0..frame_samples - order {
                    let mut err = 0i64;
                    for k in 0..order {
                        err += i64::from(buf[j + k]) * i64::from(coeff[order - k - 1]);
                    }
                    let prediction = ((err + (1 << 15)) >> 16) as i32;
                    buf[j + order] = buf[j + order].wrapping_sub(clip23(prediction));
                }
            } else {
                for _ in 0..band.fixed_orders[ch] {
                    for k in 1..frame_samples {
                        buf[k] = buf[k].wrapping_add(buf[k - 1]);
                    }
                }
            }
        }
        if let Some(d) = &band.decorrelation {
            for (pair, c) in d.coefficients.iter().enumerate() {
                if let Some(c) = *c {
                    let (first, second) = channels.split_at_mut(2 * pair + 1);
                    for (v, &u) in second[0].iter_mut().zip(&first[2 * pair]) {
                        *v = v.wrapping_add(u.wrapping_mul(i32::from(c)).wrapping_add(4) >> 3);
                    }
                }
            }
            let coded = channels.clone();
            for (i, &original) in d.order.iter().enumerate() {
                channels[usize::from(original)] = coded[i].clone();
            }
        }
    }
    // Undo the embedded downmixes.
    for o in 1..sets.len() {
        let Mapping::Speakers(Speakers {
            primary: false,
            downmix:
                Some(Downmix {
                    embedded: true,
                    codes,
                    ..
                }),
            hierarchical: true,
            ..
        }) = &sets[o].mapping
        else {
            continue;
        };
        let rears = buffers[o].clone();
        let mut row = 0;
        for below in &mut buffers[..o] {
            for channel in below.iter_mut() {
                let codes = &codes[row * (rears.len() + 1)..(row + 1) * (rears.len() + 1)];
                let (_, inverse) = xll::scale(codes[0]).unwrap();
                for (k, rear) in rears.iter().enumerate() {
                    let c_ = xll::coefficient(codes[1 + k]).unwrap();
                    let m = mul(inverse.abs(), c_.abs(), 16);
                    let undo = if c_ < 0 { -m } else { m };
                    if undo != 0 {
                        for (x, &y) in channel.iter_mut().zip(rear) {
                            *x = x.wrapping_sub(mul(y, undo, 15));
                        }
                    }
                }
                row += 1;
            }
        }
    }
    let mut out = Decoded::new();
    for (set, channels) in sets.iter().zip(buffers) {
        let mut mask = set.mask().unwrap();
        for samples in channels {
            let bit = mask.trailing_zeros();
            mask &= mask - 1;
            out.push((bit, samples));
        }
    }
    out.sort_by_key(|(bit, _)| *bit);
    out
}

fn read_segment(
    r: &mut BitReader,
    set: &ChannelSet,
    segment: usize,
    segment_samples: usize,
    nabits: u32,
    state: &mut Parameters,
    channels: &mut [Vec<i32>],
) {
    let band = &set.bands[0];
    let n = usize::from(set.channels);
    if !(segment > 0 && r.get_bit().unwrap()) {
        state.common = r.get_bit().unwrap();
        state.count = if state.common { 1 } else { n };
        for i in 0..state.count {
            state.sets[i].rice = r.get_bit().unwrap();
            if !state.common && state.sets[i].rice {
                assert!(!r.get_bit().unwrap(), "no hybrid Rice codes here");
            }
        }
        for i in 0..state.count {
            let p = &mut state.sets[i];
            let adjust = |k: u32, rice: bool| if !rice && k > 0 { k + 1 } else { k };
            if segment == 0 {
                p.part_a = adjust(r.get(nabits).unwrap(), p.rice);
                p.part_a_samples = if state.common {
                    usize::from(*band.adaptive_orders.iter().max().unwrap())
                } else {
                    usize::from(band.adaptive_orders[i])
                };
            } else {
                p.part_a = 0;
                p.part_a_samples = 0;
            }
            p.part_b = adjust(r.get(nabits).unwrap(), p.rice);
        }
    }
    for (ch, channel) in channels.iter_mut().enumerate() {
        let p = *state.of(ch);
        let run = &mut channel[segment * segment_samples..(segment + 1) * segment_samples];
        let (a, b) = run.split_at_mut(p.part_a_samples);
        for (part, k) in [(a, p.part_a), (b, p.part_b)] {
            for v in part.iter_mut() {
                *v = if p.rice {
                    let mut q = 0u32;
                    while !r.get_bit().unwrap() {
                        q += 1;
                    }
                    unzigzag(q << k | r.get(k).unwrap())
                } else if k == 0 {
                    0
                } else {
                    unzigzag(r.get(k).unwrap())
                };
            }
        }
    }
}

/// A deterministic test signal: per channel a different character, so that
/// pairing, prediction and coding all have something to choose between.
fn signal(layout: Layout, bits: u8, frames: usize) -> Vec<i32> {
    let channels = layout.channels();
    let mut state = 0x2545_f491u32;
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as i32
    };
    let full = f64::from(1u32 << (bits - 1)) - 1.0;
    let mut pcm = Vec::with_capacity(frames * FRAME_SAMPLES * channels);
    for n in 0..frames * FRAME_SAMPLES {
        let t = n as f64 / 48000.0;
        for ch in 0..channels {
            let v = match ch {
                // A tone and its correlated twin.
                0 => 0.6 * (std::f64::consts::TAU * 440.0 * t).sin(),
                1 => 0.5 * (std::f64::consts::TAU * 440.0 * t + 0.3).sin(),
                // Full-scale square.
                2 => {
                    if (n / 50) % 2 == 0 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                // Silence.
                3 => 0.0,
                // White noise at full scale.
                4 => f64::from(noise()) / f64::from(i32::MAX),
                // A chirp.
                5 => 0.9 * (std::f64::consts::TAU * (100.0 + 4000.0 * t) * t).sin(),
                // Low noise, then a burst.
                6 => {
                    f64::from(noise() >> 12) / f64::from(i32::MAX)
                        * if n > 700 { 4000.0 } else { 1.0 }
                }
                _ => -0.99,
            };
            let v = (v * full).round().clamp(-full - 1.0, full) as i32;
            pcm.push(v << (32 - bits));
        }
    }
    pcm
}

/// Speaker bit of each WAV input channel.
fn speakers(layout: Layout) -> Vec<u32> {
    match layout {
        // L R C LFE Ls Rs.
        Layout::FivePointOne => vec![1, 2, 0, 5, 3, 4],
        // L R C LFE Lsr Rsr Lss Rss.
        Layout::SevenPointOne => vec![1, 2, 0, 5, 7, 8, 9, 10],
    }
}

fn round_trip(config: Config, pcm: &[i32]) -> usize {
    let layout = config.layout;
    let bits = config.bits;
    let channels = layout.channels();
    let mut encoder = Encoder::new(config).unwrap();
    let mut bytes = 0;
    for frame in pcm.chunks(FRAME_SAMPLES * channels) {
        let coded = encoder.encode(frame).unwrap().to_vec();
        bytes += coded.len();
        let headers = Headers::read(&coded, true).unwrap();
        assert_eq!(headers.write(true).unwrap(), coded[..headers.len()]);
        let decoded = decode(&coded);
        for (input, &bit) in speakers(layout).iter().enumerate() {
            let (_, samples) = decoded.iter().find(|(b, _)| *b == bit).unwrap();
            for (n, &s) in samples.iter().enumerate() {
                assert_eq!(
                    s,
                    frame[n * channels + input] >> (32 - bits),
                    "speaker {bit}, sample {n}"
                );
            }
        }
    }
    bytes
}

#[test]
fn seven_one_comes_back_bit_for_bit() {
    for bits in [16, 24] {
        let pcm = signal(Layout::SevenPointOne, bits, 4);
        round_trip(Config::reference(Layout::SevenPointOne, bits), &pcm);
    }
}

#[test]
fn five_one_comes_back_bit_for_bit() {
    for bits in [16, 24] {
        let pcm = signal(Layout::FivePointOne, bits, 4);
        round_trip(Config::reference(Layout::FivePointOne, bits), &pcm);
    }
}

#[test]
fn every_segmentation_and_split_flag_comes_back() {
    for segments_log2 in 1..=4 {
        for scalable_lsbs in [false, true] {
            let config = Config {
                segments_log2,
                scalable_lsbs,
                ..Config::reference(Layout::SevenPointOne, 24)
            };
            let pcm = signal(Layout::SevenPointOne, 24, 2);
            round_trip(config, &pcm);
        }
    }
}

#[test]
fn silence_and_full_scale_noise_come_back() {
    for bits in [16u8, 24] {
        let channels = 8;
        let silent = vec![0i32; FRAME_SAMPLES * channels * 2];
        round_trip(Config::reference(Layout::SevenPointOne, bits), &silent);
        let mut state = 0x1234_5678u32;
        let noise: Vec<i32> = (0..FRAME_SAMPLES * channels * 2)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as i32) >> (32 - bits) << (32 - bits)
            })
            .collect();
        let bytes = round_trip(Config::reference(Layout::SevenPointOne, bits), &noise);
        // Noise does not compress: about the PCM itself, and no worse than a
        // few percent over it.
        let pcm = FRAME_SAMPLES * channels * 2 * usize::from(bits) / 8;
        assert!(bytes < pcm * 107 / 100, "{bytes} bytes for {pcm} of PCM");
    }
}

#[test]
fn the_core_gets_the_fold_a_first_set_decoder_presents() {
    let pcm = signal(Layout::SevenPointOne, 24, 1);
    let mut encoder = Encoder::new(Config::reference(Layout::SevenPointOne, 24)).unwrap();
    encoder.encode(&pcm).unwrap();
    let core = encoder.core_pcm();
    // −3 dB of every channel, and of each side plus its rear.
    let q = 23170;
    for n in 0..FRAME_SAMPLES {
        let x = |ch: usize| pcm[n * 8 + ch] >> 8;
        let fold = |side: usize, rear: usize| {
            let c = mul(92682, 23170, 16);
            mul(x(side).wrapping_add(mul(x(rear), c, 15)), q, 15)
        };
        let expect = [
            mul(x(0), q, 15),
            mul(x(1), q, 15),
            mul(x(2), q, 15),
            mul(x(3), q, 15),
            fold(6, 4),
            fold(7, 5),
        ];
        for (slot, &e) in expect.iter().enumerate() {
            assert_eq!(core[n * 6 + slot], e.clamp(-(1 << 23), (1 << 23) - 1) << 8);
        }
    }
}

#[test]
fn a_segment_count_outside_the_format_is_refused_not_shifted() {
    for segments_log2 in [0, 9, 62, 63, 64, 255] {
        let config = Config {
            segments_log2,
            ..Config::reference(Layout::FivePointOne, 16)
        };
        assert!(Encoder::new(config).is_err(), "{segments_log2}");
    }
    for segments_log2 in 1..=8 {
        let config = Config {
            segments_log2,
            ..Config::reference(Layout::FivePointOne, 16)
        };
        assert!(Encoder::new(config).is_ok(), "{segments_log2}");
    }
}

#[test]
fn bits_below_the_resolution_are_refused() {
    let mut pcm = vec![0i32; FRAME_SAMPLES * 6];
    pcm[6 * 10 + 4] = 0x100 | 0x80;
    let mut encoder = Encoder::new(Config::reference(Layout::FivePointOne, 24)).unwrap();
    assert_eq!(
        encoder.encode(&pcm).unwrap_err(),
        Refused::Resolution {
            channel: 4,
            sample: 10,
            value: 0x180,
            bits: 24
        }
    );
    pcm[6 * 10 + 4] = 0x1_0000;
    let mut encoder = Encoder::new(Config::reference(Layout::FivePointOne, 16)).unwrap();
    assert!(encoder.encode(&pcm).is_ok());
    pcm[6 * 10 + 4] = 0x8000;
    assert!(encoder.encode(&pcm).is_err());
}

#[test]
fn rice_costs_are_what_is_written() {
    let mut state = 7u32;
    let codes: Vec<u32> = (0..300)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 16) % 5000
        })
        .collect();
    for k in 0..16 {
        let mut w = BitWriter::new();
        for &u in &codes {
            put_rice(&mut w, u, k);
        }
        assert_eq!(w.bit_position() as u64, rice_bits(&codes, k));
    }
    let (k, bits) = best_rice(&codes, 31);
    for other in 0..31 {
        assert!(rice_bits(&codes, other) >= bits, "{other} beats {k}");
    }
}
