// SPDX-License-Identifier: GPL-3.0-or-later

//! The DTS:X extension's standard profile: a fixed 7.1.4.
//!
//! There is no published specification of the extension. Everything here
//! comes from measurements of streams — the `dca` decoder's notes, and this
//! project's own `cargo xtask dts` on the four standard-profile reference
//! streams to hand — and a field whose meaning those measurements do not
//! establish is written at the one value the streams carry, named as such,
//! and refused by the reader at any other (see `LEGAL.md`).
//!
//! The profile is a lossless 7.1 bed into which each of four heights has
//! been folded into the corner below it at −3 dB, and, at the end of the
//! lossless frame, an extension block that carries the heights themselves:
//!
//! ```text
//! the fold matrix       19 bytes: a type-2 layout element, rows of gain codes
//! its checksum           2 bytes: CRC16 over the matrix
//! one byte               2, its meaning not established
//! the height set        a bare four-channel lossless channel set: its
//!                       sub-header, its navigation table, its data
//! a trailer              2 zero bytes, then zeroes to the 32-bit word
//! ```
//!
//! The first 22 bytes are the same in every frame of every standard-profile
//! stream to hand, and decoders find the block by its first four, `0x02000850`
//! ([`SYNC`]); they are written here from their fields ([`FoldMatrix`]),
//! never pasted. The block starts at the first 32-bit word after the bed's
//! band data and runs to the end of the lossless frame; the navigation word
//! in the asset descriptor ([`crate::navigation`]) points at it.
//!
//! A decoder of the bed alone plays the composite 7.1 — the heights in the
//! corners, as a 7.1 receiver should play them. A DTS:X decoder takes each
//! height back out of its corner exactly as it went in, `(h · q + 2^14) >>
//! 15` with `q` the row's gain in Q15 ([`fold`]), and plays it above. The
//! bed is lossless and so is the height set, so the twelve channels before
//! the fold come back bit for bit — provided the composite fits the bed's 24
//! bits, which is the encoder's to see to ([`crate::lossless::Refused::Fold`]).

use crate::bits::Bits;
use crate::crc;
use crate::exss::channels_for_mask;
use crate::fields::Out;
use crate::xll::{self, BareSet, CommonHeader};
use hz_core::bits_read::{BitReader, Ran};

type Result<T> = std::result::Result<T, Ran>;

/// The 32 bits an extension block of the standard profile starts with, by
/// which decoders find it. Not a field of its own: it is the fold matrix's
/// element type, its zero header byte and the first of its pinned fields, as
/// [`FoldMatrix::standard`] writes them.
pub const SYNC: u32 = 0x0200_0850;

/// The bed's speakers: C, L/R, LFE, Lsr/Rsr, Lss/Rss — the lossless layer's
/// 7.1.
pub const REFERENCE_MASK: u32 = 0x084b;
/// The heights, Lh/Rh and Lhr/Rhr: TFL, TFR, TBL, TBR in the height set's
/// order.
pub const HEIGHT_MASK: u32 = 0x8020;

/// The gain code the standard profile folds each height at: −3 dB.
pub const MINUS_3_DB: u8 = 55;
/// Unity, the code the matrix states its scale and the element its level at.
pub const UNITY: u8 = 61;

/// The bed's columns, in the order of [`REFERENCE_MASK`]'s bits, as the
/// matrix's rows address them: C, L, R, LFE, Lsr, Rsr, Lss, Rss.
pub mod column {
    pub const C: u8 = 1 << 0;
    pub const L: u8 = 1 << 1;
    pub const R: u8 = 1 << 2;
    pub const LFE: u8 = 1 << 3;
    pub const LSR: u8 = 1 << 4;
    pub const RSR: u8 = 1 << 5;
    pub const LSS: u8 = 1 << 6;
    pub const RSS: u8 = 1 << 7;
}

/// A six-bit gain code's value in Q15: half a decibel a code below unity at
/// 61, the lossless layer's downmix table at `4 · code − 3` — 55 is 23170,
/// 61 is 32768. `None` for 0 and for 62 and 63, which no stream has been
/// seen to use.
pub fn gain(code: u8) -> Option<i32> {
    (1..=UNITY)
        .contains(&code)
        .then(|| i32::from(xll::DMIX_TABLE[4 * usize::from(code) - 3]))
}

