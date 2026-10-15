//! One audio track in Matroska, a coded frame a block, and a reader for what
//! it writes.
//!
//! What a codec's mapping says — its `CodecID`, its `CodecPrivate`, whether it
//! states a delay or a pre-roll, what `Channels` means for it — is the
//! caller's ([`Track`]); everything around it is the same for every codec and
//! lives here once:
//!
//! - every block is one coded frame and a key frame: a `SimpleBlock` flagged
//!   key, except a frame whose end is padding, which is a `BlockGroup` whose
//!   `DiscardPadding` says how much, with the `BlockDuration` a track that
//!   states `DefaultDuration` owes every group;
//! - block timestamps count milliseconds, a cluster spans five seconds, and
//!   there is one cue per cluster, so a player can seek to any of them;
//! - written as it goes, a cluster at a time, straight to the output: a
//!   cluster's size, the segment's, its duration and the seek head are left
//!   as placeholders of a fixed width and patched at the end, so the output
//!   has to seek.
//!
//! The IAMF mapping is `hz_iamf::matroska`'s, DTS's (`A_DTS`) the command
//! line's. [`read`] reads back what this writes, and any finished file of the
//! same shape, for the checks.

pub mod read;

use std::io::{self, Seek, SeekFrom, Write};

/// Element ids (RFC 9559), with the id's marker bits, as they are written.
pub mod id {
    pub const EBML: u32 = 0x1A45_DFA3;
    pub const EBML_VERSION: u32 = 0x4286;
    pub const EBML_READ_VERSION: u32 = 0x42F7;
    pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
    pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const DOC_TYPE_VERSION: u32 = 0x4287;
    pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;
    pub const SEGMENT: u32 = 0x1853_8067;
    pub const SEEK_HEAD: u32 = 0x114D_9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;
    pub const INFO: u32 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u32 = 0x002A_D7B1;
    pub const DURATION: u32 = 0x4489;
    pub const MUXING_APP: u32 = 0x4D80;
    pub const WRITING_APP: u32 = 0x5741;
    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_UID: u32 = 0x73C5;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const FLAG_LACING: u32 = 0x9C;
    pub const LANGUAGE: u32 = 0x0022_B59C;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const CODEC_DELAY: u32 = 0x56AA;
    pub const SEEK_PRE_ROLL: u32 = 0x56BB;
    pub const DEFAULT_DURATION: u32 = 0x0023_E383;
    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const CHANNELS: u32 = 0x9F;
    pub const BIT_DEPTH: u32 = 0x6264;
    pub const CLUSTER: u32 = 0x1F43_B675;
    pub const CLUSTER_TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
    pub const BLOCK_GROUP: u32 = 0xA0;
    pub const BLOCK: u32 = 0xA1;
    pub const BLOCK_DURATION: u32 = 0x9B;
    pub const DISCARD_PADDING: u32 = 0x75A2;
    pub const CUES: u32 = 0x1C53_BB6B;
    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_TRACK: u32 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
    pub const VOID: u32 = 0xEC;
}

use id::*;

/// Block timestamps count milliseconds, Matroska's usual tick: a block's
/// timestamp is within half a tick of its frame's start, and
/// `DefaultDuration` states the frame exactly.
pub const TICK_NS: u64 = 1_000_000;

/// A cluster spans this long, in ticks: one cue every five seconds. Every
/// frame is a key frame, so any block may open one.
const CLUSTER_TICKS: u64 = 5_000;

/// What the seek head can need: three seeks of a four-byte id and an
/// eight-byte position, with their headers. Reserved as a `Void` at the
/// head of the segment and overwritten at the end.
const SEEK_HEAD_ROOM: usize = 96;

/// The track's one number.
pub const TRACK: u64 = 1;

/// What the track states about the stream it carries: what its codec's
/// mapping puts in the track entry.
pub struct Track<'a> {
    /// `A_IAMF`, `A_DTS`, …
    pub codec_id: &'a str,
    /// The codec's private data, for a mapping that has some.
    pub codec_private: Option<&'a [u8]>,
    pub sample_rate: u32,
    /// Samples a channel per coded frame: one block's worth.
    pub frame: usize,
    /// What the mapping says `Channels` is.
    pub channels: u32,
    /// Bits a sample, for a codec that has a depth.
    pub bits: Option<u32>,
    /// Samples the stream trims off its start, for a mapping that states it
    /// as `CodecDelay`.
    pub codec_delay: Option<usize>,
    /// Samples a decoder has to decode before a seek point to be right at
    /// it, for a mapping that states it as `SeekPreRoll`.
    pub seek_pre_roll: Option<usize>,
    /// The track's UID, never zero: [`uid`] of what the track carries, so
    /// that the same stream gets the same file.
    pub uid: u64,
    /// What wrote the file.
    pub writing_app: &'a str,
}

