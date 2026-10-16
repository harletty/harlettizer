//! Reading a finished file back: its elements, and one track's frames.
//!
//! For the checks — the tests here and in the codec crates, and the harness
//! reading a stream out of an `.mka` — not a demuxer: it takes every size as
//! stated and refuses the unknown one an unfinished file still carries, and
//! a laced block, neither of which [`crate::Muxer`] writes.

use crate::id::*;
use std::fmt;

/// What is wrong with a file, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub String);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Malformed {}

type Result<T> = std::result::Result<T, Malformed>;

/// One element: its id, and where its payload is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Element {
    pub id: u32,
    pub at: usize,
    pub end: usize,
}

/// A variable-length integer at `*at`, which moves past it: an id with its
/// marker kept, a size without it.
pub fn vint(bytes: &[u8], at: &mut usize, keep_marker: bool) -> Result<u64> {
    let first = *bytes
        .get(*at)
        .ok_or_else(|| Malformed(format!("a number past the end, at {at}")))?;
    let width = first.leading_zeros() as usize + 1;
    if width > 8 || *at + width > bytes.len() {
        return Err(Malformed(format!("no number at {at}")));
    }
    let mut value = u64::from(first);
    if !keep_marker {
        value &= (1 << (8 - width)) - 1;
    }
    for i in 1..width {
        value = (value << 8) | u64::from(bytes[*at + i]);
    }
    *at += width;
    Ok(value)
}

/// The elements in `bytes[at..end]`, which they have to fill exactly.
pub fn children(bytes: &[u8], mut at: usize, end: usize) -> Result<Vec<Element>> {
    let mut out = Vec::new();
    while at < end {
        let id = vint(bytes, &mut at, true)? as u32;
        let size_at = at;
        let size = vint(bytes, &mut at, false)?;
        let width = at - size_at;
        if size == (1 << (7 * width)) - 1 {
            return Err(Malformed(format!(
                "element {id:#x} of unknown size at {size_at}: a file never finished"
            )));
        }
        let element_end = at
            .checked_add(size as usize)
            .filter(|&e| e <= end)
            .ok_or_else(|| {
                Malformed(format!("element {id:#x} at {size_at} overruns its parent"))
            })?;
        out.push(Element {
            id,
            at,
            end: element_end,
        });
        at = element_end;
    }
    Ok(out)
}

/// The one element of `id` among `elements`.
pub fn only(elements: &[Element], id: u32) -> Result<Element> {
    let mut found = elements.iter().filter(|e| e.id == id);
    match (found.next(), found.next()) {
        (Some(&e), None) => Ok(e),
        (None, _) => Err(Malformed(format!("no element {id:#x}"))),
        _ => Err(Malformed(format!("more than one element {id:#x}"))),
    }
}

/// An unsigned integer's value.
pub fn uint(bytes: &[u8], e: Element) -> u64 {
    bytes[e.at..e.end]
        .iter()
        .fold(0, |v, &b| (v << 8) | u64::from(b))
}

/// A signed integer's value.
pub fn sint(bytes: &[u8], e: Element) -> i64 {
    let payload = &bytes[e.at..e.end];
    let start = if payload.first().is_some_and(|&b| b & 0x80 != 0) {
        -1i64
    } else {
        0
    };
    payload.iter().fold(start, |v, &b| (v << 8) | i64::from(b))
}

/// A float's value, four bytes or eight.
pub fn float(bytes: &[u8], e: Element) -> Result<f64> {
    let payload = &bytes[e.at..e.end];
    match payload.len() {
        4 => Ok(f64::from(f32::from_be_bytes(payload.try_into().unwrap()))),
        8 => Ok(f64::from_be_bytes(payload.try_into().unwrap())),
        n => Err(Malformed(format!("a float of {n} bytes at {}", e.at))),
    }
}

/// The segment and its children.
pub fn segment(bytes: &[u8]) -> Result<(Element, Vec<Element>)> {
    let top = children(bytes, 0, bytes.len())?;
    if top.first().map(|e| e.id) != Some(EBML) {
        return Err(Malformed("no EBML header".into()));
    }
    let segment = only(&top, SEGMENT)?;
    Ok((segment, children(bytes, segment.at, segment.end)?))
}

/// The track entry of track `number`.
pub fn track(bytes: &[u8], number: u64) -> Result<Vec<Element>> {
    let (_, parts) = segment(bytes)?;
    let tracks = only(&parts, TRACKS)?;
    for entry in children(bytes, tracks.at, tracks.end)? {
        if entry.id != TRACK_ENTRY {
            continue;
        }
        let fields = children(bytes, entry.at, entry.end)?;
        if uint(bytes, only(&fields, TRACK_NUMBER)?) == number {
            return Ok(fields);
        }
    }
    Err(Malformed(format!("no track {number}")))
}

/// One frame of a track, as its block carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block<'a> {
    /// Its timestamp, in the segment's ticks.
    pub ticks: u64,
    /// A `SimpleBlock` flagged key; a block in a group is a key frame when
    /// the group refers to nothing, which every group here does.
    pub key: bool,
    pub frame: &'a [u8],
    /// A group's `BlockDuration`, in ticks.
    pub duration: Option<u64>,
    /// A group's `DiscardPadding`, in nanoseconds.
    pub discard_padding: Option<i64>,
}

/// Every frame of track `number`, in the order the clusters carry them.
pub fn blocks(bytes: &[u8], number: u64) -> Result<Vec<Block<'_>>> {
    let (_, parts) = segment(bytes)?;
    let mut out = Vec::new();
    for cluster in parts.iter().filter(|p| p.id == CLUSTER) {
        let fields = children(bytes, cluster.at, cluster.end)?;
        let start = uint(bytes, only(&fields, CLUSTER_TIMESTAMP)?);
        for field in fields {
            let (block, group) = match field.id {
                SIMPLE_BLOCK => (field, None),
                BLOCK_GROUP => {
                    let group = children(bytes, field.at, field.end)?;
                    (only(&group, BLOCK)?, Some(group))
                }
                _ => continue,
            };
            let mut at = block.at;
            if vint(bytes, &mut at, false)? != number {
                continue;
            }
            if at + 3 > block.end {
                return Err(Malformed(format!("a block cut short at {}", block.at)));
            }
            let offset = i16::from_be_bytes([bytes[at], bytes[at + 1]]);
            let flags = bytes[at + 2];
            if flags & 0x06 != 0 {
                return Err(Malformed(format!("a laced block at {}", block.at)));
            }
            let ticks = start
                .checked_add_signed(i64::from(offset))
                .ok_or_else(|| Malformed(format!("a block before its stream at {}", block.at)))?;
            let (duration, discard_padding, key) = match &group {
                None => (None, None, flags & 0x80 != 0),
                Some(group) => {
                    let one = |id| group.iter().find(|e| e.id == id).copied();
                    (
                        one(BLOCK_DURATION).map(|e| uint(bytes, e)),
                        one(DISCARD_PADDING).map(|e| sint(bytes, e)),
                        // ReferenceBlock: a frame that depends on another.
                        one(0xFB).is_none(),
                    )
                }
            };
            out.push(Block {
                ticks,
                key,
                frame: &bytes[at + 3..block.end],
                duration,
                discard_padding,
            });
        }
    }
    Ok(out)
}
