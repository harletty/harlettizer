//! A run of bits kept as it was read.
//!
//! Some stretches of a header are not understood, or are understood by one
//! revision of the specification and not by the decoders in use: the tail of
//! an asset descriptor, the reserved bits before a header's checksum. A
//! rewrite has to carry them unchanged, and an encoder has to be able to say
//! what goes there, so they are held as data rather than skipped.

use hz_core::bits::BitWriter;
use hz_core::bits_read::{BitReader, Ran};

/// A bit string, most significant bit first, of any length.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bits {
    bytes: Vec<u8>,
    len: usize,
}

impl Bits {
    /// No bits.
    pub fn new() -> Self {
        Self::default()
    }

    /// `len` zero bits: the padding a writer puts where it has nothing to say.
    pub fn zeroes(len: usize) -> Self {
        Self {
            bytes: vec![0; len.div_ceil(8)],
            len,
        }
    }

    /// The next `len` bits of `r`.
    pub fn read(r: &mut BitReader, len: usize) -> Result<Self, Ran> {
        let mut bytes = Vec::with_capacity(len.div_ceil(8));
        let mut left = len;
        while left >= 8 {
            bytes.push(r.get(8)? as u8);
            left -= 8;
        }
        if left > 0 {
            bytes.push((r.get(left as u32)? << (8 - left)) as u8);
        }
        Ok(Self { bytes, len })
    }

    pub fn write(&self, w: &mut BitWriter) {
        w.put_bits(&self.bytes, self.len);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bits, left-aligned in whole bytes: the last byte's low bits past
    /// [`Self::len`] are zero.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether every bit is zero, which is what padding is.
    pub fn is_zero(&self) -> bool {
        self.bytes.iter().all(|&b| b == 0)
    }

    /// The 64 bits starting `at` bits in, if there are that many.
    pub fn word64(&self, at: usize) -> Option<u64> {
        if at + 64 > self.len {
            return None;
        }
        let mut r = BitReader::at_bit(&self.bytes, at);
        let high = u64::from(r.get(32).ok()?);
        let low = u64::from(r.get(32).ok()?);
        Some(high << 32 | low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unaligned_run_comes_back_as_it_went() {
        let mut w = BitWriter::new();
        w.put(3, 0b101);
        w.put(13, 0x1abc);
        w.put(7, 0x55);
        let bytes = w.padded();
        let mut r = BitReader::at_bit(&bytes, 3);
        let bits = Bits::read(&mut r, 20).unwrap();
        assert_eq!(bits.len(), 20);
        let mut again = BitWriter::new();
        again.put(3, 0b101);
        bits.write(&mut again);
        assert_eq!(again.padded(), bytes);
    }
}
