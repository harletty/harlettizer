//! The sequence in Matroska: one audio track, `A_IAMF`.
//!
//! The mapping is the draft Omniphony proposed for IAMF in Matroska
//! (matroska-specification#940, which CELLAR has not settled yet), the AV1
//! mapping's shape over IAMF's own ISO-BMFF encapsulation:
//!
//! - `CodecPrivate` is the `IAConfigurationBox` payload — a version byte, 1,
//!   then the descriptors' length as a LEB128, then the descriptors;
//! - every `SimpleBlock` is one temporal unit, its OBUs as the standalone
//!   stream has them minus the temporal delimiter, whose one bit of news —
//!   whether the unit is a key frame — moves to the block's flags;
//! - `CodecDelay` is what the stream trims off its start and `SeekPreRoll`
//!   what its roll distance asks for, and the trims stay in the OBUs: the
//!   decoder applies them, and no `DiscardPadding` asks for them twice;
//! - `Channels` is 2 and means nothing, as in IAMF's own codec headers.
//!
//! Written as it goes, a cluster at a time, straight to the output: a
//! cluster's size, the segment's, its duration and the seek head are left as
//! placeholders of a fixed width and patched at the end, so the output has
//! to seek — as the standalone stream's loudness already makes it.

use crate::obu::put_leb128;
use std::io::{self, Seek, SeekFrom, Write};

// Element ids (RFC 9559), with the id's marker bits, as they are written.
const EBML: u32 = 0x1A45_DFA3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42F7;
const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x002A_D7B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9C;
const LANGUAGE: u32 = 0x0022_B59C;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const CODEC_DELAY: u32 = 0x56AA;
const SEEK_PRE_ROLL: u32 = 0x56BB;
const DEFAULT_DURATION: u32 = 0x0023_E383;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const BIT_DEPTH: u32 = 0x6264;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const VOID: u32 = 0xEC;

/// `CodecPrivate`'s `configurationVersion`.
const CONFIGURATION_VERSION: u8 = 1;

/// Block timestamps count milliseconds, Matroska's usual tick: a block's
/// timestamp is within half a tick of its unit's start, and `DefaultDuration`
/// states the frame exactly.
const TICK_NS: u64 = 1_000_000;

/// A cluster spans this long, in ticks: one cue every five seconds. Every
/// unit is a key frame, so any block may open one.
const CLUSTER_TICKS: u64 = 5_000;

/// What the seek head can need: three seeks of a four-byte id and an
/// eight-byte position, with their headers. Reserved as a `Void` at the
/// head of the segment and overwritten at the end.
const SEEK_HEAD_ROOM: usize = 96;

/// The track's one number.
const TRACK: u64 = 1;

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

/// A Matroska file being written.
pub(crate) struct Muxer {
    sample_rate: u64,
    frame: u64,
    /// Where the segment's size is, and where its data starts: every
    /// position the seek head and the cues state counts from there.
    segment_size_at: u64,
    segment_data: u64,
    seek_head_at: u64,
    duration_at: u64,
    info: u64,
    tracks: u64,
    /// The cluster being written: where its size is, and its timestamp.
    cluster: Option<(u64, u64)>,
    /// One cue per cluster: its timestamp and its position.
    cues: Vec<(u64, u64)>,
    block: Vec<u8>,
}

