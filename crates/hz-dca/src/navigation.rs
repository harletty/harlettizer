//! The DTS:X extension's navigation word.
//!
//! A 64-bit word in the tail of the lossless asset's descriptor locates the
//! DTS:X extension block inside the lossless frame. Its layout is read from
//! streams, not from a specification:
//!
//! ```text
//! high: 0x18 (8) | offset / 4 (11) | 13 zero bits
//! low:  0x4514 (15) | (size − 24) / 4 (11) | 6 zero bits
//! ```
//!
//! The size field is eleven bits, not ten: blocks over 4,116 bytes carry the
//! eleventh in what reads, on smaller blocks, as the last bit of a `0x8a28`
//! marker. That was established on the standard-profile streams to hand,
//! whose blocks run to 5,056 bytes (`cargo xtask dts`, every frame matching
//! the block the decoder locates). Whether either field is wider still is
//! not established — no offset over 7,884 bytes and no block over 5,056 has
//! been seen — so, **as it is read today**, the word cannot point past
//! [`MAX_OFFSET`] nor describe a block over [`MAX_SIZE`], and
//! [`Navigation::word`] refuses what does not fit rather than truncate it: a
//! frame it cannot navigate is the encoder's to reject or reshape, never to
//! write wrong.

/// The furthest offset the word can name, in bytes.
pub const MAX_OFFSET: usize = 2047 * 4;
/// The largest block the word can describe, in bytes.
pub const MAX_SIZE: usize = 2047 * 4 + 24;

const HIGH_MARKER: u32 = 0x1800_0000;
const LOW_MARKER: u32 = 0x8a28_0000;
const OFFSET_FIELD: u32 = 0x00ff_e000;
const SIZE_FIELD: u32 = 0x0001_ffc0;

/// Where the extension block sits in the lossless frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Navigation {
    /// Bytes from the start of the lossless frame.
    pub offset: usize,
    /// Bytes in the block.
    pub size: usize,
}

/// Why a block cannot be navigated by the word as it is read today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutOfRange {
    /// The offset is not a multiple of four, or past [`MAX_OFFSET`].
    Offset(usize),
    /// The size is not 24 plus a multiple of four, or over [`MAX_SIZE`].
    Size(usize),
}

impl std::fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutOfRange::Offset(offset) => write!(
                f,
                "an extension block {offset} bytes into the frame cannot be navigated \
                 (a multiple of four up to {MAX_OFFSET})"
            ),
            OutOfRange::Size(size) => write!(
                f,
                "a {size}-byte extension block cannot be navigated \
                 (24 plus a multiple of four, up to {MAX_SIZE})"
            ),
        }
    }
}

impl std::error::Error for OutOfRange {}

impl Navigation {
    /// The navigation a word carries, if it is one.
    pub fn read(word: u64) -> Option<Self> {
        let high = (word >> 32) as u32;
        let low = word as u32;
        if high & !OFFSET_FIELD != HIGH_MARKER || low & !SIZE_FIELD != LOW_MARKER {
            return None;
        }
        Some(Self {
            offset: ((high & OFFSET_FIELD) >> 13) as usize * 4,
            size: ((low & SIZE_FIELD) >> 6) as usize * 4 + 24,
        })
    }

    /// The word for this navigation, or why there is none.
    pub fn word(&self) -> Result<u64, OutOfRange> {
        if !self.offset.is_multiple_of(4) || self.offset > MAX_OFFSET {
            return Err(OutOfRange::Offset(self.offset));
        }
        if self.size < 24 || !(self.size - 24).is_multiple_of(4) || self.size > MAX_SIZE {
            return Err(OutOfRange::Size(self.size));
        }
        let high = HIGH_MARKER | ((self.offset / 4) as u32) << 13;
        let low = LOW_MARKER | (((self.size - 24) / 4) as u32) << 6;
        Ok(u64::from(high) << 32 | u64::from(low))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_navigable_block_round_trips() {
        for offset in (0..=MAX_OFFSET).step_by(4) {
            for size in [24, 28, 1000, MAX_SIZE] {
                let nav = Navigation { offset, size };
                assert_eq!(Navigation::read(nav.word().unwrap()), Some(nav));
            }
        }
    }

    #[test]
    fn what_does_not_fit_is_refused_not_truncated() {
        let past = Navigation {
            offset: MAX_OFFSET + 4,
            size: 24,
        };
        assert_eq!(past.word(), Err(OutOfRange::Offset(MAX_OFFSET + 4)));
        // Four full-coded 24-bit heights of 512 samples fit; a block twice
        // that does not.
        let heights = Navigation {
            offset: 0,
            size: 4 * 512 * 3 + 24,
        };
        assert!(heights.word().is_ok());
        let over = Navigation {
            offset: 0,
            size: MAX_SIZE + 4,
        };
        assert_eq!(over.word(), Err(OutOfRange::Size(MAX_SIZE + 4)));
        let odd = Navigation {
            offset: 2,
            size: 24,
        };
        assert!(odd.word().is_err());
    }

    #[test]
    fn a_block_over_4116_bytes_uses_the_eleventh_size_bit() {
        // A word read from a stream: a 4,128-byte block 6,428 bytes in.
        let word = 0x18c8_e000_8a29_0080u64;
        assert_eq!(
            Navigation::read(word),
            Some(Navigation {
                offset: 6428,
                size: 4128
            })
        );
        assert_eq!(Navigation::read(word).unwrap().word(), Ok(word));
    }

    #[test]
    fn a_word_without_its_markers_is_not_one() {
        let word = Navigation {
            offset: 400,
            size: 1000,
        }
        .word()
        .unwrap();
        assert!(Navigation::read(word ^ 1 << 63).is_none());
        assert!(Navigation::read(word ^ 1).is_none());
    }
}
