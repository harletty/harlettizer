// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from the `truehd` crate
// (https://github.com/truehdd/truehdd) — Copyright (c) the truehdd authors —
// under the Apache License, Version 2.0, whose text is reproduced in
// LICENSES/Apache-2.0.txt.
// What was taken and what was changed is recorded in docs/provenance.md.

//! The Evolution frame: the container a delivery bitstream carries its
//! metadata payloads in.
//!
//! Both bitstreams use the same frame — ETSI TS 102 366 Annex H calls it the
//! extensible metadata delivery format — and wrap it differently: TrueHD in
//! the extra data block after an access unit's last substream, E-AC-3 behind
//! a sync word in the auxiliary data of a syncframe. The wrapping belongs to
//! each codec crate; the frame is here, so that neither has to depend on the
//! other to write it.
//!
//! ```text
//! ┌─────────┬─────┬──────────────────────────────┬──────┬────────────┬──────────────┐
//! │ version │ key │ payloads: id, configuration, │ id 0 │ protection │ protection   │
//! │ 2 bits  │ 3   │ size, bytes — repeated       │ 5    │ lengths 2+2│ fields       │
//! └─────────┴─────┴──────────────────────────────┴──────┴────────────┴──────────────┘
//! ```
//!
//! What closes the frame is one or two protection fields whose computation
//! the format leaves to the implementation and which a decoder is told it may
//! ignore. Their *lengths* are not free: TrueHD writes an eight-bit primary
//! field and no secondary one, E-AC-3 a 32-bit primary and an eight-bit
//! secondary. So the writer takes the lengths, writes a constant in the
//! fields, and says where they start; filling them is the wrapping's
//! business, since what a digest covers is a property of the wrapping and
//! not of the frame.

use hz_core::bits::BitWriter;
use hz_core::bits_read::{BitReader, Ran};

/// The payload identifier of programme loudness data (TS 102 366 table H.2.3).
pub const LOUDNESS: u32 = 1;
/// The payload identifier of programme information (TS 102 366 table H.2.3).
pub const PROGRAMME_INFORMATION: u32 = 2;
/// The payload identifier of object audio metadata.
pub const OAMD: u32 = 11;
/// The payload identifier of joint object coding parameters.
pub const JOC: u32 = 14;

/// An identifier that escapes into a variable-width extension.
const ID_ESCAPE: u32 = 0x1f;

/// One payload to write: what it is, its bytes, when in the frame of audio
/// it rides with it takes effect, and the rest of its configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct Payload<'a> {
    /// See [`OAMD`] and [`JOC`].
    pub id: u32,
    pub bytes: &'a [u8],
    /// The sample of the audio frame the payload takes effect at, from nought
    /// to one short of the frame's length. Nought is written as no offset at
    /// all, which is what a payload taking effect at the start of the frame
    /// is.
    pub sample_offset: u32,
    pub config: Config,
}

/// What a payload's configuration says besides its sample offset.
///
/// The default is the shortest configuration the syntax allows — nothing
/// stated, and a decoder that does not know the payload discards it — which
/// is what TrueHD's reference streams write. E-AC-3's reference streams say
/// more, and a writer that is to reproduce them has to be able to say it too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Config {
    /// How many samples the payload stands for, at most [`MAX_DURATION`].
    pub duration: Option<u32>,
    /// The group the payload belongs to: payloads of one group go together.
    /// At most [`MAX_GROUP`].
    pub group: Option<u32>,
    /// The eight reserved bits of codec-specific data, when present.
    pub codec_data: Option<u8>,
    /// What a decoder that does not know the payload is to do with it —
    /// `None` is the bit that says discard it, which ends the configuration.
    pub retain: Option<Retain>,
}

/// Groups of eleven bits a duration may take: two, the specification says
/// (TS 102 366 H.2.2.3.4), the second with its continuation bit clear.
const DURATION_GROUPS: u32 = 2;
/// The longest duration two groups hold.
pub const MAX_DURATION: u32 = max_variable(11, DURATION_GROUPS);

/// Groups of two bits a group identifier may take. The specification sets no
/// bound; this one is ours, far above anything a stream uses — every
/// reference stream says group nought — and low enough that a run of
/// continuation bits cannot carry the value out of its word.
const GROUP_GROUPS: u32 = 8;
/// The largest group identifier eight groups hold.
pub const MAX_GROUP: u32 = max_variable(2, GROUP_GROUPS);

