//! Reading and writing one value through a codebook.

use crate::trees;
use hz_core::bits::BitWriter;
use hz_core::bits_read::{BitReader, Ran};
use std::sync::OnceLock;

/// Which codebook: TS 103 420 clause 6.6.3's `get_joc_huff_code(mode, type)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A dense matrix entry, coarse or fine.
    Matrix { fine: bool },
    /// A sparse vector entry, coarse or fine.
    Vector { fine: bool },
    /// A sparse channel index, for five or seven downmix channels.
    Index { seven: bool },
}

/// A decoding tree and the codeword it gives each value.
pub struct Codebook {
    tree: &'static [[i16; 2]],
    /// `(codeword, length)` by value, the codeword in its low bits.
    codes: Vec<(u32, u8)>,
}

impl Codebook {
    /// Walk the tree once, depth first, noting the path to every leaf.
    fn new(tree: &'static [[i16; 2]]) -> Self {
        let mut codes = Vec::new();
        let mut stack = vec![(0usize, 0u32, 0u8)];
        while let Some((node, code, length)) = stack.pop() {
            for bit in 0..2u32 {
                let child = tree[node][bit as usize];
                let (code, length) = ((code << 1) | bit, length + 1);
                if child > 0 {
                    stack.push((child as usize, code, length));
                } else {
                    let value = (-i32::from(child) - 1) as usize;
                    if codes.len() <= value {
                        codes.resize(value + 1, (0, 0));
                    }
                    debug_assert_eq!(codes[value].1, 0, "a value with two codewords");
                    codes[value] = (code, length);
                }
            }
        }
        Self { tree, codes }
    }

    pub fn get(kind: Kind) -> &'static Codebook {
        static BOOKS: [OnceLock<Codebook>; 6] = [const { OnceLock::new() }; 6];
        let (slot, tree) = match kind {
            Kind::Matrix { fine: false } => (0, trees::COARSE_GENERIC),
            Kind::Matrix { fine: true } => (1, trees::FINE_GENERIC),
            Kind::Vector { fine: false } => (2, trees::COARSE_COEFF_SPARSE),
            Kind::Vector { fine: true } => (3, trees::FINE_COEFF_SPARSE),
            Kind::Index { seven: false } => (4, trees::POS_INDEX_5CH),
            Kind::Index { seven: true } => (5, trees::POS_INDEX_7CH),
        };
        BOOKS[slot].get_or_init(|| Codebook::new(tree))
    }

    /// Values the codebook can say: nought to one short of this.
    pub fn values(&self) -> usize {
        self.codes.len()
    }

    /// Clause 6.6.3's `huff_decode`: from the root, a bit at a time, to a
    /// leaf.
    pub fn read(&self, reader: &mut BitReader<'_>) -> Result<u16, Ran> {
        let mut node = 0i16;
        loop {
            let bit = usize::from(reader.get_bit()?);
            node = self
                .tree
                .get(node as usize)
                .ok_or_else(|| Ran("a codeword that leaves its tree".into()))?[bit];
            if node <= 0 {
                return Ok((-i32::from(node) - 1) as u16);
            }
        }
    }

    /// The value's codeword.
    ///
    /// # Panics
    /// If the codebook cannot say the value.
    pub fn write(&self, writer: &mut BitWriter, value: u16) {
        let (code, length) = self.codes[usize::from(value)];
        debug_assert!(length > 0, "{value} has no codeword");
        writer.put(u32::from(length), code);
    }

    /// Bits the value's codeword takes.
    pub fn length(&self, value: u16) -> usize {
        usize::from(self.codes[usize::from(value)].1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Kind; 6] = [
        Kind::Matrix { fine: false },
        Kind::Matrix { fine: true },
        Kind::Vector { fine: false },
        Kind::Vector { fine: true },
        Kind::Index { seven: false },
        Kind::Index { seven: true },
    ];

    /// Every codebook is a complete prefix code: Kraft's sum is exactly one,
    /// which a mistyped node would break; every value has a codeword; and
    /// every codeword reads back as its value.
    #[test]
    fn every_codebook_is_a_complete_prefix_code() {
        for kind in ALL {
            let book = Codebook::get(kind);
            let kraft: f64 = book
                .codes
                .iter()
                .map(|(_, length)| 0.5f64.powi(i32::from(*length)))
                .sum();
            assert!((kraft - 1.0).abs() < 1e-12, "{kind:?}: Kraft sum {kraft}");
            assert!(
                book.codes.iter().all(|(_, l)| *l > 0),
                "{kind:?}: a value without a code"
            );

            let mut writer = BitWriter::new();
            for value in 0..book.values() {
                book.write(&mut writer, value as u16);
            }
            let bytes = writer.finish();
            let mut reader = BitReader::new(&bytes);
            for value in 0..book.values() {
                assert_eq!(book.read(&mut reader).unwrap(), value as u16, "{kind:?}");
            }
        }
    }

    /// What the decoder and clause 6.6.3 both rely on: every codebook says
    /// nought in one bit.
    #[test]
    fn nought_takes_one_bit() {
        for kind in ALL {
            assert_eq!(Codebook::get(kind).length(0), 1, "{kind:?}");
        }
        assert_eq!(Codebook::get(Kind::Index { seven: false }).values(), 5);
        assert_eq!(Codebook::get(Kind::Index { seven: true }).values(), 7);
    }
}