/// `(a · q + 2^14) >> 15`: a height's share of its corner at the Q15 gain
/// `q`, rounded as the lossless decoder rounds its own downmix.
pub fn fold(height: i32, q: i32) -> i32 {
    ((i64::from(height) * i64::from(q) + (1 << 14)) >> 15) as i32
}

/// One row of the fold: which of the bed's columns a height goes into, and
/// at what gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// A bit per column of the bed, in [`column`]'s order.
    pub mask: u8,
    /// A gain code per set bit of `mask`, in bit order.
    pub codes: Vec<u8>,
}

/// The fold matrix: a type-2 layout element saying which of the output's
/// channels the bed already carries and what of every other was folded into
/// it, with its checksum and the byte after it.
///
/// Only the form the lossless (XLL) carrier uses is modelled. The fields the
/// streams never vary are constants here, written as the streams carry them
/// and refused by [`FoldMatrix::read`] at any other value, since what another
/// value would mean is not known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldMatrix {
    /// The bed's speakers when stated, with the code of their mask's width
    /// (`4 · (code + 1)` bits); `None` where the element leaves them to be
    /// inherited, which the standard profile does not.
    pub reference: Option<(u8, u32)>,
    /// The output's level as a gain code, when stated: [`UNITY`] in every
    /// stream.
    pub level: Option<u8>,
    /// The output's speakers — the bed's and the heights' — and their mask's
    /// width code.
    pub output: (u8, u32),
    /// The matrix's scale, a gain code: [`UNITY`] in every stream.
    pub scale: u8,
    /// One row per channel the output has beyond the bed, in mask order.
    pub rows: Vec<Row>,
    /// The byte alignment before the checksum.
    pub padding: Bits,
    /// The checksum as read; a written one is computed.
    pub crc: u16,
    /// The byte after the checksum: 2 in every frame of every
    /// standard-profile stream, its meaning not established.
    pub next: u8,
}

/// The element type of the layout element the standard profile's matrix is.
const ELEMENT_TYPE: u32 = 2;
/// Fields that hold one value in every stream, by width: the element's
/// header byte, a flag, two four-bit fields (the second 0 on a type-2
/// element), the flags of an explicit mask (a zero, a one, two zeroes, three
/// zeroes), the additional-level flag, and the matrix header's five- and
/// six-bit fields, its optional-parameter flag, its inline flag and its
/// additional-scale flag.
const HEADER_BYTE: u32 = 0;
const HEADER_FLAG: u32 = 0;
const FIELD_A: u32 = 1;
const FIELD_B: u32 = 0;
const MATRIX_FIELD: u32 = 2;
const MATRIX_VALUE: u32 = 0;

impl Row {
    /// A height folded into one column at one gain.
    pub fn single(column: u8, code: u8) -> Self {
        Self {
            mask: column,
            codes: vec![code],
        }
    }
}

impl FoldMatrix {
    /// The standard profile's matrix, as every frame of the reference
    /// streams carries it: the 7.1 bed stated explicitly, unity level and
    /// scale, the four heights, and each height into the corner below it at
    /// −3 dB — TFL into L, TFR into R, TBL into Lsr, TBR into Rsr.
    pub fn standard() -> Self {
        Self {
            reference: Some((2, REFERENCE_MASK)),
            level: Some(UNITY),
            output: (3, REFERENCE_MASK | HEIGHT_MASK),
            scale: UNITY,
            rows: [column::L, column::R, column::LSR, column::RSR]
                .map(|c| Row::single(c, MINUS_3_DB))
                .to_vec(),
            padding: Bits::new(),
            crc: 0,
            next: 2,
        }
    }

    /// The bed's speakers, stated or inherited (the inherited ones being the
    /// 7.1).
    pub fn reference_mask(&self) -> u32 {
        self.reference.map_or(REFERENCE_MASK, |(_, mask)| mask)
    }

