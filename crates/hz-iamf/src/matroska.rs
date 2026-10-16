//! The sequence in Matroska: one audio track, `A_IAMF`.
//!
//! The mapping is the draft Omniphony proposed for IAMF in Matroska
//! (matroska-specification#940, which CELLAR has not settled yet), the AV1
//! mapping's shape over IAMF's own ISO-BMFF encapsulation:
//!
//! - `CodecPrivate` is the `IAConfigurationBox` payload — a version byte, 1,
//!   then the descriptors' length as a LEB128, then the descriptors;
//! - every block is one temporal unit, its OBUs as the standalone stream has
//!   them minus the temporal delimiter, whose one bit of news — whether the
//!   unit is a key frame — moves to the block's flags; and every unit is a
//!   key frame (see [`crate::stream`]), so every block is a `SimpleBlock`
//!   flagged key, except the one below;
//! - the trims stay in the OBUs, the decoder's to apply, and the container
//!   restates them for a player that cannot decode the track: `CodecDelay` is
//!   what the stream trims off its start, and a unit that trims its end is a
//!   `BlockGroup` whose `DiscardPadding` says how much, with the
//!   `BlockDuration` a track that states `DefaultDuration` owes every group —
//!   a player applies one of the two, never both; `SeekPreRoll` is what the
//!   roll distance asks for;
//! - `Channels` is 2 and means nothing, as in IAMF's own codec headers.
//!
//! The file around the track — clusters, cues, seek head, written as it goes
//! and patched at the end, so the output has to seek, as the standalone
//! stream's loudness already makes it — is [`hz_matroska`]'s, which DTS's
//! `A_DTS` track shares.

use crate::obu::put_leb128;
use hz_matroska::Muxer;
use std::io::{self, Seek, Write};

/// `CodecPrivate`'s `configurationVersion`.
const CONFIGURATION_VERSION: u8 = 1;

/// What the track states about the sequence it carries.
pub(crate) struct Track<'a> {
    /// The descriptors, which become the `CodecPrivate`.
    pub descriptors: &'a [u8],
    pub sample_rate: u32,
    /// Samples a channel per temporal unit.
    pub frame: usize,
    /// Samples the stream trims off its start.
    pub trimmed: usize,
    /// Units a decoder has to decode before a seek point to be right at it.
    pub roll_units: usize,
    /// Bits a sample, for a codec that has a depth.
    pub bits: Option<u32>,
    pub writing_app: &'a str,
}

