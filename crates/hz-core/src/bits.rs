//! A big-endian bit writer.
//!
//! MLP is written most-significant-bit first into a byte stream, and almost
//! every field in it is a non-byte-aligned width. Two things it needs that a
//! general bit writer usually does not:
//!
//! - **The bit position has to be readable while writing**, because several of
//!   the format's checksums cover "everything written since here" and the
//!   restart header's checksum covers a range that does not start or end on a
//!   byte boundary.
//! - **Bytes already written have to be readable and patchable**, because the
//!   access unit header carries the total length and a parity nibble over the
//!   substream headers, and neither is known until the payload has been
//!   written.
//!
//! So this owns its buffer and exposes it, rather than wrapping a `Write`.
//!
//! # Writing and counting are the same operation
//!
//! An encoder that chooses between candidates has to price each of them, and
//! the price is exactly what writing it would emit. Two implementations of
//! [`BitSink`] make that identity available rather than merely true: the
//! writer, and a [`BitCounter`] that keeps the position and throws the bits
//! away. A closed form may stand in for the counter in a hot loop, but only
//! with a test that says the two agree.

/// What a bit writer and a bit counter both do.
///
/// Every field-level writer is generic over this, so costing a candidate is
/// writing it into a counter rather than adding up a description of what the
/// writer would have done. The two drift; the counter cannot.
pub trait BitSink {
    /// Write the low `bits` of `value`, most significant first.
    fn put(&mut self, bits: u32, value: u32);
    /// Write a signed value in `bits` bits, two's complement.
    fn put_signed(&mut self, bits: u32, value: i32);
    fn put_bit(&mut self, bit: bool);
    /// Bits written so far, the partial byte included.
    fn bit_position(&self) -> usize;
    /// Pad with zero bits until the position is a multiple of `bits`.
    fn align_to(&mut self, bits: usize);
}

/// Check a signed value fits its field, and return it masked to that width.
///
/// # Panics
/// If it does not, because a field silently truncated is a stream that decodes
/// to the wrong samples rather than one that fails — and a candidate priced
/// through a counter has to fail there too, not later at the writer.
fn signed_bits(bits: u32, value: i32) -> u32 {
    assert!((1..=32).contains(&bits), "a signed field needs 1..=32 bits");
    if bits < 32 {
        let limit = 1i64 << (bits - 1);
        let wide = i64::from(value);
        assert!(
            wide >= -limit && wide < limit,
            "{value} does not fit in {bits} signed bits"
        );
    }
    let mask = if bits == 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    };
    (value as u32) & mask
}

/// A sink that keeps the position and throws the bits away.
///
/// What a candidate costs, priced by writing it. It carries no buffer, so it
/// is a counter in a register and costs a candidate what the writer's own
/// per-bit loop costs — which is why the closed forms that survive alongside
/// it have tests saying they agree.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BitCounter {
    at: usize,
}

impl BitCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bits written into it.
    pub fn bits(&self) -> usize {
        self.at
    }
}

impl BitSink for BitCounter {
    fn put(&mut self, bits: u32, _value: u32) {
        assert!(bits <= 32, "a field of {bits} bits does not fit in u32");
        self.at += bits as usize;
    }

    fn put_signed(&mut self, bits: u32, value: i32) {
        signed_bits(bits, value);
        self.at += bits as usize;
    }

    fn put_bit(&mut self, _bit: bool) {
        self.at += 1;
    }

    fn bit_position(&self) -> usize {
        self.at
    }

    fn align_to(&mut self, bits: usize) {
        debug_assert!(bits > 0);
        let over = self.at % bits;
        if over != 0 {
            self.at += bits - over;
        }
    }
}

impl BitSink for BitWriter {
    fn put(&mut self, bits: u32, value: u32) {
        BitWriter::put(self, bits, value);
    }

    fn put_signed(&mut self, bits: u32, value: i32) {
        BitWriter::put_signed(self, bits, value);
    }

    fn put_bit(&mut self, bit: bool) {
        BitWriter::put_bit(self, bit);
    }

    fn bit_position(&self) -> usize {
        BitWriter::bit_position(self)
    }

    fn align_to(&mut self, bits: usize) {
        BitWriter::align_to(self, bits);
    }
}