/// A Matroska file being written.
pub struct Muxer {
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
    /// codec private data landed, for a caller that patches it at the end.
    pub fn start<W: Write + Seek>(out: &mut W, track: &Track) -> io::Result<(Self, Option<u64>)> {
        let private_len = track.codec_private.map_or(0, <[u8]>::len);
        let mut head = Vec::with_capacity(512 + private_len);
        let mut body = Vec::with_capacity(64);
        put_uint(&mut body, EBML_VERSION, 1);
        put_uint(&mut body, EBML_READ_VERSION, 1);
        put_uint(&mut body, EBML_MAX_ID_LENGTH, 4);
        put_uint(&mut body, EBML_MAX_SIZE_LENGTH, 8);
        put_bytes(&mut body, DOC_TYPE, b"matroska");
        // CodecDelay, SeekPreRoll and DiscardPadding are version 4's.
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
        let mut entry = Vec::with_capacity(128 + private_len);
        put_uint(&mut entry, TRACK_NUMBER, TRACK);
        put_uint(&mut entry, TRACK_UID, track.uid);
        put_uint(&mut entry, TRACK_TYPE, 2); // audio
        put_uint(&mut entry, FLAG_LACING, 0);
        put_bytes(&mut entry, LANGUAGE, b"und");
        put_bytes(&mut entry, CODEC_ID, track.codec_id.as_bytes());
        let private_in_entry = track.codec_private.map(|private| {
            let at = entry.len() + header_len(CODEC_PRIVATE, private.len()) as usize;
            put_bytes(&mut entry, CODEC_PRIVATE, private);
            at
        });
        if let Some(delay) = track.codec_delay {
            put_uint(&mut entry, CODEC_DELAY, ns(delay));
        }
        if let Some(roll) = track.seek_pre_roll {
            put_uint(&mut entry, SEEK_PRE_ROLL, ns(roll));
        }
        put_uint(&mut entry, DEFAULT_DURATION, ns(track.frame));
        body.clear();
        put_id(&mut body, SAMPLING_FREQUENCY);
        put_size(&mut body, 8);
        body.extend_from_slice(&f64::from(track.sample_rate).to_be_bytes());
        put_uint(&mut body, CHANNELS, u64::from(track.channels));
        if let Some(bits) = track.bits {
            put_uint(&mut body, BIT_DEPTH, u64::from(bits));
        }
        put_bytes(&mut entry, AUDIO, &body);
        let entry_header = header_len(TRACK_ENTRY, entry.len());
        let entry_at = segment_data
            + tracks
            + header_len(TRACKS, entry_header as usize + entry.len())
            + entry_header;
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
        Ok((muxer, private_in_entry.map(|at| entry_at + at as u64)))
    }

    /// Write coded frame `index` as a key frame; `end_trim` is the samples a
    /// channel of padding at its end, which the block then states as
    /// `DiscardPadding`.
    pub fn block<W: Write + Seek>(
        &mut self,
        out: &mut W,
        index: u64,
        frame: &[u8],
        end_trim: usize,
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
        if end_trim == 0 {
            put_id(&mut self.block, SIMPLE_BLOCK);
            put_size(&mut self.block, 4 + frame.len() as u64);
            put_size(&mut self.block, TRACK);
            self.block.extend_from_slice(&offset.to_be_bytes());
            self.block.push(0x80); // key frame, no lacing
            out.write_all(&self.block)?;
            return out.write_all(frame);
        }
        // A `Block` has the `SimpleBlock`'s header without its key flag —
        // a `BlockGroup` says so by carrying no `ReferenceBlock`. The track
        // states `DefaultDuration`, which makes `BlockDuration` (in ticks)
        // mandatory in every group; the padding is a signed count of
        // nanoseconds.
        let mut tail = Vec::with_capacity(24);
        put_uint(&mut tail, BLOCK_DURATION, self.ticks(self.frame));
        put_sint(
            &mut tail,
            DISCARD_PADDING,
            (end_trim as u128 * 1_000_000_000 / u128::from(self.sample_rate)) as i64,
        );
        let block_size = 4 + frame.len() as u64;
        let block_len = header_len(BLOCK, block_size as usize) + block_size;
        put_id(&mut self.block, BLOCK_GROUP);
        put_size(&mut self.block, block_len + tail.len() as u64);
        put_id(&mut self.block, BLOCK);
        put_size(&mut self.block, block_size);
        put_size(&mut self.block, TRACK);
        self.block.extend_from_slice(&offset.to_be_bytes());
        self.block.push(0); // no lacing
        out.write_all(&self.block)?;
        out.write_all(frame)?;
        out.write_all(&tail)
    }

    /// Close the file: the last cluster, the cues, the seek head, and the
    /// sizes and duration left open. `presented` is the samples a channel a
    /// decoder hands out, after its trims. Leaves `out` at the end.
    pub fn finish<W: Write + Seek>(&mut self, out: &mut W, presented: u64) -> io::Result<()> {
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

/// A track UID that follows from what the track carries: FNV-1a over
/// `bytes`, never zero.
pub fn uid(bytes: &[u8]) -> u64 {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    hash.max(1)
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

/// A signed integer in the fewest bytes that hold it, two's complement.
fn put_sint(out: &mut Vec<u8>, id: u32, value: i64) {
    let width = (1..=8)
        .find(|&w| {
            let bits = 8 * w - 1;
            w == 8 || (-(1i64 << bits)..(1i64 << bits)).contains(&value)
        })
        .expect("eight bytes hold any i64");
    put_bytes(out, id, &value.to_be_bytes()[8 - width..]);
}

/// A `Void` element `room` bytes long in all, header included.
fn put_void(out: &mut Vec<u8>, room: usize) {
    assert!(room >= 2, "a Void element takes two bytes at least");
    let size = (room - 2) as u64;
    assert!(size_len(size) == 1, "a Void of {room} bytes");
    put_bytes(out, VOID, &vec![0; size as usize]);
}

#[cfg(test)]
mod tests;
