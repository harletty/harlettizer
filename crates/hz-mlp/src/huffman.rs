// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/mlp.c — Copyright (c) 2007-2008 Ian Caulfield
//   libavcodec/mlpdec.c — Copyright (c) 2007-2008 Ian Caulfield
// What was taken and what was changed is recorded in docs/provenance.md.

//! The format's three entropy codebooks.
//!
//! # What a coded residual looks like
//!
//! A residual is split in two. Its low `lsb_bits` go on the wire as they are;
//! what is left above them is a small number, and *that* is what a codebook
//! prices — short codes for the values near zero, long ones for the values at
//! the edges. A block whose residuals average nine bits and peak at fifteen
//! pays fifteen for every one of them without this, and about ten with it.
//!
//! The decoder reassembles a sample as:
//!
//! ```text
//! value  = (symbol << lsb_bits) + lsbs
//! sample = value + sign_huff_offset
//! ```
//!
//! where `sign_huff_offset` is not stored but derived — from the codebook, the
//! width, and the offset field — so that the representable range lands around
//! zero. The derivation is [`sign_offset`], and it is not obvious: the term it
//! subtracts depends on a shift that can go negative, in which case it is not
//! subtracted at all.
//!
//! # Three tables, and why they differ
//!
//! All three spend the same codes on values below −1 and above +1, and differ
//! only in what they charge for the values in the middle. Table 2 gives a
//! single bit to one value and nothing short to its neighbours; table 0
//! spreads three-bit codes across four of them. So the choice between them is
//! a choice about how peaked the residual distribution is, and it is made per
//! block by trying all of them.
//!
//! The tables are FFmpeg's `ff_mlp_huffman_tables` (LGPL-2.1-or-later; see
//! `docs/provenance.md`). They are the format's, and the test below checks
//! that what was transcribed is a prefix code, which a mistyped digit would
//! almost certainly not be.

/// A code and its length in bits.
type Code = (u32, u32);

/// Codebook 1: eighteen symbols, three-bit codes across four middle values.
const TABLE_0: [Code; 18] = [
    (0x01, 9),
    (0x01, 8),
    (0x01, 7),
    (0x01, 6),
    (0x01, 5),
    (0x01, 4),
    (0x01, 3),
    (0x04, 3),
    (0x05, 3),
    (0x06, 3),
    (0x07, 3),
    (0x03, 3),
    (0x05, 4),
    (0x09, 5),
    (0x11, 6),
    (0x21, 7),
    (0x41, 8),
    (0x81, 9),
];

/// Codebook 2: sixteen symbols, two-bit codes on the two middle values.
const TABLE_1: [Code; 16] = [
    (0x01, 9),
    (0x01, 8),
    (0x01, 7),
    (0x01, 6),
    (0x01, 5),
    (0x01, 4),
    (0x01, 3),
    (0x02, 2),
    (0x03, 2),
    (0x03, 3),
    (0x05, 4),
    (0x09, 5),
    (0x11, 6),
    (0x21, 7),
    (0x41, 8),
    (0x81, 9),
];

/// Codebook 3: fifteen symbols, one bit on the middle value.
const TABLE_2: [Code; 15] = [
    (0x01, 9),
    (0x01, 8),
    (0x01, 7),
    (0x01, 6),
    (0x01, 5),
    (0x01, 4),
    (0x01, 3),
    (0x01, 1),
    (0x03, 3),
    (0x05, 4),
    (0x09, 5),
    (0x11, 6),
    (0x21, 7),
    (0x41, 8),
    (0x81, 9),
];

/// Which codebook a channel uses. Zero means none: the residual goes on the
/// wire whole, in `lsb_bits` bits.
pub const NONE: u8 = 0;
/// The highest codebook number the two-bit field carries.
pub const MAX_CODEBOOK: u8 = 3;

/// The table for a codebook, or `None` for codebook zero.
pub fn table(codebook: u8) -> Option<&'static [Code]> {
    match codebook {
        1 => Some(&TABLE_0),
        2 => Some(&TABLE_1),
        3 => Some(&TABLE_2),
        _ => None,
    }
}

/// How many symbols a codebook has.
pub fn symbols(codebook: u8) -> usize {
    table(codebook).map_or(0, <[Code]>::len)
}