/// A bit writer over a growable buffer.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits of `partial` that are filled, `0..8`.
    filled: u32,
    /// The byte being assembled, left-aligned.
    partial: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes),
            filled: 0,
            partial: 0,
        }
    }

    /// Bits written so far, including the partial byte in progress.
    pub fn bit_position(&self) -> usize {
        self.bytes.len() * 8 + self.filled as usize
    }

    /// Whole bytes written so far. A partial byte is not one.
    pub fn byte_position(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_aligned(&self) -> bool {
        self.filled == 0
    }

    /// Write the low `bits` of `value`, most significant first.
    ///
    /// Whole bytes at a time rather than a bit at a time: the partial byte and
    /// the value go into one 64-bit accumulator — at most seven bits held plus
    /// thirty-two written — and whole bytes fall out of the top of it. This is
    /// the innermost loop of the encoder, where every residual is a field of
    /// its own.
    ///
    /// # Panics
    /// If `bits` exceeds 32.
    pub fn put(&mut self, bits: u32, value: u32) {
        assert!(bits <= 32, "a field of {bits} bits does not fit in u32");
        if bits == 0 {
            return;
        }
        let value = if bits == 32 {
            u64::from(value)
        } else {
            u64::from(value) & ((1u64 << bits) - 1)
        };
        let mut accumulator = (u64::from(self.partial) << bits) | value;
        let mut held = self.filled + bits;
        while held >= 8 {
            held -= 8;
            self.bytes.push((accumulator >> held) as u8);
        }
        accumulator &= (1u64 << held) - 1;
        self.partial = accumulator as u8;
        self.filled = held;
    }

    /// Append whole bytes, which is a copy when the writer is on a boundary.
    ///
    /// For carrying a block assembled on its own — a substream, whose
    /// checksums are defined over its own bytes — into the unit that holds it.
    pub fn put_bytes(&mut self, bytes: &[u8]) {
        if self.filled == 0 {
            self.bytes.extend_from_slice(bytes);
            return;
        }
        for byte in bytes {
            self.put(8, u32::from(*byte));
        }
    }

    /// Write a signed value in `bits` bits, two's complement.
    ///
    /// # Panics
    /// If `value` does not fit, because a field silently truncated is a stream
    /// that decodes to the wrong samples rather than one that fails.
    pub fn put_signed(&mut self, bits: u32, value: i32) {
        self.put(bits, signed_bits(bits, value));
    }

    pub fn put_bit(&mut self, bit: bool) {
        self.partial = (self.partial << 1) | u8::from(bit);
        self.filled += 1;
        if self.filled == 8 {
            self.bytes.push(self.partial);
            self.partial = 0;
            self.filled = 0;
        }
    }

    /// Append the first `count` bits of `bytes`, most significant first.
    ///
    /// For carrying a block that had to be written before its own length was
    /// known: it is assembled on its own and copied in once there is somewhere
    /// to put it.
    ///
    /// # Panics
    /// If `bytes` is too short to hold `count` bits.
    pub fn put_bits(&mut self, bytes: &[u8], count: usize) {
        assert!(
            bytes.len() * 8 >= count,
            "{count} bits is more than there are"
        );
        let whole = count / 8;
        for byte in &bytes[..whole] {
            self.put(8, u32::from(*byte));
        }
        let left = count % 8;
        if left > 0 {
            self.put(left as u32, u32::from(bytes[whole] >> (8 - left)));
        }
    }

    /// A value in groups of `n` bits, each followed by a bit saying whether
    /// another group follows.
    ///
    /// The coding these formats use for a length that is usually small and
    /// occasionally is not. A continuation means the value so far was one
    /// short of the group's span, so the groups are worth `2^n` more than they
    /// look — which is what makes it exact rather than merely compact.
    ///
    /// # Panics
    /// If the value needs more than `groups` groups.
    pub fn put_variable(&mut self, n: u32, groups: u32, value: u32) {
        let span = 1u32 << n;
        let mut high = Vec::new();
        let mut rest = value;
        while rest >= span {
            high.push(rest % span);
            rest = rest / span - 1;
        }
        assert!(
            high.len() < groups as usize,
            "{value} does not fit in {groups} groups of {n} bits"
        );
        self.put(n, rest);
        for group in high.iter().rev() {
            self.put(1, 1);
            self.put(n, *group);
        }
        self.put(1, 0);
    }

    /// How many bits [`Self::put_variable`] would write.
    pub fn variable_width(n: u32, value: u32) -> usize {
        let span = 1u32 << n;
        let mut groups = 1;
        let mut rest = value;
        while rest >= span {
            rest = rest / span - 1;
            groups += 1;
        }
        groups * (n as usize + 1)
    }

    pub fn align_to(&mut self, bits: usize) {
        debug_assert!(bits > 0);
        let over = self.bit_position() % bits;
        if over != 0 {
            self.put_zeroes(bits - over);
        }
    }

    pub fn put_zeroes(&mut self, count: usize) {
        for _ in 0..count {
            self.put_bit(false);
        }
    }

    /// Everything written, up to the last whole byte.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Everything written, including the byte in progress padded with zeroes.
    ///
    /// Needed by the restart header's checksum, which covers a bit range that
    /// ends part-way through a byte: the bits of that byte are inside the
    /// range and [`Self::bytes`] does not have them yet. Copies, but it is
    /// called once per access unit over about sixteen bytes.
    pub fn padded(&self) -> Vec<u8> {
        let mut out = self.bytes.clone();
        if self.filled != 0 {
            out.push(self.partial << (8 - self.filled));
        }
        out
    }

    /// Patch bytes already written. Used for the fields — lengths, parities,
    /// checksums — that cannot be known until what they cover exists.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Reserve `count` bytes to be filled in later, and return where they are.
    ///
    /// # Panics
    /// If the writer is not byte-aligned, since a placeholder that starts
    /// mid-byte cannot be patched by index.
    pub fn placeholder(&mut self, count: usize) -> usize {
        assert!(self.is_aligned(), "a placeholder must start on a byte");
        let at = self.bytes.len();
        self.bytes.resize(at + count, 0);
        at
    }

    /// Finish, padding the last byte with zeroes if it is partial.
    pub fn finish(mut self) -> Vec<u8> {
        if self.filled != 0 {
            self.partial <<= 8 - self.filled;
            self.bytes.push(self.partial);
            self.partial = 0;
            self.filled = 0;
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_pack_most_significant_first() {
        let mut writer = BitWriter::new();
        writer.put(4, 0xA);
        writer.put(4, 0x5);
        writer.put(12, 0xF01);
        assert_eq!(writer.finish(), vec![0xA5, 0xF0, 0x10]);
    }

    #[test]
    fn a_partial_byte_is_padded_at_the_end() {
        let mut writer = BitWriter::new();
        writer.put(3, 0b101);
        assert_eq!(writer.byte_position(), 0, "no whole byte yet");
        assert_eq!(writer.bit_position(), 3);
        assert_eq!(writer.finish(), vec![0b1010_0000]);
    }

    #[test]
    fn signed_fields_are_twos_complement() {
        let mut writer = BitWriter::new();
        writer.put_signed(4, -1);
        writer.put_signed(4, -8);
        writer.put_signed(8, 127);
        assert_eq!(writer.finish(), vec![0xF8, 0x7F]);
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn a_signed_field_that_does_not_fit_is_a_bug_not_a_truncation() {
        let mut writer = BitWriter::new();
        writer.put_signed(4, 8);
    }

    #[test]
    fn alignment_pads_to_the_next_boundary() {
        let mut writer = BitWriter::new();
        writer.put(3, 0b111);
        writer.align_to(16);
        assert_eq!(writer.bit_position(), 16);
        assert_eq!(writer.finish(), vec![0b1110_0000, 0]);
    }

    #[test]
    fn padding_exposes_the_byte_in_progress() {
        let mut writer = BitWriter::new();
        writer.put(8, 0xAB);
        writer.put(3, 0b101);
        assert_eq!(writer.bytes(), &[0xAB], "the partial byte is not whole");
        assert_eq!(writer.padded(), vec![0xAB, 0b1010_0000]);
    }

    #[test]
    fn placeholders_can_be_patched_afterwards() {
        let mut writer = BitWriter::new();
        let at = writer.placeholder(2);
        writer.put(16, 0xBEEF);
        let length = writer.byte_position() as u16;
        writer.bytes_mut()[at..at + 2].copy_from_slice(&length.to_be_bytes());
        assert_eq!(writer.finish(), vec![0x00, 0x04, 0xBE, 0xEF]);
    }
}

#[cfg(test)]
mod counter_tests {
    use super::*;

    /// The counter and the writer have to agree about every field, or a
    /// candidate is priced at one width and written at another.
    #[test]
    fn the_counter_agrees_with_the_writer_field_for_field() {
        let mut writer = BitWriter::new();
        let mut counter = BitCounter::new();
        let mut state = 0x9e37_79b9u32;
        for step in 0..2000u32 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            match step % 5 {
                0 => {
                    let bits = 1 + state % 32;
                    let value = state >> (32 - bits);
                    BitSink::put(&mut writer, bits, value);
                    BitSink::put(&mut counter, bits, value);
                }
                1 => {
                    let bits = 1 + state % 31;
                    let value = ((state as i32) >> (32 - bits)) >> 1;
                    BitSink::put_signed(&mut writer, bits, value);
                    BitSink::put_signed(&mut counter, bits, value);
                }
                2 => {
                    BitSink::put_bit(&mut writer, state & 1 == 1);
                    BitSink::put_bit(&mut counter, state & 1 == 1);
                }
                3 => {
                    BitSink::align_to(&mut writer, 8);
                    BitSink::align_to(&mut counter, 8);
                }
                _ => {
                    BitSink::align_to(&mut writer, 16);
                    BitSink::align_to(&mut counter, 16);
                }
            }
            assert_eq!(
                BitSink::bit_position(&writer),
                BitSink::bit_position(&counter),
                "after step {step}"
            );
        }
    }

    /// A field the writer would refuse has to be refused while it is being
    /// priced, or a candidate that cannot be written is chosen and panics.
    #[test]
    #[should_panic(expected = "does not fit")]
    fn the_counter_refuses_what_the_writer_would_refuse() {
        let mut counter = BitCounter::new();
        counter.put_signed(4, 8);
    }
}
