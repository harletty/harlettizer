use super::*;
use crate::xll::{Headers, unzigzag};
use crate::{dtsx, navigation};
use hz_core::bits_read::BitReader;

/// Samples of every speaker of a frame, by DTS speaker mask bit.
type Decoded = Vec<(u32, Vec<i32>)>;

/// The frame decoded as FFmpeg's `dca_xll.c` decodes it, every channel set
/// active, residual channels added to `core`: the test's own reader, so that
/// a round trip is checked where the oracles are not installed. Band 0 only,
/// no LSB section; anything else panics.
fn decode(frame: &[u8], core: Option<&CoreDecoder>) -> Decoded {
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
    // The DTS:X extension block, when the frame has one: at the next 32-bit
    // word, to the frame's end.
    let bed_end = (at / 8).next_multiple_of(4);
    assert!(
        frame[at / 8..bed_end].iter().all(|&b| b == 0),
        "the bed is padded to the word"
    );
    let heights = (bed_end < frame.len()).then(|| decode_heights(&frame[bed_end..], common));
    assert!(
        heights.is_some() || frame[at / 8..].iter().all(|&b| b == 0),
        "the frame's tail is padding"
    );
    for (set, channels) in sets.iter().zip(&mut buffers) {
        reconstruct(set, channels, frame_samples);
    }
    // Add the core to the residual channels (`combine_residual_frame`), the
    // core brought back up from the scale of the next downmix below it.
    for (index, (set, channels)) in sets.iter().zip(&mut buffers).enumerate() {
        let full = (1u32 << set.channels) - 1;
        if u32::from(set.residual_encode) == full {
            continue;
        }
        let core = core.expect("residual channels need the core");
        let below = sets[index + 1..].iter().find_map(|o| match &o.mapping {
            Mapping::Speakers(Speakers {
                primary: false,
                downmix:
                    Some(Downmix {
                        embedded: true,
                        codes,
                        ..
                    }),
                hierarchical: true,
                ..
            }) => Some((usize::from(o.channels), codes)),
            _ => None,
        });
        let shift = 24 - u32::from(set.pcm_bit_res);
        let round = if shift > 0 { 1 << (shift - 1) } else { 0 };
        let mut mask = set.mask().unwrap();
        for (ch, samples) in channels.iter_mut().enumerate() {
            let bit = mask.trailing_zeros();
            mask &= mask - 1;
            if u32::from(set.residual_encode) & (1 << ch) != 0 {
                continue;
            }
            let src = core.channel(core_slot(bit));
            for (x, &s) in samples.iter_mut().zip(src) {
                let add = match below {
                    Some((n, codes)) => {
                        let (_, inverse) = xll::scale(codes[ch * (n + 1)]).unwrap();
                        clip23((mul(s, inverse, 16) + round) >> shift)
                    }
                    None => (s + round) >> shift,
                };
                *x = x.wrapping_add(add);
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
    // The heights, as speakers past any mask's: TFL, TFR, TBL, TBR.
    for (k, samples) in heights.into_iter().flatten().enumerate() {
        out.push((HEIGHT_BIT + k as u32, samples));
    }
    out.sort_by_key(|(bit, _)| *bit);
    out
}

/// Where [`decode`] puts the heights among the speaker bits.
const HEIGHT_BIT: u32 = 100;

/// The DTS:X extension block at the start of `block`, which runs to the
/// lossless frame's end, decoded as `dca` decodes it: its matrix and the
/// height set's sub-header and table read and checked, the set's data read
/// in the frame's segments, the trailer and the padding zero.
fn decode_heights(block: &[u8], common: &CommonHeader) -> Vec<Vec<i32>> {
    assert_eq!(
        u32::from_be_bytes(block[..4].try_into().unwrap()),
        dtsx::SYNC
    );
    let headers = dtsx::Block::read(block, common).unwrap();
    assert!(headers.checksums(block, common), "a block checksum fails");
    assert_eq!(headers.write(common).unwrap(), block[..headers.len(common)]);
    assert_eq!(headers.matrix, {
        let mut m = FoldMatrix::standard();
        m.fit();
        FoldMatrix {
            crc: headers.matrix.crc,
            ..m
        }
    });
    let set = &headers.heights.set;
    assert_eq!((set.channels, set.residual_encode), (4, 0xf));
    let samples = common.frame_samples();
    let mut channels = vec![vec![0i32; samples]; 4];
    let mut state = Parameters::default();
    let mut at = headers.len(common) * 8;
    for (segment, &size) in headers.heights.navigation.sizes.iter().enumerate() {
        let end = at + size as usize * 8;
        let mut r = BitReader::at_bit(block, at);
        read_segment(
            &mut r,
            set,
            segment,
            common.segment_samples(),
            5,
            &mut state,
            &mut channels,
        );
        assert!(end - r.bit_position() < 8, "a height segment ends early");
        at = end;
    }
    assert_eq!(
        block.len(),
        headers.size(common, headers.heights.data_len()),
        "the block is its headers, its data, the trailer and the padding"
    );
    assert!(
        block[at / 8..].iter().all(|&b| b == 0),
        "the trailer and the padding are zeroes"
    );
    reconstruct(set, &mut channels, samples);
    channels
}

/// Undo a set's prediction, then its pairs, then put its channels back in
/// their original order.
fn reconstruct(set: &ChannelSet, channels: &mut [Vec<i32>], frame_samples: usize) {
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
        let coded = channels.to_vec();
        for (i, &original) in d.order.iter().enumerate() {
            channels[usize::from(original)] = coded[i].clone();
        }
    }
}

/// The core channel, in WAV 5.1 order, a speaker bit is coded against
/// (`ff_dca_core_map_spkr`): itself, or for a side surround the core's
/// surround.
fn core_slot(bit: u32) -> usize {
    match bit {
        0 => 2,
        1 => 0,
        2 => 1,
        3 | 9 => 4,
        4 | 10 => 5,
        5 => 3,
        _ => panic!("speaker {bit} is not in the core"),
    }
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
    // A 7.1.4's heights repeat the 7.1's first characters, and everything
    // is at half scale, so that a corner with its height folded in stays
    // inside 24 bits.
    let level = if layout == Layout::SevenPointOneFour {
        0.5
    } else {
        1.0
    };
    for n in 0..frames * FRAME_SAMPLES {
        let t = n as f64 / 48000.0;
        for ch in 0..channels {
            let v = level
                * match ch % 8 {
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
        // And the heights.
        Layout::SevenPointOneFour => vec![1, 2, 0, 5, 7, 8, 9, 10, 100, 101, 102, 103],
    }
}

/// What a decoder presents of input channel `input` at sample `n` of
/// `frame`: the channel, or for a corner of a 7.1.4 the composite its height
/// is folded into.
fn presented(layout: Layout, frame: &[i32], n: usize, input: usize, bits: u8) -> i32 {
    let channels = layout.channels();
    let x = frame[n * channels + input] >> (32 - bits);
    if layout != Layout::SevenPointOneFour {
        return x;
    }
    // L, R, Lsr and Rsr take TFL, TFR, TBL and TBR.
    match [0, 1, 4, 5].iter().position(|&c| c == input) {
        Some(h) => x + dtsx::fold(frame[n * channels + 8 + h] >> 8, 23170),
        None => x,
    }
}

fn round_trip(config: Config, pcm: &[i32]) -> usize {
    round_trip_with(config, pcm, false).0
}

/// Encode `pcm` and decode every frame with the test's reader, checking it
/// against the input: with `against_core`, as `master` pairs the layers, the
/// core a frame ahead and its reconstruction given to the lossless layer —
/// but not for the first frame of a 7.1, as there. The bytes of the
/// lossless frames, and the channels coded against the core.
fn round_trip_with(config: Config, pcm: &[i32], against_core: bool) -> (usize, u32) {
    let layout = config.layout;
    let bits = config.bits;
    let channels = layout.channels();
    let mut encoder = Encoder::new(config).unwrap();
    let mut core =
        crate::encoder::Encoder::new(crate::encoder::Config::reference_five_one()).unwrap();
    let mut reconstruction = CoreDecoder::new();
    let frames: Vec<&[i32]> = pcm.chunks(FRAME_SAMPLES * channels).collect();
    let silence = vec![0i32; FRAME_SAMPLES * channels];
    let mut core_frame = Vec::new();
    if against_core {
        core.encode(encoder.core_input(frames[0]), &mut core_frame);
        core.restart_prediction();
    }
    let (mut bytes, mut residual) = (0, 0);
    for (k, frame) in frames.iter().enumerate() {
        let mut given = None;
        if against_core {
            let next = frames.get(k + 1).copied().unwrap_or(&silence);
            core_frame.clear();
            core.encode(encoder.core_input(next), &mut core_frame);
            reconstruction.decode(&core);
            if k > 0 || layout == Layout::FivePointOne {
                given = Some(&reconstruction);
            }
        }
        let coded = encoder.encode(frame, given).unwrap().to_vec();
        residual += encoder.against_core();
        bytes += coded.len();
        let headers = Headers::read(&coded, true).unwrap();
        assert_eq!(headers.write(true).unwrap(), coded[..headers.len()]);
        let decoded = decode(&coded, given);
        for (input, &bit) in speakers(layout).iter().enumerate() {
            let (_, samples) = decoded.iter().find(|(b, _)| *b == bit).unwrap();
            for (n, &s) in samples.iter().enumerate() {
                assert_eq!(
                    s,
                    presented(layout, frame, n, input, bits),
                    "frame {k}, speaker {bit}, sample {n}"
                );
            }
        }
    }
    (bytes, residual)
}

/// Coded against the core, both layouts at both resolutions come back bit
/// for bit, most channels are coded against it, and the layer is smaller
/// than coded alone.
#[test]
fn coded_against_the_core_it_comes_back_bit_for_bit() {
    for layout in [Layout::FivePointOne, Layout::SevenPointOne] {
        for bits in [16, 24] {
            let pcm = signal(layout, bits, 6);
            let (alone, _) = round_trip_with(Config::reference(layout, bits), &pcm, false);
            let (against, residual) = round_trip_with(Config::reference(layout, bits), &pcm, true);
            assert!(
                residual > 0,
                "{layout:?} at {bits} bits: nothing against the core"
            );
            assert!(
                against < alone,
                "{layout:?} at {bits} bits: {against} bytes against the core, {alone} alone"
            );
        }
    }
}

/// With the residual off, the core's reconstruction is not used.
#[test]
fn the_residual_off_codes_every_channel_as_itself() {
    let pcm = signal(Layout::SevenPointOne, 24, 3);
    let config = Config {
        residual: false,
        ..Config::reference(Layout::SevenPointOne, 24)
    };
    assert_eq!(round_trip_with(config, &pcm, true).1, 0);
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
    let core = encoder.core_input(&pcm).to_vec();
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
        encoder.encode(&pcm, None).unwrap_err(),
        Refused::Resolution {
            channel: 4,
            sample: 10,
            value: 0x180,
            bits: 24
        }
    );
    pcm[6 * 10 + 4] = 0x1_0000;
    let mut encoder = Encoder::new(Config::reference(Layout::FivePointOne, 16)).unwrap();
    assert!(encoder.encode(&pcm, None).is_ok());
    pcm[6 * 10 + 4] = 0x8000;
    assert!(encoder.encode(&pcm, None).is_err());
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

/// The standard DTS:X profile: the bed decodes to the composite, the
/// heights to themselves, every frame — coded alone and against the core.
#[test]
fn seven_one_four_comes_back_bit_for_bit() {
    let pcm = signal(Layout::SevenPointOneFour, 24, 6);
    let config = || Config::reference(Layout::SevenPointOneFour, 24);
    let (alone, _) = round_trip_with(config(), &pcm, false);
    let (against, residual) = round_trip_with(config(), &pcm, true);
    assert!(residual > 0 && against < alone, "{against} against {alone}");
    // Silence: the smallest block there is.
    round_trip(config(), &vec![0; FRAME_SAMPLES * 12 * 2]);
}

/// Only 24 bits: a height's share of a corner does not fit fewer.
#[test]
fn seven_one_four_is_24_bits_only() {
    assert!(Encoder::new(Config::reference(Layout::SevenPointOneFour, 16)).is_err());
}

/// The block is located by the word: its offset is the bed's size rounded
/// to the word, its size the rest of the frame.
#[test]
fn the_block_ends_the_frame_where_the_word_points() {
    let pcm = signal(Layout::SevenPointOneFour, 24, 2);
    let mut encoder = Encoder::new(Config::reference(Layout::SevenPointOneFour, 24)).unwrap();
    for frame in pcm.chunks(FRAME_SAMPLES * 12) {
        let coded = encoder.encode(frame, None).unwrap().to_vec();
        let nav = encoder.extension().unwrap();
        assert_eq!(nav.offset + nav.size, coded.len());
        assert_eq!(
            u32::from_be_bytes(coded[nav.offset..nav.offset + 4].try_into().unwrap()),
            dtsx::SYNC
        );
        let headers = Headers::read(&coded, true).unwrap();
        assert_eq!(
            nav.offset,
            (headers.len() + headers.band_data_len()).next_multiple_of(4)
        );
        assert!(nav.word().is_ok());
    }
    assert_eq!(encoder.frames(), 2);
}

/// A corner its height takes past 24 bits is refused, named, before
/// anything is coded.
#[test]
fn a_fold_past_24_bits_is_refused() {
    let mut pcm = vec![0i32; FRAME_SAMPLES * 12];
    let mut encoder = Encoder::new(Config::reference(Layout::SevenPointOneFour, 24)).unwrap();
    encoder.encode(&pcm, None).unwrap();
    // Rsr at 0.8 of full scale and TBR at full scale: 0.8 + 0.707.
    pcm[300 * 12 + 5] = ((0.8 * f64::from(1 << 23)) as i32) << 8;
    pcm[300 * 12 + 11] = ((1 << 23) - 1) << 8;
    assert_eq!(
        encoder.encode(&pcm, None),
        Err(Refused::Fold {
            frame: 1,
            channel: 5,
            sample: 300,
            value: 6_710_886 + 5_931_519,
        })
    );
    assert_eq!(encoder.frames(), 1);
    // The same at −3 dB fits.
    pcm[300 * 12 + 11] = ((1 << 22) - 1) << 8;
    pcm[300 * 12 + 5] = ((0.6 * f64::from(1 << 23)) as i32) << 8;
    encoder.encode(&pcm, None).unwrap();
}

/// White noise at 24 bits codes to more than the word can point past: the
/// frame is refused by the navigation check, named, rather than written
/// short. The bed alone passes the furthest offset the word can name.
#[test]
fn white_noise_is_refused_by_the_navigation_check() {
    let mut state = 0x1234_5678u32;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        // Half scale, so that no fold passes 24 bits.
        (state as i32) >> 9 << 8
    };
    let pcm: Vec<i32> = (0..FRAME_SAMPLES * 12).map(|_| noise()).collect();
    let mut encoder = Encoder::new(Config::reference(Layout::SevenPointOneFour, 24)).unwrap();
    match encoder.encode(&pcm, None) {
        Err(Refused::Navigation {
            frame: 0,
            cause: navigation::OutOfRange::Offset(offset),
        }) => assert!(offset > navigation::MAX_OFFSET),
        other => panic!("{other:?}"),
    }
    assert_eq!(encoder.frames(), 0);
}