/// The largest value `groups` groups of `n` bits hold.
const fn max_variable(n: u32, groups: u32) -> u32 {
    let span = 1u32 << n;
    let mut value = span - 1;
    let mut group = 1;
    while group < groups {
        value = (value + 1) * span + span - 1;
        group += 1;
    }
    value
}

/// The rest of the configuration of a payload a decoder is not to discard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Retain {
    /// Without a sample offset, whether the payload is aligned with the frame
    /// of audio, and if so whether a decoder may create and remove
    /// duplicates of it. Always `None` with a sample offset, which has no
    /// such field.
    pub frame_aligned: Option<(bool, bool)>,
    /// Priority (5 bits) and what processing is allowed (2 bits), present
    /// with a sample offset or a frame-aligned payload and nought otherwise.
    pub priority: u8,
    pub processing: u8,
}

/// The bits a protection field's two-bit length code stands for.
///
/// Zero is reserved for the primary field and means an empty secondary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectionLength {
    Bits8 = 1,
    Bits32 = 2,
    Bits128 = 3,
}

impl ProtectionLength {
    pub fn bits(self) -> usize {
        PROTECTION_BITS[self as usize] as usize
    }
}

/// The two protection fields that close a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Protection {
    pub primary: ProtectionLength,
    pub secondary: Option<ProtectionLength>,
}

impl Protection {
    /// What every TrueHD reference stream closes its frame with.
    pub const TRUEHD: Self = Self {
        primary: ProtectionLength::Bits8,
        secondary: None,
    };

    /// What every E-AC-3 reference stream closes its frame with.
    pub const EAC3: Self = Self {
        primary: ProtectionLength::Bits32,
        secondary: Some(ProtectionLength::Bits8),
    };

    fn field_bits(self) -> usize {
        self.primary.bits() + self.secondary.map_or(0, ProtectionLength::bits)
    }
}

/// Where a written frame's protection fields start, counted in bits from the
/// writer's start, for a wrapping that fills them once what they cover is
/// final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectionAt {
    pub primary: usize,
    /// Where the secondary field starts — straight after the primary one —
    /// when there is one.
    pub secondary: Option<usize>,
}

/// What stands in a protection field before anything fills it, and in a
/// stream written without a key.
///
/// Not zero: a reader that skipped the field would take it for the padding
/// after the frame and say so, where zeroes would let that pass unseen.
pub const PROTECTION_BYTE: u8 = 0x5a;

