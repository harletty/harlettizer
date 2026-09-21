//! A big-endian bit reader, the mirror of [`crate::bits`].
//!
//! An encoder needs a reader for one reason: to be checked against streams it
//! did not write. Every field of this format that was got right was got right
//! by comparing against a real stream, and every field that was got wrong was
//! wrong in a way that parsed cleanly — so the comparison has to be a program
//! rather than a hand-decode that is thrown away afterwards.
//!
//! It is deliberately not a decoder. It reads the *structure* of an access
//! unit — the header, the major sync, the substream directory, the restart
//! headers — and stops at the block data, which is where a decoder's work
//! begins and an encoder's checking ends.

/// Reading past the end of the buffer, or a field that makes no sense.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran(pub String);

impl std::fmt::Display for Ran {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Ran {}

type Result<T> = std::result::Result<T, Ran>;

/// A bit reader over a borrowed buffer.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Start `bits` into the buffer.
    pub fn at_bit(bytes: &'a [u8], bits: usize) -> Self {
        Self { bytes, at: bits }
    }

    pub fn bit_position(&self) -> usize {
        self.at
    }

    pub fn byte_position(&self) -> usize {
        self.at / 8
    }

    pub fn is_aligned(&self) -> bool {
        self.at.is_multiple_of(8)
    }

    pub fn bits_left(&self) -> usize {
        (self.bytes.len() * 8).saturating_sub(self.at)
    }

    /// Read `bits` bits, most significant first.
    ///
    /// # Panics
    /// If `bits` exceeds 32.
    pub fn get(&mut self, bits: u32) -> Result<u32> {
        assert!(bits <= 32, "a field of {bits} bits does not fit in u32");
        if self.bits_left() < bits as usize {
            return Err(Ran(format!(
                "wanted {bits} bits at {}, only {} left",
                self.at,
                self.bits_left()
            )));
        }
        let mut value = 0u32;
        for _ in 0..bits {
            let byte = self.bytes[self.at / 8];
            let bit = (byte >> (7 - (self.at % 8))) & 1;
            value = (value << 1) | u32::from(bit);
            self.at += 1;
        }
        Ok(value)
    }

    /// Read a signed field, two's complement.
    pub fn get_signed(&mut self, bits: u32) -> Result<i32> {
        assert!((1..=32).contains(&bits), "a signed field needs 1..=32 bits");
        let raw = self.get(bits)?;
        Ok(if bits == 32 {
            raw as i32
        } else if raw >> (bits - 1) & 1 == 1 {
            (raw as i32) - (1i32 << bits)
        } else {
            raw as i32
        })
    }

    pub fn get_bit(&mut self) -> Result<bool> {
        Ok(self.get(1)? == 1)
    }

    /// A value in groups of `n` bits, each followed by a bit saying whether
    /// another group follows — the counterpart of
    /// [`BitWriter::put_variable`](crate::bits::BitWriter::put_variable).
    ///
    /// Stops after `groups`, which is the bound the syntax states for each
    /// field that uses this: a stream saying otherwise is one that has already
    /// gone wrong, and reading on would follow it.
    pub fn get_variable(&mut self, n: u32, groups: u32) -> Result<u32> {
        let mut value = 0u32;
        for _ in 0..groups {
            value += self.get(n)?;
            if !self.get_bit()? {
                return Ok(value);
            }
            value = (value + 1) << n;
        }
        Ok(value)
    }

    pub fn skip(&mut self, bits: usize) -> Result<()> {
        if self.bits_left() < bits {
            return Err(Ran(format!(
                "wanted to skip {bits} bits at {}, only {} left",
                self.at,
                self.bits_left()
            )));
        }
        self.at += bits;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;

    /// The reader and the writer have to be inverses, or a stream this project
    /// writes and reads back tells it nothing.
    #[test]
    fn what_the_writer_wrote_is_what_the_reader_reads() {
        let fields: [(u32, u32); 7] = [
            (24, 0x00f8_726f),
            (8, 0xba),
            (4, 0),
            (13, 0x004f),
            (16, 0xb752),
            (1, 1),
            (15, 663),
        ];
        let mut writer = BitWriter::new();
        for (bits, value) in fields {
            writer.put(bits, value);
        }
        let bytes = writer.finish();

        let mut reader = BitReader::new(&bytes);
        for (bits, value) in fields {
            assert_eq!(reader.get(bits).unwrap(), value, "a {bits}-bit field");
        }
    }

    #[test]
    fn signed_fields_survive_the_round_trip() {
        for value in [-256i32, -198, -14, -1, 0, 1, 255] {
            let mut writer = BitWriter::new();
            writer.put_signed(9, value);
            let bytes = writer.finish();
            assert_eq!(BitReader::new(&bytes).get_signed(9).unwrap(), value);
        }
    }

    #[test]
    fn running_off_the_end_is_an_error_not_a_panic() {
        let mut reader = BitReader::new(&[0xff]);
        assert!(reader.get(8).is_ok());
        assert!(reader.get(1).is_err());
    }
}
