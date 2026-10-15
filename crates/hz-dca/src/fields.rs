//! Field-level reading and writing shared by the headers.
//!
//! The headers of every component are grammars of fixed and conditional
//! fields; these are the two primitives they are read and written through,
//! with the writer refusing what a field cannot hold rather than truncating
//! it.

use crate::bits::Bits;
use hz_core::bits::BitWriter;
use hz_core::bits_read::{BitReader, Ran};

type Result<T> = std::result::Result<T, Ran>;

pub(crate) fn get_option<T>(
    r: &mut BitReader,
    read: impl FnOnce(&mut BitReader) -> Result<T>,
) -> Result<Option<T>> {
    if r.get_bit()? {
        read(r).map(Some)
    } else {
        Ok(None)
    }
}

/// A bit writer that refuses, rather than truncates, a value its field cannot
/// hold. `BitWriter::put` keeps the low bits, which for a size field is a
/// header that navigates somewhere else and still writes. The first refusal
/// is kept and returned by [`Out::finish`], so the writing code can read like
/// the grammar it writes.
pub(crate) struct Out {
    w: BitWriter,
    error: Option<String>,
}

impl Out {
    pub(crate) fn new() -> Self {
        Self {
            w: BitWriter::new(),
            error: None,
        }
    }

    pub(crate) fn refuse(&mut self, why: impl FnOnce() -> String) {
        if self.error.is_none() {
            self.error = Some(why());
        }
    }

    pub(crate) fn check(&mut self, ok: bool, why: impl FnOnce() -> String) {
        if !ok {
            self.refuse(why);
        }
    }

    /// Whether writing goes on. After the first refusal nothing more is
    /// written: a later field's width can depend on the refused one, and
    /// what would be written is going to be thrown away.
    pub(crate) fn writing(&self) -> bool {
        self.error.is_none()
    }

    /// A field coded as it is.
    pub(crate) fn field(&mut self, bits: u32, value: impl Into<u64>, what: &str) {
        let value = value.into();
        if !self.writing() {
            return;
        }
        if bits > 64 {
            self.refuse(|| format!("{what} cannot be {bits} bits wide"));
        } else if bits < 64 && value >> bits != 0 {
            self.refuse(|| format!("{what} = {value} does not fit in {bits} bits"));
        } else {
            put_wide(&mut self.w, bits, value);
        }
    }

    /// A count or size coded as itself less one, so `1..=2^bits`.
    pub(crate) fn size(&mut self, bits: u32, value: impl Into<u64>, what: &str) {
        let value = value.into();
        if !self.writing() {
            return;
        }
        if bits >= 64 {
            self.refuse(|| format!("{what} cannot be {bits} bits wide"));
        } else if value == 0 || value > 1u64 << bits {
            self.refuse(|| format!("{what} = {value} is outside 1..={}", 1u64 << bits));
        } else {
            put_wide(&mut self.w, bits, value - 1);
        }
    }

    pub(crate) fn bit(&mut self, bit: bool) {
        if self.writing() {
            self.w.put_bit(bit);
        }
    }

    pub(crate) fn option<T>(&mut self, value: &Option<T>, write: impl FnOnce(&mut Self, &T)) {
        self.bit(value.is_some());
        if let Some(value) = value {
            write(self, value);
        }
    }

    pub(crate) fn bits(&mut self, bits: &Bits) {
        if self.writing() {
            bits.write(&mut self.w);
        }
    }

    pub(crate) fn position(&self) -> usize {
        self.w.bit_position()
    }

    pub(crate) fn finish(self) -> Result<BitWriter> {
        match self.error {
            Some(why) => Err(Ran(why)),
            None => Ok(self.w),
        }
    }
}

/// Read up to 64 bits.
pub(crate) fn get_wide(r: &mut BitReader, bits: u32) -> Result<u64> {
    if bits > 32 {
        let high = u64::from(r.get(bits - 32)?);
        Ok(high << 32 | u64::from(r.get(32)?))
    } else {
        Ok(u64::from(r.get(bits)?))
    }
}

pub(crate) fn put_wide(w: &mut BitWriter, bits: u32, value: u64) {
    if bits > 32 {
        w.put(bits - 32, (value >> 32) as u32);
        w.put(32, value as u32);
    } else {
        w.put(bits, value as u32);
    }
}