impl Muxer {
    /// Write the head of the file — the EBML header, and the segment up to
    /// its first cluster — and return the muxer and where, in `out`, the
    /// descriptors landed.
    pub(crate) fn start<W: Write + Seek>(out: &mut W, track: &Track) -> io::Result<(Self, u64)> {
        let mut head = Vec::with_capacity(512 + track.descriptors.len());
        let mut body = Vec::with_capacity(64);
        put_uint(&mut body, EBML_VERSION, 1);
        put_uint(&mut body, EBML_READ_VERSION, 1);
        put_uint(&mut body, EBML_MAX_ID_LENGTH, 4);
        put_uint(&mut body, EBML_MAX_SIZE_LENGTH, 8);
        put_bytes(&mut body, DOC_TYPE, b"matroska");
        // CodecDelay and SeekPreRoll are version 4's.
        put_uint(&mut body, DOC_TYPE_VERSION, 4);
        put_uint(&mut body, DOC_TYPE_READ_VERSION, 2);
        put_bytes(&mut head, EBML, &body);

        put_id(&mut head, SEGMENT);
        let base = out.stream_position()?;
        let segment_size_at = base + head.len() as u64;
        head.extend_from_slice(&UNKNOWN_SIZE);
        let segment_data = base + head.len() as u64;

        let seek_head_at = head.len();
        put_void(&mut head, SEEK_HEAD_ROOM);

        let info = (head.len() - seek_head_at) as u64;
        body.clear();
        put_uint(&mut body, TIMESTAMP_SCALE, TICK_NS);
        put_id(&mut body, DURATION);
        put_size(&mut body, 8);
        let duration_in_info = body.len();
        body.extend_from_slice(&0f64.to_be_bytes());
        put_bytes(&mut body, MUXING_APP, track.writing_app.as_bytes());
        put_bytes(&mut body, WRITING_APP, track.writing_app.as_bytes());
        let duration_at =
            segment_data + info + header_len(INFO, body.len()) + duration_in_info as u64;
        put_bytes(&mut head, INFO, &body);

        let tracks = (head.len() - seek_head_at) as u64;
        let rate = u64::from(track.sample_rate);
        let ns = |samples: usize| samples as u64 * 1_000_000_000 / rate;
        let mut entry = Vec::with_capacity(128 + track.descriptors.len());
        put_uint(&mut entry, TRACK_NUMBER, TRACK);
        put_uint(&mut entry, TRACK_UID, uid(track.descriptors));
        put_uint(&mut entry, TRACK_TYPE, 2); // audio
        put_uint(&mut entry, FLAG_LACING, 0);
        put_bytes(&mut entry, LANGUAGE, b"und");
        put_bytes(&mut entry, CODEC_ID, b"A_IAMF");
        let mut private = Vec::with_capacity(track.descriptors.len() + 10);
        private.push(CONFIGURATION_VERSION);
        put_leb128(&mut private, track.descriptors.len() as u64);
        let descriptors_in_private = private.len();
        private.extend_from_slice(track.descriptors);
        let descriptors_in_entry = entry.len()
            + header_len(CODEC_PRIVATE, private.len()) as usize
            + descriptors_in_private;
        put_bytes(&mut entry, CODEC_PRIVATE, &private);
        put_uint(&mut entry, CODEC_DELAY, ns(track.trimmed));
        put_uint(
            &mut entry,
            SEEK_PRE_ROLL,
            ns(track.roll_units * track.frame),
        );
        put_uint(&mut entry, DEFAULT_DURATION, ns(track.frame));
        body.clear();
        put_id(&mut body, SAMPLING_FREQUENCY);
        put_size(&mut body, 8);
        body.extend_from_slice(&f64::from(track.sample_rate).to_be_bytes());
        put_uint(&mut body, CHANNELS, 2);
        if let Some(bits) = track.bits {
            put_uint(&mut body, BIT_DEPTH, u64::from(bits));
        }
        put_bytes(&mut entry, AUDIO, &body);
        let entry_header = header_len(TRACK_ENTRY, entry.len());
        let descriptors_at = segment_data
            + tracks
            + header_len(TRACKS, entry_header as usize + entry.len())
            + entry_header
            + descriptors_in_entry as u64;
        put_id(&mut head, TRACKS);
        put_size(&mut head, entry_header + entry.len() as u64);
        put_bytes(&mut head, TRACK_ENTRY, &entry);

        out.write_all(&head)?;
        let muxer = Self {
            sample_rate: rate,
            frame: track.frame as u64,
            segment_size_at,
            segment_data,
            seek_head_at: base + seek_head_at as u64,
            duration_at,
            info,
            tracks,
            cluster: None,
            cues: Vec::new(),
            block: Vec::new(),
        };
        Ok((muxer, descriptors_at))
    }

    /// Write temporal unit `index` — its OBUs, without a temporal delimiter
    /// — as a key frame.
    pub(crate) fn block<W: Write + Seek>(
        &mut self,
        out: &mut W,
        index: u64,
        unit: &[u8],
    ) -> io::Result<()> {
        let ticks = self.ticks(index * self.frame);
        let opens = match self.cluster {
            None => true,
            Some((_, start)) => ticks - start >= CLUSTER_TICKS,
        };
        if opens {
            self.close_cluster(out)?;
            let at = out.stream_position()?;
            self.cues.push((ticks, at - self.segment_data));
            self.block.clear();
            put_id(&mut self.block, CLUSTER);
            self.block.extend_from_slice(&UNKNOWN_SIZE);
            put_uint(&mut self.block, CLUSTER_TIMESTAMP, ticks);
            out.write_all(&self.block)?;
            self.cluster = Some((at + 4, ticks));
        }
        let (_, start) = self.cluster.expect("opened above");
        // A cluster is five seconds, far inside a block's signed 16-bit
        // offset from it.
        let offset = (ticks - start) as i16;
        self.block.clear();
        put_id(&mut self.block, SIMPLE_BLOCK);
        put_size(&mut self.block, 4 + unit.len() as u64);
        put_size(&mut self.block, TRACK);
        self.block.extend_from_slice(&offset.to_be_bytes());
        self.block.push(0x80); // key frame, no lacing
        out.write_all(&self.block)?;
        out.write_all(unit)
    }