/// Write one frame carrying `payloads`, in order.
///
/// Version zero, key identifier zero, and per payload its configuration — a
/// sample offset, present only when it is not nought, in eleven bits and a
/// reserved one, then the rest of [`Config`] — its size and its bytes.
///
/// # Panics
/// If a payload's duration exceeds [`MAX_DURATION`] or its group
/// [`MAX_GROUP`]: the field has no way to say it.
///
/// Returns where the protection fields start; they hold
/// [`PROTECTION_BYTE`] until the wrapping overwrites them.
pub fn write(
    writer: &mut BitWriter,
    payloads: &[Payload<'_>],
    protection: Protection,
) -> ProtectionAt {
    writer.put(2, 0); // version
    writer.put(3, 0); // key identifier

    for payload in payloads {
        debug_assert!(payload.id != 0, "identifier zero ends the payload list");
        write_id(writer, payload.id);
        if payload.sample_offset > 0 {
            debug_assert!(payload.sample_offset < 1 << 11);
            writer.put(1, 1);
            writer.put(11, payload.sample_offset);
            writer.put(1, 0); // reserved
        } else {
            writer.put(1, 0);
        }
        write_config(writer, payload.sample_offset > 0, &payload.config);
        writer.put_variable(8, 4, payload.bytes.len() as u32);
        writer.put_bytes(payload.bytes);
    }
    writer.put(5, 0); // no further payload

    writer.put(2, protection.primary as u32);
    writer.put(2, protection.secondary.map_or(0, |length| length as u32));
    let primary = writer.bit_position();
    for _ in 0..protection.primary.bits() / 8 {
        writer.put(8, u32::from(PROTECTION_BYTE));
    }
    let secondary = protection.secondary.map(|length| {
        let at = writer.bit_position();
        for _ in 0..length.bits() / 8 {
            writer.put(8, u32::from(PROTECTION_BYTE));
        }
        at
    });
    ProtectionAt { primary, secondary }
}

fn write_config(writer: &mut BitWriter, offset: bool, config: &Config) {
    writer.put_bit(config.duration.is_some());
    if let Some(duration) = config.duration {
        writer.put_variable(11, DURATION_GROUPS, duration);
    }
    writer.put_bit(config.group.is_some());
    if let Some(group) = config.group {
        writer.put_variable(2, GROUP_GROUPS, group);
    }
    writer.put_bit(config.codec_data.is_some());
    if let Some(data) = config.codec_data {
        writer.put(8, u32::from(data));
    }
    writer.put_bit(config.retain.is_none());
    if let Some(retain) = config.retain {
        debug_assert!(
            !(offset && retain.frame_aligned.is_some()),
            "a payload with a sample offset has no frame alignment to state"
        );
        if !offset {
            writer.put_bit(retain.frame_aligned.is_some());
            if let Some((create, remove)) = retain.frame_aligned {
                writer.put_bit(create);
                writer.put_bit(remove);
            }
        }
        if offset || retain.frame_aligned.is_some() {
            writer.put(5, u32::from(retain.priority));
            writer.put(2, u32::from(retain.processing));
        }
    }
}

fn config_bits(offset: bool, config: &Config) -> usize {
    let mut bits = 4;
    bits += config
        .duration
        .map_or(0, |duration| BitWriter::variable_width(11, duration));
    bits += config
        .group
        .map_or(0, |group| BitWriter::variable_width(2, group));
    bits += config.codec_data.map_or(0, |_| 8);
    if let Some(retain) = config.retain {
        if !offset {
            bits += 1 + retain.frame_aligned.map_or(0, |_| 2);
        }
        if offset || retain.frame_aligned.is_some() {
            bits += 7;
        }
    }
    bits
}

/// Bits [`write`] would write for the same arguments.
pub fn bits(payloads: &[Payload<'_>], protection: Protection) -> usize {
    let payloads: usize = payloads
        .iter()
        .map(|payload| {
            let offset = if payload.sample_offset > 0 { 12 } else { 0 };
            id_bits(payload.id)
                + 1
                + config_bits(payload.sample_offset > 0, &payload.config)
                + offset
                + BitWriter::variable_width(8, payload.bytes.len() as u32)
                + 8 * payload.bytes.len()
        })
        .sum();
    2 + 3 + payloads + 5 + 4 + protection.field_bits()
}

fn write_id(writer: &mut BitWriter, id: u32) {
    if id < ID_ESCAPE {
        writer.put(5, id);
    } else {
        writer.put(5, ID_ESCAPE);
        writer.put_variable(5, 4, id - ID_ESCAPE);
    }
}

fn id_bits(id: u32) -> usize {
    if id < ID_ESCAPE {
        5
    } else {
        5 + BitWriter::variable_width(5, id - ID_ESCAPE)
    }
}

/// A frame as read: its header, its payloads, and its protection fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    pub version: u32,
    pub key: u32,
    pub payloads: Vec<ReadPayload>,
    /// Bits in the primary and secondary protection fields, as their length
    /// codes declare them.
    pub protection: (u32, u32),
    /// The primary field's bytes as written, and the bit, counted from where
    /// the reader started, at which they begin. The secondary field's bytes
    /// follow straight after.
    pub protection_bytes: Vec<u8>,
    pub protection_at: usize,
    pub secondary_bytes: Vec<u8>,
}

/// One payload as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPayload {
    /// See [`OAMD`] and [`JOC`].
    pub id: u32,
    /// Where in the audio frame it takes effect, when it says.
    pub sample_offset: Option<u32>,
    pub config: Config,
    pub bytes: Vec<u8>,
}

/// Bits a protection field's two-bit length code stands for, reserved code
/// included: reading it as no bits is what lets a frame that says the
/// reserved thing still parse to its end.
const PROTECTION_BITS: [u32; 4] = [0, 8, 32, 128];