    /// Columns of the bed, which is the width of a row's mask.
    fn columns(&self) -> u32 {
        channels_for_mask(self.reference_mask())
    }

    /// Rows the matrix has: a channel of the output that the bed does not
    /// carry.
    fn row_count(&self) -> usize {
        channels_for_mask(self.output.1 & !self.reference_mask()) as usize
    }

    /// The Q15 gain at which each height goes into each of the bed's
    /// columns: `[height][column]`, 0 where it does not.
    pub fn gains(&self) -> Result<Vec<[i32; 8]>> {
        if self.columns() != 8 {
            return Err(Ran(format!(
                "a bed of {} columns: the fold is over the 7.1's eight",
                self.columns()
            )));
        }
        self.rows
            .iter()
            .map(|row| {
                let mut gains = [0; 8];
                let mut codes = row.codes.iter();
                for (column, g) in gains.iter_mut().enumerate() {
                    if row.mask & 1 << column != 0 {
                        let &code = codes.next().ok_or_else(|| {
                            Ran("a row with fewer codes than columns".to_string())
                        })?;
                        *g = gain(code)
                            .ok_or_else(|| Ran(format!("gain code {code} has no known gain")))?;
                    }
                }
                Ok(gains)
            })
            .collect()
    }

    pub fn read(bytes: &[u8]) -> Result<Self> {
        let mut r = BitReader::new(bytes);
        let expect = |r: &mut BitReader, bits: u32, want: u32, what: &str| -> Result<()> {
            let got = r.get(bits)?;
            if got == want {
                Ok(())
            } else {
                Err(Ran(format!(
                    "{what} is {got}, where every stream measured has {want}: a form not modelled"
                )))
            }
        };
        expect(&mut r, 8, ELEMENT_TYPE, "the element type")?;
        expect(&mut r, 8, HEADER_BYTE, "the element's header byte")?;
        expect(&mut r, 1, HEADER_FLAG, "the element's header flag")?;
        expect(&mut r, 4, FIELD_A, "the element's first field")?;
        expect(&mut r, 4, FIELD_B, "the element's second field")?;
        let reference = if r.get_bit()? {
            expect(&mut r, 1, 0, "the reference flag")?;
            expect(&mut r, 1, 1, "the explicit reference mask flag")?;
            expect(&mut r, 2, 0, "the reference mode")?;
            expect(&mut r, 3, 0, "the reference field")?;
            let code = r.get(3)? as u8;
            Some((code, r.get(4 * (u32::from(code) + 1))?))
        } else {
            expect(&mut r, 1, 0, "the inherited reference flag")?;
            expect(&mut r, 4, 0, "the inherited reference field")?;
            None
        };
        let level = if r.get_bit()? {
            let code = r.get(6)? as u8;
            expect(&mut r, 1, 0, "the additional level flag")?;
            Some(code)
        } else {
            None
        };
        let code = r.get(3)? as u8;
        let output = (code, r.get(4 * (u32::from(code) + 1))?);
        expect(&mut r, 5, MATRIX_FIELD, "the matrix header's first field")?;
        expect(&mut r, 6, MATRIX_VALUE, "the matrix header's second field")?;
        expect(&mut r, 1, 1, "the lossless carrier's flag")?;
        expect(&mut r, 1, 0, "the optional matrix parameters flag")?;
        expect(&mut r, 1, 1, "the inline matrix flag")?;
        let scale = r.get(6)? as u8;
        expect(&mut r, 1, 0, "the additional scale flag")?;
        let mut matrix = Self {
            reference,
            level,
            output,
            scale,
            rows: Vec::new(),
            padding: Bits::new(),
            crc: 0,
            next: 0,
        };
        let columns = matrix.columns();
        if columns > 8 {
            return Err(Ran(format!("a bed of {columns} columns")));
        }
        for _ in 0..matrix.row_count() {
            let mask = r.get(columns)? as u8;
            let codes = (0..mask.count_ones())
                .map(|_| r.get(6).map(|c| c as u8))
                .collect::<Result<_>>()?;
            matrix.rows.push(Row { mask, codes });
        }
        let align = (8 - r.bit_position() % 8) % 8;
        matrix.padding = Bits::read(&mut r, align)?;
        matrix.crc = r.get(16)? as u16;
        matrix.next = r.get(8)? as u8;
        Ok(matrix)
    }