/// The offset a decoder adds after reassembling a value.
///
/// Ported from the decoder, whose derivation this has to match exactly. The
/// conditional is the part worth reading twice: when `lsb_bits` is small
/// enough and the codebook large enough, `sign_shift` goes negative and the
/// second term is *not* subtracted, which moves the representable range off
/// centre. An implementation that subtracts `1 << sign_shift` unconditionally
/// agrees with this one everywhere except there.
pub fn sign_offset(codebook: u8, lsb_bits: u32, huff_offset: i32) -> i64 {
    let mut offset = i64::from(huff_offset);
    let sign_shift = lsb_bits as i64
        + if codebook > 0 {
            2 - i64::from(codebook)
        } else {
            -1
        };

    if codebook > 0 {
        offset -= 7i64 << lsb_bits;
    }
    if sign_shift >= 0 {
        offset -= 1i64 << sign_shift;
    }
    offset
}

/// The residuals a coding can represent, inclusive at both ends.
///
/// Computed rather than discovered: the search below needs to know, for each
/// codebook, the narrowest field that holds a block — and asking that of the
/// block itself, one width at a time, costs a pass over the samples for every
/// width tried.
pub fn range(codebook: u8, lsb_bits: u32, huff_offset: i32) -> (i64, i64) {
    let low = sign_offset(codebook, lsb_bits, huff_offset);
    let span = match table(codebook) {
        Some(table) => (table.len() as i64) << lsb_bits,
        None => 1i64 << lsb_bits,
    };
    (low, low + span - 1)
}

/// How a single residual is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// The symbol to look up, or `None` when there is no codebook.
    pub symbol: Option<usize>,
    /// The low bits, written raw.
    pub lsbs: u32,
}

/// Split a residual, or `None` if this coding cannot represent it.
pub fn split(codebook: u8, lsb_bits: u32, huff_offset: i32, residual: i32) -> Option<Split> {
    let value = i64::from(residual) - sign_offset(codebook, lsb_bits, huff_offset);
    if value < 0 {
        return None;
    }
    match table(codebook) {
        None => {
            if lsb_bits == 0 {
                // Nothing is written at all, so the only representable value
                // is the one the offset alone produces.
                return (value == 0).then_some(Split {
                    symbol: None,
                    lsbs: 0,
                });
            }
            (value < 1i64 << lsb_bits).then_some(Split {
                symbol: None,
                lsbs: value as u32,
            })
        }
        Some(table) => {
            let symbol = (value >> lsb_bits) as usize;
            (symbol < table.len()).then_some(Split {
                symbol: Some(symbol),
                lsbs: (value & ((1i64 << lsb_bits) - 1)) as u32,
            })
        }
    }
}

/// The code for a symbol.
pub fn code(codebook: u8, symbol: usize) -> Code {
    table(codebook).expect("codebook 0 has no symbols")[symbol]
}