/// Read one frame from where `reader` stands.
///
/// `None` when it runs off its bytes. A caller reports that rather than
/// failing the audio beside it. A version other than nought is read and
/// returned rather than refused: what follows it is the same syntax.
pub fn read(reader: &mut BitReader<'_>) -> Option<Frame> {
    let start = reader.bit_position();
    let mut version = reader.get(2).ok()?;
    if version == 3 {
        version += read_variable(reader, 2, 4).ok()?;
    }
    let mut key = reader.get(3).ok()?;
    if key == 7 {
        key += read_variable(reader, 3, 4).ok()?;
    }
    let mut payloads = Vec::new();
    loop {
        let mut id = reader.get(5).ok()?;
        if id == 0 {
            break;
        }
        if id == ID_ESCAPE {
            id += read_variable(reader, 5, 4).ok()?;
        }
        payloads.push(payload(reader, id).ok()?);
    }
    let primary = PROTECTION_BITS[reader.get(2).ok()? as usize];
    let secondary = PROTECTION_BITS[reader.get(2).ok()? as usize];
    let protection_at = reader.bit_position() - start;
    let protection_bytes = read_bytes(reader, (primary / 8) as usize).ok()?;
    let secondary_bytes = read_bytes(reader, (secondary / 8) as usize).ok()?;
    Some(Frame {
        version,
        key,
        payloads,
        protection: (primary, secondary),
        protection_bytes,
        protection_at,
        secondary_bytes,
    })
}

/// The word that opens a frame where nothing around it says one is there —
/// the auxiliary data of an E-AC-3 syncframe.
pub const SYNC_WORD: u16 = 0x5838;

