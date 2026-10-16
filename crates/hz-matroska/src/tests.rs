use super::read::{self, Element};
use super::*;
use std::io::Cursor;

#[test]
fn sizes_take_the_shortest_width_short_of_the_reserved_value() {
    let mut out = Vec::new();
    put_size(&mut out, 126);
    assert_eq!(out, [0xfe]);
    out.clear();
    put_size(&mut out, 127);
    assert_eq!(out, [0x40, 0x7f]);
    out.clear();
    put_size(&mut out, 0);
    assert_eq!(out, [0x80]);
    assert_eq!(sized(5), [1, 0, 0, 0, 0, 0, 0, 5]);
}

#[test]
fn ids_keep_their_marker_and_drop_leading_zero_bytes() {
    assert_eq!(id_bytes(SEGMENT), [0x18, 0x53, 0x80, 0x67]);
    assert_eq!(id_bytes(LANGUAGE), [0x22, 0xb5, 0x9c]);
    assert_eq!(id_bytes(SEEK_ID), [0x53, 0xab]);
    assert_eq!(id_bytes(TRACK_ENTRY), [0xae]);
}

#[test]
fn unsigned_integers_are_as_short_as_their_value() {
    let mut out = Vec::new();
    put_uint(&mut out, TRACK_TYPE, 2);
    assert_eq!(out, [0x83, 0x81, 2]);
    out.clear();
    put_uint(&mut out, FLAG_LACING, 0);
    assert_eq!(out, [0x9c, 0x81, 0]);
    out.clear();
    put_uint(&mut out, CODEC_DELAY, 6_500_000);
    assert_eq!(out, [0x56, 0xaa, 0x83, 0x63, 0x2e, 0xa0]);
}

#[test]
fn signed_integers_take_the_fewest_bytes_that_hold_them() {
    let mut out = Vec::new();
    put_sint(&mut out, DISCARD_PADDING, 0);
    assert_eq!(out, [0x75, 0xa2, 0x81, 0]);
    out.clear();
    put_sint(&mut out, DISCARD_PADDING, 127);
    assert_eq!(out, [0x75, 0xa2, 0x81, 0x7f]);
    out.clear();
    put_sint(&mut out, DISCARD_PADDING, 128);
    assert_eq!(out, [0x75, 0xa2, 0x82, 0x00, 0x80]);
    out.clear();
    put_sint(&mut out, DISCARD_PADDING, -129);
    assert_eq!(out, [0x75, 0xa2, 0x82, 0xff, 0x7f]);
    out.clear();
    // 3096 samples at 16 kHz.
    put_sint(&mut out, DISCARD_PADDING, 193_500_000);
    assert_eq!(out, [0x75, 0xa2, 0x84, 0x0b, 0x88, 0x93, 0x60]);
}

/// The reader takes back what the writer gives, signs included.
#[test]
fn the_reader_takes_back_signed_integers() {
    for value in [0, 1, -1, 127, 128, -129, 193_500_000, i64::MIN, i64::MAX] {
        let mut out = Vec::new();
        put_sint(&mut out, DISCARD_PADDING, value);
        let elements = read::children(&out, 0, out.len()).unwrap();
        assert_eq!(read::sint(&out, elements[0]), value);
    }
}

#[test]
fn a_void_fills_exactly_the_room_it_is_given() {
    for room in [2, 3, 50, 96, 128] {
        let mut out = Vec::new();
        put_void(&mut out, room);
        assert_eq!(out.len(), room);
    }
}

fn track<'a>(private: Option<&'a [u8]>, delay: Option<usize>) -> Track<'a> {
    Track {
        codec_id: "A_TEST",
        codec_private: private,
        sample_rate: 48_000,
        frame: 512,
        channels: 8,
        bits: Some(24),
        codec_delay: delay,
        seek_pre_roll: delay,
        uid: uid(b"test"),
        writing_app: "test",
    }
}

/// `frames` frames of distinct bytes, the last `trim` samples short.
fn file(track: &Track, frames: usize, trim: usize) -> (Vec<u8>, Vec<Vec<u8>>, Option<u64>) {
    let mut out = Cursor::new(Vec::new());
    let (mut muxer, private_at) = Muxer::start(&mut out, track).unwrap();
    let mut sent = Vec::new();
    for n in 0..frames {
        let frame: Vec<u8> = (0..100 + n % 37).map(|i| (i * 7 + n) as u8).collect();
        let last = n + 1 == frames;
        muxer
            .block(&mut out, n as u64, &frame, if last { trim } else { 0 })
            .unwrap();
        sent.push(frame);
    }
    let presented = (frames * track.frame - trim) as u64;
    muxer.finish(&mut out, presented).unwrap();
    (out.into_inner(), sent, private_at)
}

fn only(elements: &[Element], id: u32) -> Element {
    read::only(elements, id).unwrap()
}