/// What a block of residuals costs in this coding, or `None` if it does not
/// fit.
pub fn cost(codebook: u8, lsb_bits: u32, huff_offset: i32, residuals: &[i32]) -> Option<usize> {
    let offset = sign_offset(codebook, lsb_bits, huff_offset);
    match table(codebook) {
        None => {
            for residual in residuals {
                let value = i64::from(*residual) - offset;
                let fits = if lsb_bits == 0 {
                    value == 0
                } else {
                    (0..1i64 << lsb_bits).contains(&value)
                };
                if !fits {
                    return None;
                }
            }
            Some(residuals.len() * lsb_bits as usize)
        }
        Some(table) => {
            let mut bits = residuals.len() * lsb_bits as usize;
            for residual in residuals {
                let value = i64::from(*residual) - offset;
                if value < 0 {
                    return None;
                }
                let symbol = (value >> lsb_bits) as usize;
                bits += *table.get(symbol).map(|(_, length)| length)? as usize;
            }
            Some(bits)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mistyped digit in a table would almost certainly break this, and
    /// would otherwise produce a stream that decodes to the wrong samples with
    /// every checksum intact.
    #[test]
    fn every_table_is_a_prefix_code() {
        for codebook in 1..=MAX_CODEBOOK {
            let table = table(codebook).unwrap();
            for (index, (code, length)) in table.iter().enumerate() {
                assert!(
                    *length >= 1 && *length <= 9,
                    "codebook {codebook} symbol {index} has length {length}"
                );
                assert!(
                    code >> length == 0,
                    "codebook {codebook} symbol {index}: {code:#x} does not fit {length} bits"
                );
                for (other, (other_code, other_length)) in table.iter().enumerate() {
                    if other == index {
                        continue;
                    }
                    let (short, long, shift) = if length <= other_length {
                        (code, other_code, other_length - length)
                    } else {
                        (other_code, code, length - other_length)
                    };
                    assert_ne!(
                        *short,
                        long >> shift,
                        "codebook {codebook}: symbols {index} and {other} share a prefix"
                    );
                }
            }
        }
    }

    /// Kraft's inequality. A code that violates it is not decodable at all;
    /// one that leaves slack is merely not quite optimal, which these do.
    #[test]
    fn every_table_satisfies_kraft() {
        for codebook in 1..=MAX_CODEBOOK {
            let sum: f64 = table(codebook)
                .unwrap()
                .iter()
                .map(|(_, length)| 2f64.powi(-(*length as i32)))
                .sum();
            assert!(
                sum <= 1.0 + 1e-12,
                "codebook {codebook} sums to {sum}, which is not a code"
            );
            assert!(sum > 0.9, "codebook {codebook} sums to only {sum}");
        }
    }

    /// The decoder's reassembly, written out here from its own description, so
    /// the split can be checked against something other than itself.
    fn reassemble(codebook: u8, lsb_bits: u32, huff_offset: i32, split: Split) -> i64 {
        let symbol = split.symbol.unwrap_or(0) as i64;
        let mut value = if lsb_bits > 0 {
            (symbol << lsb_bits) + i64::from(split.lsbs)
        } else {
            symbol
        };
        value += sign_offset(codebook, lsb_bits, huff_offset);
        value
    }

    #[test]
    fn splitting_and_reassembling_is_the_identity() {
        for codebook in 0..=MAX_CODEBOOK {
            for lsb_bits in 0..=8u32 {
                for offset in [-1000i32, 0, 1000] {
                    for residual in -5000..=5000i32 {
                        let Some(split) = split(codebook, lsb_bits, offset, residual) else {
                            continue;
                        };
                        assert_eq!(
                            reassemble(codebook, lsb_bits, offset, split),
                            i64::from(residual),
                            "codebook {codebook}, {lsb_bits} lsbs, offset {offset}"
                        );
                    }
                }
            }
        }
    }

    /// The range each coding covers has to be contiguous, or the search would
    /// find a width that fits the extremes and drops something between them.
    #[test]
    fn each_coding_covers_a_contiguous_range() {
        for codebook in 0..=MAX_CODEBOOK {
            for lsb_bits in [0u32, 1, 4] {
                let representable: Vec<i32> = (-20_000..=20_000i32)
                    .filter(|r| split(codebook, lsb_bits, 0, *r).is_some())
                    .collect();
                assert!(
                    !representable.is_empty(),
                    "codebook {codebook} with {lsb_bits} lsbs represents nothing"
                );
                let first = representable[0];
                for (step, value) in representable.iter().enumerate() {
                    assert_eq!(
                        *value,
                        first + step as i32,
                        "codebook {codebook}, {lsb_bits} lsbs: a gap in the range"
                    );
                }
            }
        }
    }

    /// The cost function has to agree with what would actually be written.
    #[test]
    fn cost_counts_the_bits_that_would_be_written() {
        let residuals: Vec<i32> = (-30..30).collect();
        for codebook in 0..=MAX_CODEBOOK {
            for lsb_bits in 0..=8u32 {
                let Some(reported) = cost(codebook, lsb_bits, 0, &residuals) else {
                    continue;
                };
                let mut counted = 0usize;
                for residual in &residuals {
                    let split = split(codebook, lsb_bits, 0, *residual)
                        .expect("cost said this coding fits");
                    if let Some(symbol) = split.symbol {
                        counted += code(codebook, symbol).1 as usize;
                    }
                    counted += lsb_bits as usize;
                }
                assert_eq!(reported, counted, "codebook {codebook}, {lsb_bits} lsbs");
            }
        }
    }

    /// A block of silence should cost nothing at all: no codebook, no low
    /// bits, and the offset alone reproduces zero.
    #[test]
    fn silence_is_free() {
        let silence = [0i32; 40];
        assert_eq!(cost(NONE, 0, 0, &silence), Some(0));
    }

    /// The stated range and what actually splits have to be the same set, or
    /// the search picks a width that drops samples.
    #[test]
    fn the_stated_range_is_the_representable_one() {
        for codebook in 0..=MAX_CODEBOOK {
            for lsb_bits in 0..=6u32 {
                for offset in [-100i32, 0, 100] {
                    let (low, high) = range(codebook, lsb_bits, offset);
                    assert!(low <= high, "codebook {codebook} has an empty range");
                    for probe in [low - 1, low, high, high + 1] {
                        let inside = probe >= low && probe <= high;
                        let splits = split(codebook, lsb_bits, offset, probe as i32).is_some();
                        assert_eq!(
                            splits, inside,
                            "codebook {codebook}, {lsb_bits} lsbs, offset {offset}: \
                             {probe} against [{low}, {high}]"
                        );
                    }
                }
            }
        }
    }
}