    fn write_fields(&self, out: &mut Out) {
        out.field(8, ELEMENT_TYPE, "the element type");
        out.field(8, HEADER_BYTE, "the element's header byte");
        out.field(1, HEADER_FLAG, "the element's header flag");
        out.field(4, FIELD_A, "the element's first field");
        out.field(4, FIELD_B, "the element's second field");
        match self.reference {
            Some((code, mask)) => {
                out.bit(true);
                out.bit(false);
                out.bit(true);
                out.field(5, 0u8, "the reference mode and field");
                out.field(3, code, "the reference mask's width");
                out.field(4 * (u32::from(code) + 1), mask, "the reference mask");
            }
            None => {
                out.bit(false);
                out.field(5, 0u8, "the inherited reference fields");
            }
        }
        out.option(&self.level, |out, &code| {
            out.field(6, code, "the level");
            out.bit(false);
        });
        out.field(3, self.output.0, "the output mask's width");
        out.field(
            4 * (u32::from(self.output.0) + 1),
            self.output.1,
            "the output mask",
        );
        out.field(5, MATRIX_FIELD, "the matrix header's first field");
        out.field(6, MATRIX_VALUE, "the matrix header's second field");
        out.bit(true);
        out.bit(false);
        out.bit(true);
        out.field(6, self.scale, "the matrix scale");
        out.bit(false);
        let columns = self.columns();
        out.check(columns <= 8, || format!("a bed of {columns} columns"));
        out.check(self.rows.len() == self.row_count(), || {
            format!(
                "{} rows where the output has {} channels beyond the bed",
                self.rows.len(),
                self.row_count()
            )
        });
        for row in &self.rows {
            out.field(columns, row.mask, "a row's columns");
            out.check(row.codes.len() == row.mask.count_ones() as usize, || {
                "a gain code per column of a row".into()
            });
            for &code in &row.codes {
                out.field(6, code, "a gain code");
            }
        }
        out.bits(&self.padding);
    }

    /// Bytes from the element's start to the byte after its checksum.
    pub fn len(&self) -> usize {
        let mut out = Out::new();
        self.write_fields(&mut out);
        out.position().div_ceil(8) + 3
    }

    /// Never empty; for the lint.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Whether the checksum stored in `bytes`, whose matrix this is, holds.
    pub fn crc_is_valid(&self, bytes: &[u8]) -> bool {
        bytes.get(..self.len() - 1).is_some_and(crc::check)
    }

    /// Write the element, its checksum computed over it, and the byte after.
    /// Refuses one whose fields do not end on a byte.
    pub fn write(&self) -> Result<Vec<u8>> {
        let mut out = Out::new();
        self.write_fields(&mut out);
        if !out.position().is_multiple_of(8) {
            let at = out.position();
            out.refuse(|| format!("the matrix's fields end at bit {at}, not on a byte"));
        }
        let mut w = out.finish()?;
        let crc = crc::crc16(w.bytes());
        w.put(16, u32::from(crc));
        w.put(8, u32::from(self.next));
        Ok(w.finish())
    }

    /// Pad the fields to the byte before the checksum.
    pub fn fit(&mut self) {
        self.padding = Bits::new();
        let mut out = Out::new();
        self.write_fields(&mut out);
        self.padding = Bits::zeroes((8 - out.position() % 8) % 8);
    }
}

/// Zero bytes between the height set's data and the word padding.
pub const TRAILER: usize = 2;

/// The headers of an extension block: the fold matrix and the height set's
/// sub-header and navigation table. What follows them is the set's data,
/// [`TRAILER`] zero bytes and zeroes to the 32-bit word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub matrix: FoldMatrix,
    pub heights: BareSet,
}