/// A finished file is one a reader can walk: the sizes nest, the seek head
/// and the cues point at what they say, every frame comes back as it was
/// sent at the time it was sent, a key frame, and the short last one in a
/// group whose padding is the trim; the track states what it was given and
/// no delay or pre-roll a mapping did not ask for.
#[test]
fn a_finished_file_gives_back_every_frame_where_it_was_put() {
    // 1,000 frames of 512 at 48 kHz: 10.7 s, three clusters.
    let (bytes, sent, private_at) = file(&track(None, None), 1000, 200);
    assert_eq!(private_at, None);
    let (segment, parts) = read::segment(&bytes).unwrap();
    assert_eq!(parts[0].id, SEEK_HEAD);
    let mut sought = Vec::new();
    for seek in read::children(&bytes, parts[0].at, parts[0].end).unwrap() {
        let fields = read::children(&bytes, seek.at, seek.end).unwrap();
        let id = read::uint(&bytes, only(&fields, SEEK_ID)) as u32;
        let mut at = segment.at + read::uint(&bytes, only(&fields, SEEK_POSITION)) as usize;
        assert_eq!(read::vint(&bytes, &mut at, true).unwrap() as u32, id);
        sought.push(id);
    }
    assert_eq!(sought, [INFO, TRACKS, CUES]);

    let info = read::children(&bytes, only(&parts, INFO).at, only(&parts, INFO).end).unwrap();
    let presented = 1000.0 * 512.0 - 200.0;
    assert_eq!(
        read::float(&bytes, only(&info, DURATION)).unwrap(),
        presented * 1000.0 / 48_000.0
    );

    let entry = read::track(&bytes, TRACK).unwrap();
    let codec = only(&entry, CODEC_ID);
    assert_eq!(&bytes[codec.at..codec.end], b"A_TEST");
    for absent in [CODEC_PRIVATE, CODEC_DELAY, SEEK_PRE_ROLL] {
        assert!(entry.iter().all(|e| e.id != absent), "{absent:#x}");
    }
    assert_eq!(
        read::uint(&bytes, only(&entry, DEFAULT_DURATION)),
        10_666_666
    );
    let audio = read::children(&bytes, only(&entry, AUDIO).at, only(&entry, AUDIO).end).unwrap();
    assert_eq!(read::uint(&bytes, only(&audio, CHANNELS)), 8);
    assert_eq!(read::uint(&bytes, only(&audio, BIT_DEPTH)), 24);

    let clusters: Vec<_> = parts.iter().filter(|p| p.id == CLUSTER).collect();
    assert_eq!(clusters.len(), 3);
    let cues = read::children(&bytes, only(&parts, CUES).at, only(&parts, CUES).end).unwrap();
    for (cluster, cue) in clusters.iter().zip(&cues) {
        let fields = read::children(&bytes, cluster.at, cluster.end).unwrap();
        let point = read::children(&bytes, cue.at, cue.end).unwrap();
        assert_eq!(
            read::uint(&bytes, only(&point, CUE_TIME)),
            read::uint(&bytes, only(&fields, CLUSTER_TIMESTAMP))
        );
        let positions = only(&point, CUE_TRACK_POSITIONS);
        let positions = read::children(&bytes, positions.at, positions.end).unwrap();
        let mut at =
            segment.at + read::uint(&bytes, only(&positions, CUE_CLUSTER_POSITION)) as usize;
        assert_eq!(read::vint(&bytes, &mut at, true).unwrap() as u32, CLUSTER);
    }

    let blocks = read::blocks(&bytes, TRACK).unwrap();
    assert_eq!(blocks.len(), sent.len());
    for (n, (block, frame)) in blocks.iter().zip(&sent).enumerate() {
        assert_eq!(block.frame, frame.as_slice(), "frame {n}");
        assert!(block.key, "frame {n}");
        assert_eq!(block.ticks, (n as u64 * 512 * 1000 + 24_000) / 48_000);
        let last = n + 1 == sent.len();
        assert_eq!(block.discard_padding.is_some(), last, "frame {n}");
    }
    let last = blocks.last().unwrap();
    assert_eq!(last.discard_padding, Some(200 * 1_000_000_000 / 48_000));
    assert_eq!(last.duration, Some(11));
}

/// A mapping with private data finds it where the muxer says it put it, and
/// states the delay and pre-roll it asked for, zero included.
#[test]
fn private_data_lands_where_the_muxer_says() {
    let private = [9u8, 8, 7, 6, 5];
    let (bytes, _, private_at) = file(&track(Some(&private), Some(0)), 3, 0);
    let at = private_at.unwrap() as usize;
    assert_eq!(&bytes[at..at + private.len()], private);
    let entry = read::track(&bytes, TRACK).unwrap();
    let element = only(&entry, CODEC_PRIVATE);
    assert_eq!(element.at, at);
    assert_eq!(read::uint(&bytes, only(&entry, CODEC_DELAY)), 0);
    assert_eq!(read::uint(&bytes, only(&entry, SEEK_PRE_ROLL)), 0);
    let blocks = read::blocks(&bytes, TRACK).unwrap();
    assert!(blocks.iter().all(|b| b.discard_padding.is_none()));
}

/// A file whose sizes were never patched is refused, not read past.
#[test]
fn an_unfinished_file_is_refused() {
    let mut out = Cursor::new(Vec::new());
    let (mut muxer, _) = Muxer::start(&mut out, &track(None, None)).unwrap();
    muxer.block(&mut out, 0, &[1, 2, 3], 0).unwrap();
    let bytes = out.into_inner();
    let error = read::blocks(&bytes, TRACK).unwrap_err();
    assert!(error.0.contains("unknown size"), "{error}");
}