/// A frame behind its sync word and byte length, as E-AC-3 carries it, read
/// from byte `at` of `bytes`.
///
/// Returns the frame and the byte after the length the header declares —
/// where the next sync word may stand. `None` when there is no sync word at
/// `at`, the declared length overruns `bytes`, or the frame does not parse
/// inside it.
pub fn read_synced(bytes: &[u8], at: usize) -> Option<(Frame, usize)> {
    let header = bytes.get(at..at + 4)?;
    if u16::from_be_bytes([header[0], header[1]]) != SYNC_WORD {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let body = bytes.get(at + 4..at + 4 + length)?;
    let frame = read(&mut BitReader::new(body))?;
    Some((frame, at + 4 + length))
}

/// One payload: when it takes effect, and its bytes.
fn payload(reader: &mut BitReader<'_>, id: u32) -> Result<ReadPayload, Ran> {
    let sample_offset = if reader.get_bit()? {
        let offset = reader.get(11)?;
        reader.skip(1)?; // reserved
        Some(offset)
    } else {
        None
    };
    let mut config = Config::default();
    if reader.get_bit()? {
        config.duration = Some(read_variable(reader, 11, DURATION_GROUPS)?);
    }
    if reader.get_bit()? {
        config.group = Some(read_variable(reader, 2, GROUP_GROUPS)?);
    }
    if reader.get_bit()? {
        config.codec_data = Some(reader.get(8)? as u8);
    }
    // The bit says a decoder that does not know this payload should discard
    // it, and setting it ends the configuration there.
    if !reader.get_bit()? {
        let mut retain = Retain::default();
        if sample_offset.is_none() && reader.get_bit()? {
            retain.frame_aligned = Some((reader.get_bit()?, reader.get_bit()?));
        }
        if sample_offset.is_some() || retain.frame_aligned.is_some() {
            retain.priority = reader.get(5)? as u8;
            retain.processing = reader.get(2)? as u8;
        }
        config.retain = Some(retain);
    }

    let size = read_variable(reader, 8, 4)? as usize;
    Ok(ReadPayload {
        id,
        sample_offset,
        config,
        bytes: read_bytes(reader, size)?,
    })
}

fn read_bytes(reader: &mut BitReader<'_>, count: usize) -> Result<Vec<u8>, Ran> {
    let mut bytes = Vec::with_capacity(count);
    for _ in 0..count {
        bytes.push(reader.get(8)? as u8);
    }
    Ok(bytes)
}

/// A value in groups of `n` bits, each followed by a bit saying whether
/// another group follows — and a continuation means the value so far was one
/// short of the group's span.
///
/// At most `groups` groups: a last group that still asks for another is a
/// malformed field and refused, as is a value its word cannot hold — rather
/// than read on into the bits after it, or carried out of the word.
fn read_variable(reader: &mut BitReader<'_>, n: u32, groups: u32) -> Result<u32, Ran> {
    let mut value = 0u64;
    for group in 1..=groups {
        value += u64::from(reader.get(n)?);
        if !reader.get_bit()? {
            return u32::try_from(value)
                .map_err(|_| Ran(format!("a variable-width field of {value} overflows")));
        }
        if group == groups {
            break;
        }
        value = (value + 1) << n;
    }
    Err(Ran(format!(
        "a variable-width field asks for more than {groups} groups of {n} bits"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(payloads: &[Payload<'_>], protection: Protection) -> (Vec<u8>, usize, ProtectionAt) {
        let mut writer = BitWriter::with_capacity(256);
        let at = write(&mut writer, payloads, protection);
        let bits = writer.bit_position();
        (writer.finish(), bits, at)
    }

    /// What is written reads back as itself, its length is the one [`bits`]
    /// predicts, and the protection fields are where the writer says — for
    /// both wrappings' lengths, several payloads, an offset and a size on
    /// each side of the boundary where a length needs a second group.
    #[test]
    fn a_frame_reads_back_as_itself() {
        let oamd: Vec<u8> = (0..65u8).collect();
        let joc: Vec<u8> = (0..300u32).map(|n| (n * 13) as u8).collect();
        let payloads = [
            Payload {
                id: OAMD,
                bytes: &oamd,
                sample_offset: 0,
                config: Config::default(),
            },
            Payload {
                id: JOC,
                bytes: &joc,
                sample_offset: 5,
                config: Config {
                    group: Some(3),
                    retain: Some(Retain {
                        frame_aligned: None,
                        priority: 7,
                        processing: 2,
                    }),
                    ..Config::default()
                },
            },
            Payload {
                id: 40,
                bytes: &[0xaa],
                sample_offset: 1535,
                config: Config {
                    duration: Some(3000),
                    codec_data: Some(0x81),
                    ..Config::default()
                },
            },
            Payload {
                id: OAMD,
                bytes: &[0x55; 2],
                sample_offset: 0,
                config: Config {
                    group: Some(9),
                    retain: Some(Retain {
                        frame_aligned: Some((true, false)),
                        priority: 31,
                        processing: 1,
                    }),
                    ..Config::default()
                },
            },
        ];
        for protection in [Protection::TRUEHD, Protection::EAC3] {
            let (bytes, length, at) = written(&payloads, protection);
            assert_eq!(length, bits(&payloads, protection), "{protection:?}");

            let frame = read(&mut BitReader::new(&bytes)).expect("parses");
            assert_eq!((frame.version, frame.key), (0, 0));
            assert_eq!(frame.payloads.len(), 4);
            for (read, written) in frame.payloads.iter().zip(&payloads) {
                assert_eq!(read.id, written.id);
                assert_eq!(read.bytes, written.bytes);
                assert_eq!(read.config, written.config);
                assert_eq!(
                    read.sample_offset,
                    (written.sample_offset > 0).then_some(written.sample_offset)
                );
            }
            assert_eq!(frame.protection_at, at.primary);
            assert_eq!(
                frame.protection,
                (
                    protection.primary.bits() as u32,
                    protection.secondary.map_or(0, |l| l.bits() as u32)
                )
            );
            assert!(frame.protection_bytes.iter().all(|b| *b == PROTECTION_BYTE));
            assert!(frame.secondary_bytes.iter().all(|b| *b == PROTECTION_BYTE));
            assert_eq!(
                at.secondary,
                protection
                    .secondary
                    .map(|_| at.primary + protection.primary.bits())
            );
        }
    }

    /// The bounds the configuration's variable fields take: the largest
    /// duration and group the syntax allows come back, and a field that asks
    /// for one group more is refused rather than read on or overflowed.
    #[test]
    fn variable_fields_are_held_to_their_groups() {
        assert_eq!(MAX_DURATION, 4_196_351);
        assert_eq!(MAX_GROUP, 87_379);
        let payloads = [Payload {
            id: LOUDNESS,
            bytes: &[1, 2],
            sample_offset: 0,
            config: Config {
                duration: Some(MAX_DURATION),
                group: Some(MAX_GROUP),
                ..Config::default()
            },
        }];
        let (bytes, length, _) = written(&payloads, Protection::EAC3);
        assert_eq!(length, bits(&payloads, Protection::EAC3));
        let frame = read(&mut BitReader::new(&bytes)).expect("parses");
        assert_eq!(frame.payloads[0].config, payloads[0].config);

        // Version, key, payload one, no offset, a duration whose groups all
        // ask for another: refused, not a panic.
        let mut writer = BitWriter::new();
        writer.put(10, 1);
        writer.put(1, 0);
        writer.put(1, 1);
        for group in [1022, 2046, 2047] {
            writer.put(11, group);
            writer.put(1, 1);
        }
        writer.put_zeroes(16);
        assert!(read(&mut BitReader::new(&writer.finish())).is_none());

        // A group identifier that runs past its eight groups.
        let mut writer = BitWriter::new();
        writer.put(10, 1);
        writer.put(2, 0b01); // no offset, no duration, a group
        for _ in 0..GROUP_GROUPS {
            writer.put(3, 0b111);
        }
        writer.put_zeroes(16);
        assert!(read(&mut BitReader::new(&writer.finish())).is_none());

        // A payload size whose four groups of eight carry it past 32 bits.
        let mut writer = BitWriter::new();
        writer.put(10, 1);
        writer.put(5, 0b00001); // nothing stated, discard if unknown
        for group in 0..4 {
            writer.put(8, 0xff);
            writer.put(1, u32::from(group < 3));
        }
        writer.put_zeroes(16);
        assert!(read(&mut BitReader::new(&writer.finish())).is_none());
    }

    #[test]
    #[should_panic]
    fn a_duration_the_field_cannot_hold_is_not_written() {
        let payloads = [Payload {
            id: LOUDNESS,
            config: Config {
                duration: Some(MAX_DURATION + 1),
                ..Config::default()
            },
            ..Payload::default()
        }];
        written(&payloads, Protection::EAC3);
    }

    /// Behind its sync word and length, a frame reads the same, and the next
    /// one starts at the length the header declared, padding included.
    #[test]
    fn a_synced_frame_reads_back_and_says_where_the_next_starts() {
        let joc = [1u8, 2, 3, 4, 5];
        let payloads = [Payload {
            id: JOC,
            bytes: &joc,
            sample_offset: 0,
            config: Config::default(),
        }];
        let (frame, _, _) = written(&payloads, Protection::EAC3);
        let length = frame.len() + 3; // a little padding
        let mut bytes = vec![0xff, 0x00];
        bytes.extend_from_slice(&SYNC_WORD.to_be_bytes());
        bytes.extend_from_slice(&(length as u16).to_be_bytes());
        bytes.extend_from_slice(&frame);
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.push(0x77);

        assert!(read_synced(&bytes, 0).is_none(), "no sync word there");
        let (read, next) = read_synced(&bytes, 2).expect("a frame");
        assert_eq!(read.payloads[0].bytes, joc);
        assert_eq!(read.protection, (32, 8));
        assert_eq!(bytes[next], 0x77);
        assert!(
            read_synced(&bytes[..bytes.len() - 2], 2).is_none(),
            "it overruns"
        );
    }

    /// The frame TrueHD carries for a sixteen-element programme: one object
    /// metadata payload at the start of the unit, and an eight-bit field. It
    /// is the Evolution frame of `hz-mlp`'s pinned extra data block, from bit
    /// 32 to the end of its field, so a change here that moved a bit would
    /// change every TrueHD stream this project writes.
    #[test]
    fn the_truehd_frame_is_the_pinned_one() {
        let oamd = [0x1f, 0x84, 0x4b];
        let (bytes, length, at) = written(
            &[Payload {
                id: OAMD,
                bytes: &oamd,
                sample_offset: 0,
                config: Config::default(),
            }],
            Protection::TRUEHD,
        );
        // Version and key, identifier eleven, five bits of configuration, a
        // size of three, the bytes, the end of the list, two length codes and
        // the field.
        assert_eq!(length, 5 + 5 + 5 + 9 + 24 + 5 + 4 + 8);
        assert_eq!(at.primary, length - 8);
        assert_eq!(
            bytes[0], 0b0000_0010,
            "version, key, and the identifier's top bits"
        );
        assert_eq!(bytes[1] >> 6, 0b11, "the rest of eleven");
    }
}