impl Block {
    /// Read the headers of the block at the start of `bytes`, which sits in
    /// the lossless frame `common` heads.
    pub fn read(bytes: &[u8], common: &CommonHeader) -> Result<Self> {
        let matrix = FoldMatrix::read(bytes)?;
        let at = matrix.len();
        let heights = BareSet::read(
            bytes
                .get(at..)
                .ok_or_else(|| Ran("the block ends inside its matrix".into()))?,
            common,
        )?;
        Ok(Self { matrix, heights })
    }

    /// Bytes from the block's start to the height set's data.
    pub fn len(&self, common: &CommonHeader) -> usize {
        self.matrix.len() + self.heights.len(common)
    }

    /// Never empty; for the lint.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The block's size when its data is `data` bytes: the headers, the
    /// data, the trailer and the padding to the 32-bit word.
    pub fn size(&self, common: &CommonHeader, data: usize) -> usize {
        (self.len(common) + data + TRAILER).next_multiple_of(4)
    }

    /// Whether every checksum stored in `bytes`, whose block this is, holds.
    pub fn checksums(&self, bytes: &[u8], common: &CommonHeader) -> bool {
        let at = self.matrix.len();
        self.matrix.crc_is_valid(bytes)
            && bytes
                .get(at..)
                .is_some_and(|b| self.heights.checksums(b, common))
    }

    /// Write the headers, every checksum computed.
    pub fn write(&self, common: &CommonHeader) -> Result<Vec<u8>> {
        let mut bytes = self.matrix.write()?;
        bytes.extend(self.heights.write(common)?);
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 22 bytes every frame of every standard-profile reference stream
    /// starts its block with. Only a test vector: the encoder writes them
    /// from [`FoldMatrix::standard`]'s fields.
    const MEASURED: [u8; 22] = [
        0x02, 0x00, 0x08, 0x50, 0x28, 0x4b, 0xfa, 0x71, 0x0d, 0x62, 0x02, 0xfa, 0x02, 0xdc, 0x13,
        0x71, 0x0d, 0xc8, 0x37, 0x3c, 0xf1, 0x02,
    ];

    #[test]
    fn the_standard_matrix_is_the_measured_prefix() {
        let mut matrix = FoldMatrix::standard();
        matrix.fit();
        let written = matrix.write().unwrap();
        assert_eq!(written, MEASURED);
        assert_eq!(matrix.len(), 22);
        assert_eq!(u32::from_be_bytes(written[..4].try_into().unwrap()), SYNC);
        let read = FoldMatrix::read(&MEASURED).unwrap();
        assert!(read.crc_is_valid(&MEASURED));
        assert_eq!(read.crc, 0x3cf1);
        assert_eq!(
            read,
            FoldMatrix {
                crc: 0x3cf1,
                ..matrix
            }
        );
    }

    #[test]
    fn the_standard_fold_is_minus_3_db_into_each_corner() {
        let gains = FoldMatrix::standard().gains().unwrap();
        assert_eq!(gains.len(), 4);
        // L, R, Lsr, Rsr: columns 1, 2, 4, 5.
        for (height, column) in [1, 2, 4, 5].into_iter().enumerate() {
            for (c, &g) in gains[height].iter().enumerate() {
                assert_eq!(g, if c == column { 23170 } else { 0 });
            }
        }
        assert_eq!(gain(UNITY), Some(32768));
        assert_eq!(gain(0), None);
        assert_eq!(gain(62), None);
        assert_eq!(fold(1 << 23, 23170), 5_931_520);
        assert_eq!(fold(-1, 23170), -1);
        assert_eq!(fold(1, 23170), 1);
    }

    #[test]
    fn a_value_never_seen_is_refused() {
        // The optional matrix parameters flag set: bit 87.
        let mut bytes = MEASURED;
        bytes[10] |= 1;
        assert!(FoldMatrix::read(&bytes).is_err());
        // A changed gain reads, and its checksum no longer holds.
        let mut bytes = MEASURED;
        bytes[13] ^= 0x04;
        let read = FoldMatrix::read(&bytes).unwrap();
        assert!(!read.crc_is_valid(&bytes));
        assert_eq!(read.rows[0].codes, [MINUS_3_DB ^ 1]);
    }
}