    /// Close the file: the last cluster, the cues, the seek head, and the
    /// sizes and duration left open. `presented` is the samples a channel a
    /// decoder hands out, after its trims. Leaves `out` at the end.
    pub(crate) fn finish<W: Write + Seek>(
        &mut self,
        out: &mut W,
        presented: u64,
    ) -> io::Result<()> {
        self.close_cluster(out)?;
        let cues = out.stream_position()? - self.segment_data;
        let mut body = Vec::with_capacity(self.cues.len() * 16);
        let mut point = Vec::with_capacity(32);
        let mut positions = Vec::with_capacity(16);
        for &(ticks, position) in &self.cues {
            positions.clear();
            put_uint(&mut positions, CUE_TRACK, TRACK);
            put_uint(&mut positions, CUE_CLUSTER_POSITION, position);
            point.clear();
            put_uint(&mut point, CUE_TIME, ticks);
            put_bytes(&mut point, CUE_TRACK_POSITIONS, &positions);
            put_bytes(&mut body, CUE_POINT, &point);
        }
        let mut cue_element = Vec::with_capacity(body.len() + 12);
        put_bytes(&mut cue_element, CUES, &body);
        out.write_all(&cue_element)?;
        let end = out.stream_position()?;

        body.clear();
        for (id, position) in [(INFO, self.info), (TRACKS, self.tracks), (CUES, cues)] {
            point.clear();
            put_bytes(&mut point, SEEK_ID, &id_bytes(id));
            put_uint(&mut point, SEEK_POSITION, position);
            put_bytes(&mut body, SEEK, &point);
        }
        let mut seek_head = Vec::with_capacity(SEEK_HEAD_ROOM);
        put_bytes(&mut seek_head, SEEK_HEAD, &body);
        let left = SEEK_HEAD_ROOM - seek_head.len();
        put_void(&mut seek_head, left);
        out.seek(SeekFrom::Start(self.seek_head_at))?;
        out.write_all(&seek_head)?;

        let milliseconds = presented as f64 * 1000.0 / self.sample_rate as f64;
        out.seek(SeekFrom::Start(self.duration_at))?;
        out.write_all(&milliseconds.to_be_bytes())?;
        out.seek(SeekFrom::Start(self.segment_size_at))?;
        out.write_all(&sized(end - self.segment_data))?;
        out.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// Ticks at `samples` into the stream, to the nearest.
    fn ticks(&self, samples: u64) -> u64 {
        let ns_per_tick = u128::from(TICK_NS);
        let rate = u128::from(self.sample_rate);
        let scaled = u128::from(samples) * 1_000_000_000;
        ((scaled + rate * ns_per_tick / 2) / (rate * ns_per_tick)) as u64
    }

    /// Patch the open cluster's size, if there is one.
    fn close_cluster<W: Write + Seek>(&mut self, out: &mut W) -> io::Result<()> {
        let Some((size_at, _)) = self.cluster.take() else {
            return Ok(());
        };
        let end = out.stream_position()?;
        out.seek(SeekFrom::Start(size_at))?;
        out.write_all(&sized(end - size_at - 8))?;
        out.seek(SeekFrom::Start(end))?;
        Ok(())
    }
}

/// An eight-byte size not yet known: the reserved all-ones value, which a
/// reader takes as "unknown" should the file never be finished.
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

/// `size` as an eight-byte element size, the width every placeholder has.
fn sized(size: u64) -> [u8; 8] {
    assert!(size < (1 << 56) - 1, "an element of {size} bytes");
    ((1u64 << 56) | size).to_be_bytes()
}

fn id_bytes(id: u32) -> Vec<u8> {
    let bytes = id.to_be_bytes();
    let skip = (id.leading_zeros() / 8) as usize;
    bytes[skip..].to_vec()
}

fn put_id(out: &mut Vec<u8>, id: u32) {
    out.extend_from_slice(&id_bytes(id));
}

/// Bytes the shortest size of `size` takes. The all-ones value of each
/// width is reserved, so a size one short of a width's ceiling takes the
/// next.
fn size_len(size: u64) -> usize {
    (1..=8)
        .find(|&w| size < (1 << (7 * w)) - 1)
        .expect("an element under 2^56 bytes")
}

fn put_size(out: &mut Vec<u8>, size: u64) {
    let width = size_len(size);
    let marked = (1u64 << (7 * width)) | size;
    out.extend_from_slice(&marked.to_be_bytes()[8 - width..]);
}

/// The id and size an element of `size` bytes is preceded by.
fn header_len(id: u32, size: usize) -> u64 {
    (id_bytes(id).len() + size_len(size as u64)) as u64
}

fn put_bytes(out: &mut Vec<u8>, id: u32, payload: &[u8]) {
    put_id(out, id);
    put_size(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

fn put_uint(out: &mut Vec<u8>, id: u32, value: u64) {
    let width = (8 - value.leading_zeros() as usize / 8).max(1);
    put_bytes(out, id, &value.to_be_bytes()[8 - width..]);
}

/// A `Void` element `room` bytes long in all, header included.
fn put_void(out: &mut Vec<u8>, room: usize) {
    assert!(room >= 2, "a Void element takes two bytes at least");
    let size = (room - 2) as u64;
    assert!(size_len(size) == 1, "a Void of {room} bytes");
    put_bytes(out, VOID, &vec![0; size as usize]);
}

/// A track UID that follows from what the track carries: FNV-1a over the
/// descriptors, never zero. The same sequence gets the same file.
fn uid(descriptors: &[u8]) -> u64 {
    let hash = descriptors.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    hash.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    use crate::layout::SEVEN_ONE_FOUR;
    use crate::stream::{Codec, Config, Element, Headphones, Loudness, Writer};
    use std::io::Cursor;

    /// One element as read back: its id, and where its payload is.
    #[derive(Debug, Clone, Copy)]
    struct Read {
        id: u32,
        at: usize,
        end: usize,
    }

    fn vint(bytes: &[u8], at: &mut usize, keep_marker: bool) -> u64 {
        let width = bytes[*at].leading_zeros() as usize + 1;
        assert!(width <= 8, "a vint at {at}");
        let mut value = u64::from(bytes[*at]);
        if !keep_marker {
            value &= (1 << (8 - width)) - 1;
        }
        for i in 1..width {
            value = (value << 8) | u64::from(bytes[*at + i]);
        }
        *at += width;
        value
    }

    /// The elements in `bytes[at..end]`, which they have to fill exactly.
    fn children(bytes: &[u8], mut at: usize, end: usize) -> Vec<Read> {
        let mut out = Vec::new();
        while at < end {
            let id = vint(bytes, &mut at, true) as u32;
            let size = vint(bytes, &mut at, false) as usize;
            out.push(Read {
                id,
                at,
                end: at + size,
            });
            at += size;
        }
        assert_eq!(at, end, "children overrun their parent");
        out
    }

    fn only(elements: &[Read], id: u32) -> Read {
        let found: Vec<_> = elements.iter().filter(|e| e.id == id).collect();
        assert_eq!(found.len(), 1, "{id:#x}");
        *found[0]
    }

    fn uint(bytes: &[u8], e: Read) -> u64 {
        bytes[e.at..e.end]
            .iter()
            .fold(0, |v, &b| (v << 8) | u64::from(b))
    }

    fn float(bytes: &[u8], e: Read) -> f64 {
        f64::from_be_bytes(bytes[e.at..e.end].try_into().unwrap())
    }

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
    /// blocks count time from their clusters, and the duration is what the
    /// stream presents after its trim.
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
                assert_eq!(block.id, SIMPLE_BLOCK);
                let mut at = block.at;
                assert_eq!(vint(&file, &mut at, false), TRACK);
                let offset = i16::from_be_bytes([file[at], file[at + 1]]);
                let ticks = (unit * 4096 * 1000 + 8000) / 16_000;
                assert_eq!(start + offset as u64, ticks, "unit {unit}");
                assert_eq!(file[at + 2], 0x80);
                rebuilt.extend_from_slice(&[4 << 3, 0]);
                rebuilt.extend_from_slice(&file[at + 3..block.end]);
                unit += 1;
            }
        }
        assert_eq!(unit, 50);
        assert!(
            rebuilt == standalone,
            "the blocks are not the standalone sequence"
        );
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

    #[test]
    fn a_void_fills_exactly_the_room_it_is_given() {
        for room in [2, 3, 50, 96, 128] {
            let mut out = Vec::new();
            put_void(&mut out, room);
            assert_eq!(out.len(), room);
        }
    }
}