/// Write the head of the file and return the muxer and where, in `out`,
/// the descriptors landed.
pub(crate) fn start<W: Write + Seek>(out: &mut W, track: &Track) -> io::Result<(Muxer, u64)> {
    let mut private = Vec::with_capacity(track.descriptors.len() + 10);
    private.push(CONFIGURATION_VERSION);
    put_leb128(&mut private, track.descriptors.len() as u64);
    let descriptors_in_private = private.len() as u64;
    private.extend_from_slice(track.descriptors);
    let (muxer, private_at) = Muxer::start(
        out,
        &hz_matroska::Track {
            codec_id: "A_IAMF",
            codec_private: Some(&private),
            sample_rate: track.sample_rate,
            frame: track.frame,
            channels: 2,
            bits: track.bits,
            codec_delay: Some(track.trimmed),
            seek_pre_roll: Some(track.roll_units * track.frame),
            uid: hz_matroska::uid(track.descriptors),
            writing_app: track.writing_app,
        },
    )?;
    let private_at = private_at.expect("the track has private data");
    Ok((muxer, private_at + descriptors_in_private))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hz_matroska::id::*;
    use hz_matroska::read::{self, Element as Read};

    const TICK_NS: u64 = hz_matroska::TICK_NS;
    const TRACK: u64 = hz_matroska::TRACK;

    fn vint(bytes: &[u8], at: &mut usize, keep_marker: bool) -> u64 {
        read::vint(bytes, at, keep_marker).unwrap()
    }

    fn children(bytes: &[u8], at: usize, end: usize) -> Vec<Read> {
        read::children(bytes, at, end).unwrap()
    }

    fn only(elements: &[Read], id: u32) -> Read {
        read::only(elements, id).unwrap()
    }

    fn uint(bytes: &[u8], e: Read) -> u64 {
        read::uint(bytes, e)
    }

    fn float(bytes: &[u8], e: Read) -> f64 {
        read::float(bytes, e).unwrap()
    }

    use crate::layout::SEVEN_ONE_FOUR;
    use crate::stream::{Codec, Config, Element, Headphones, Loudness, Writer};
    use std::io::Cursor;

    fn sequence(framing: Option<&str>, config: &Config, lengths: &[usize]) -> Vec<u8> {
        let out = Cursor::new(Vec::new());
        let mut writer = match framing {
            None => Writer::new(out, config.clone()),
            Some(app) => Writer::matroska(out, config.clone(), app),
        }
        .unwrap();
        let width = 12;
        for (n, &length) in lengths.iter().enumerate() {
            let samples: Vec<i32> = (0..(length * width) as i32)
                .map(|i| (i * 37 + n as i32) % 20_000 - 10_000)
                .collect();
            writer.push(&samples, &[]).unwrap();
        }
        let loudness = Loudness {
            integrated: -23.5,
            digital_peak: -3.0,
            true_peak: -2.5,
        };
        writer
            .finish([
                loudness,
                Loudness {
                    integrated: -22.0,
                    ..loudness
                },
            ])
            .unwrap()
            .into_inner()
    }

    /// The Matroska file holds the standalone stream exactly: its codec
    /// private data and its blocks, a temporal delimiter put back before
    /// each, give back the standalone sequence byte for byte, loudness
    /// included. And around them, everything a reader goes by is right: the
    /// sizes nest, the seek head and the cues point at what they say, the
    /// blocks count time from their clusters, the duration is what the
    /// stream presents after its trim, and the one unit that trims its end
    /// — the short last one — is a `BlockGroup` whose `DiscardPadding`
    /// restates that trim beside the `BlockDuration` the track's
    /// `DefaultDuration` makes mandatory, every other a key `SimpleBlock`.
    #[test]
    fn the_file_carries_the_standalone_sequence_and_says_where_it_is() {
        let config = Config {
            elements: vec![Element::Channels(SEVEN_ONE_FOUR)],
            codec: Codec::Lpcm,
            sample_rate: 16_000,
            bits: 16,
            frame: 4096,
            headphones: Headphones::Stereo,
            presentation: Default::default(),
        };
        // Fifty units, three clusters of five seconds; the last short.
        let mut lengths = vec![4096; 49];
        lengths.push(1000);
        let standalone = sequence(None, &config, &lengths);
        let file = sequence(Some("test"), &config, &lengths);

        let top = children(&file, 0, file.len());
        assert_eq!(top.len(), 2);
        let header = children(&file, top[0].at, top[0].end);
        assert_eq!(
            &file[only(&header, DOC_TYPE).at..only(&header, DOC_TYPE).end],
            b"matroska"
        );
        let segment = top[1];
        assert_eq!(segment.id, SEGMENT);
        let parts = children(&file, segment.at, segment.end);

        let seeks = children(&file, parts[0].at, parts[0].end);
        assert_eq!(parts[0].id, SEEK_HEAD);
        assert_eq!(parts[1].id, VOID);
        let mut sought = Vec::new();
        for seek in &seeks {
            let fields = children(&file, seek.at, seek.end);
            let id = uint(&file, only(&fields, SEEK_ID)) as u32;
            let position = segment.at + uint(&file, only(&fields, SEEK_POSITION)) as usize;
            let mut at = position;
            assert_eq!(vint(&file, &mut at, true) as u32, id);
            sought.push(id);
        }
        assert_eq!(sought, [INFO, TRACKS, CUES]);

        let info = children(&file, only(&parts, INFO).at, only(&parts, INFO).end);
        assert_eq!(uint(&file, only(&info, TIMESTAMP_SCALE)), TICK_NS);
        let presented = 49 * 4096 + 1000;
        assert_eq!(
            float(&file, only(&info, DURATION)),
            presented as f64 * 1000.0 / 16_000.0
        );

        let tracks = children(&file, only(&parts, TRACKS).at, only(&parts, TRACKS).end);
        let entry = children(
            &file,
            only(&tracks, TRACK_ENTRY).at,
            only(&tracks, TRACK_ENTRY).end,
        );
        assert_eq!(
            &file[only(&entry, CODEC_ID).at..only(&entry, CODEC_ID).end],
            b"A_IAMF"
        );
        assert_eq!(uint(&file, only(&entry, CODEC_DELAY)), 0);
        assert_eq!(uint(&file, only(&entry, SEEK_PRE_ROLL)), 0);
        assert_eq!(uint(&file, only(&entry, DEFAULT_DURATION)), 256_000_000);
        let audio = children(&file, only(&entry, AUDIO).at, only(&entry, AUDIO).end);
        assert_eq!(float(&file, only(&audio, SAMPLING_FREQUENCY)), 16_000.0);
        assert_eq!(uint(&file, only(&audio, BIT_DEPTH)), 16);
        let private = only(&entry, CODEC_PRIVATE);
        assert_eq!(file[private.at], CONFIGURATION_VERSION);
        let mut at = private.at + 1;
        let mut size = 0u64;
        for shift in (0..).step_by(7) {
            let b = file[at];
            at += 1;
            size |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
        }
        assert_eq!(at + size as usize, private.end);

        let mut rebuilt = file[at..private.end].to_vec();
        let clusters: Vec<_> = parts.iter().filter(|p| p.id == CLUSTER).collect();
        assert_eq!(clusters.len(), 3);
        let cues = children(&file, only(&parts, CUES).at, only(&parts, CUES).end);
        assert_eq!(cues.len(), clusters.len());
        let mut unit = 0u64;
        for (cluster, cue) in clusters.iter().zip(&cues) {
            let fields = children(&file, cluster.at, cluster.end);
            let start = uint(&file, fields[0]);
            assert_eq!(fields[0].id, CLUSTER_TIMESTAMP);
            let point = children(&file, cue.at, cue.end);
            assert_eq!(uint(&file, only(&point, CUE_TIME)), start);
            let positions = children(
                &file,
                only(&point, CUE_TRACK_POSITIONS).at,
                only(&point, CUE_TRACK_POSITIONS).end,
            );
            let mut at = segment.at + uint(&file, only(&positions, CUE_CLUSTER_POSITION)) as usize;
            assert_eq!(vint(&file, &mut at, true) as u32, CLUSTER);
            for block in &fields[1..] {
                let last = unit == 49;
                let (data, flags) = if last {
                    assert_eq!(block.id, BLOCK_GROUP, "unit {unit}");
                    let group = children(&file, block.at, block.end);
                    assert_eq!(group.len(), 3);
                    assert_eq!(group[0].id, BLOCK);
                    // The unit's 4096 samples at 16 kHz, in ticks.
                    assert_eq!(group[1].id, BLOCK_DURATION);
                    assert_eq!(uint(&file, group[1]), 256);
                    // 3096 samples at 16 kHz.
                    assert_eq!(group[2].id, DISCARD_PADDING);
                    assert_eq!(uint(&file, group[2]), 193_500_000);
                    (group[0], 0)
                } else {
                    assert_eq!(block.id, SIMPLE_BLOCK, "unit {unit}");
                    (*block, 0x80)
                };
                let mut at = data.at;
                assert_eq!(vint(&file, &mut at, false), TRACK);
                let offset = i16::from_be_bytes([file[at], file[at + 1]]);
                let ticks = (unit * 4096 * 1000 + 8000) / 16_000;
                assert_eq!(start + offset as u64, ticks, "unit {unit}");
                assert_eq!(file[at + 2], flags);
                rebuilt.extend_from_slice(&[4 << 3, 0]);
                rebuilt.extend_from_slice(&file[at + 3..data.end]);
                unit += 1;
            }
        }
        assert_eq!(unit, 50);
        assert!(
            rebuilt == standalone,
            "the blocks are not the standalone sequence"
        );
    }

    /// A sequence of whole units trims nothing at its end, and writes no
    /// `BlockGroup`.
    #[test]
    fn a_sequence_without_an_end_trim_has_only_simple_blocks() {
        let config = Config {
            elements: vec![Element::Channels(SEVEN_ONE_FOUR)],
            codec: Codec::Lpcm,
            sample_rate: 16_000,
            bits: 16,
            frame: 1024,
            headphones: Headphones::Stereo,
            presentation: Default::default(),
        };
        let file = sequence(Some("test"), &config, &[1024; 7]);
        let top = children(&file, 0, file.len());
        let parts = children(&file, top[1].at, top[1].end);
        let clusters: Vec<_> = parts.iter().filter(|p| p.id == CLUSTER).collect();
        assert_eq!(clusters.len(), 1);
        let fields = children(&file, clusters[0].at, clusters[0].end);
        assert_eq!(fields.len(), 8);
        assert!(fields[1..].iter().all(|f| f.id == SIMPLE_BLOCK));
    }

    /// Opus states its pre-skip as the codec delay and its roll distance as
    /// the seek pre-roll: 80 ms at 20 ms frames.
    #[cfg(feature = "opus")]
    #[test]
    fn opus_states_its_delay_and_pre_roll() {
        let config = Config {
            elements: vec![Element::Channels(SEVEN_ONE_FOUR)],
            codec: Codec::Opus { bitrate: 64_000 },
            sample_rate: 48_000,
            bits: 24,
            frame: 960,
            headphones: Headphones::Stereo,
            presentation: Default::default(),
        };
        let file = sequence(Some("test"), &config, &[960; 10]);
        let top = children(&file, 0, file.len());
        let parts = children(&file, top[1].at, top[1].end);
        let tracks = children(&file, only(&parts, TRACKS).at, only(&parts, TRACKS).end);
        let entry = children(&file, tracks[0].at, tracks[0].end);
        assert_eq!(
            uint(&file, only(&entry, CODEC_DELAY)),
            312 * 1_000_000_000 / 48_000
        );
        assert_eq!(uint(&file, only(&entry, SEEK_PRE_ROLL)), 80_000_000);
        let audio = children(&file, only(&entry, AUDIO).at, only(&entry, AUDIO).end);
        assert!(audio.iter().all(|e| e.id != BIT_DEPTH));
        let info = children(&file, only(&parts, INFO).at, only(&parts, INFO).end);
        assert_eq!(float(&file, only(&info, DURATION)), 200.0);
    }
}
