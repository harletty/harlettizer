// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/mlp.c — Copyright (c) 2007-2008 Ian Caulfield
//   libavutil/crc.c — Copyright (c) 2006 Michael Niedermayer
// What was taken and what was changed is recorded in docs/provenance.md.

//! The four integrity fields MLP puts on a stream.
//!
//! None of them is a textbook CRC, and each is odd in its own way:
//!
//! - The **major sync** checksum is a CRC-16 over all but the last two bytes,
//!   XORed with those two bytes read *little*-endian in a format that is
//!   otherwise big-endian throughout.
//! - The **substream** checksum is the same trick one byte wide, with an
//!   initial value that is itself the CRC of a byte that is never in the data.
//! - The **restart header** checksum covers a range that neither starts nor
//!   ends on a byte boundary: it begins two bits into the first byte and runs
//!   for a bit count the caller knows and the buffer does not.
//! - The **parity** byte is not a CRC at all, and is written XORed with a
//!   constant that appears nowhere in any description of the format.
//!
//! Ported from FFmpeg's `libavcodec/mlp.c` (LGPL-2.1-or-later; see
//! `docs/provenance.md`) and checked against real streams rather than against
//! the port, since a port that reproduces a mistake is still wrong.

/// The major sync's CRC-16.
const POLY16: u16 = 0x002D;
/// The substream checksum's CRC-8.
const POLY8_SUBSTREAM: u8 = 0x63;
/// The restart header's CRC-8.
const POLY8_RESTART: u8 = 0x1D;

/// The substream checksum's initial value.
///
/// FFmpeg writes it as `0x3c` with a note that `crc_63[0xa2] == 0x3c`, and the
/// decoder crate in this workspace states the same algorithm with an initial
/// value of `0xa2`. Those look like two ways of saying one thing and they are
/// not: the register update is `crc = table[crc ^ byte]`, so seeding with
/// `0xa2` gives `table[0xa2 ^ first]` where seeding with `0x3c` gives
/// `table[0x3c ^ first]`, and the two agree only when the first byte is zero.
///
/// `0x3c` is the one that reproduces real streams. This was seeded with `0xa2`
/// first, on exactly that reasoning, and two independent decoders rejected
/// every access unit it produced.
const SUBSTREAM_INIT: u8 = 0x3c;

/// The constant the parity byte is XORed with before being written.
pub const PARITY_XOR: u8 = 0xa9;

const fn table8(poly: u8) -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u8;
        let mut bit = 0;
        while bit < 8 {
            value = (value << 1) ^ (((value >> 7) & 1) * poly);
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

const fn table16(poly: u16) -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = (index as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            value = (value << 1) ^ (((value >> 15) & 1) * poly);
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

static TABLE_SUBSTREAM: [u8; 256] = table8(POLY8_SUBSTREAM);
static TABLE_RESTART: [u8; 256] = table8(POLY8_RESTART);
static TABLE_MAJOR_SYNC: [u16; 256] = table16(POLY16);

fn crc8(table: &[u8; 256], init: u8, data: &[u8]) -> u8 {
    data.iter()
        .fold(init, |crc, byte| table[(crc ^ byte) as usize])
}

fn crc16(table: &[u16; 256], init: u16, data: &[u8]) -> u16 {
    data.iter().fold(init, |crc, byte| {
        (crc << 8) ^ table[((crc >> 8) as u8 ^ byte) as usize]
    })
}

/// The major sync's checksum: the two bytes to write after `data`.
///
/// `data` is the major sync from its first byte up to, but not including, the
/// checksum — twenty-six bytes for both MLP and TrueHD.
///
/// The two bytes are returned rather than a `u16`, because there is no
/// endianness that describes what happens here. The CRC covers everything
/// except the last two bytes, and those last two bytes are then XORed into it
/// **byte for byte, in the opposite order**: the CRC's high byte against the
/// first of them, its low byte against the second.
///
/// FFmpeg arrives at the same result by a different route — it holds the CRC
/// register byte-swapped throughout, which is what `av_crc` does for a
/// most-significant-bit-first polynomial, then XORs the two bytes read
/// little-endian and writes the answer little-endian. Two reversals and a
/// swap that cancel. Written out as it is here there is nothing to cancel,
/// and the port was wrong until a real stream said so.
///
/// # Panics
/// If `data` is shorter than two bytes.
pub fn major_sync(data: &[u8]) -> [u8; 2] {
    assert!(data.len() >= 2, "the major sync checksum needs two bytes");
    let split = data.len() - 2;
    let crc = crc16(&TABLE_MAJOR_SYNC, 0, &data[..split]);
    [
        (crc >> 8) as u8 ^ data[split],
        (crc & 0xFF) as u8 ^ data[split + 1],
    ]
}

/// A substream's checksum, over everything in the substream before it.
///
/// # Panics
/// If `data` is empty.
pub fn substream(data: &[u8]) -> u8 {
    assert!(!data.is_empty(), "the substream checksum needs a byte");
    let split = data.len() - 1;
    crc8(&TABLE_SUBSTREAM, SUBSTREAM_INIT, &data[..split]) ^ data[split]
}

/// The restart header's checksum, over `bits` bits starting two bits into
/// `data`.
///
/// The two leading bits are excluded because they belong to whatever preceded
/// the restart header in the same byte. FFmpeg achieves that by seeding the
/// register with `data[0] & 0xC0`, which cancels them when the first byte goes
/// through; the same trick is used here rather than a shifted copy of the
/// buffer, because copying is what this is on the hot path to avoid.
pub fn restart_header(data: &[u8], bits: usize) -> u8 {
    let whole = (bits + 2) / 8;
    assert!(
        data.len() > whole || (bits + 2).is_multiple_of(8) && data.len() >= whole,
        "the restart header checksum needs {whole} bytes and a tail"
    );

    let mut crc = crc8(&TABLE_RESTART, data[0] & 0xC0, &data[..whole - 1]);
    crc ^= data[whole - 1];

    // The bits of the next byte that are inside the range, one at a time.
    for bit in 0..((bits + 2) & 7) {
        let high = crc & 0x80 != 0;
        crc <<= 1;
        if high {
            crc ^= POLY8_RESTART;
        }
        crc ^= (data[whole] >> (7 - bit)) & 1;
    }

    crc
}

/// Every byte XORed together.
pub fn parity(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, byte| acc ^ byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables are built at compile time; if the generator is wrong every
    /// checksum here is wrong together and nothing else would notice.
    #[test]
    fn the_tables_agree_with_a_bit_at_a_time_crc() {
        fn slow8(poly: u8, byte: u8) -> u8 {
            let mut value = byte;
            for _ in 0..8 {
                let high = value & 0x80 != 0;
                value <<= 1;
                if high {
                    value ^= poly;
                }
            }
            value
        }
        for byte in 0..=255u8 {
            assert_eq!(TABLE_SUBSTREAM[byte as usize], slow8(POLY8_SUBSTREAM, byte));
            assert_eq!(TABLE_RESTART[byte as usize], slow8(POLY8_RESTART, byte));
        }
    }

    /// The substream checksum, against a substream this project did not
    /// write: the first one FFmpeg's encoder writes for a 48 kHz stereo sine
    /// (`ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000 -c:a truehd`).
    /// Its last two bytes are the parity and the checksum of the bytes before
    /// them.
    ///
    /// Here for the same reason as the major sync fixture: the seed cannot be
    /// derived from anything, only checked, and it was wrong once already.
    #[test]
    fn the_substream_checksum_matches_a_real_stream() {
        const SUBSTREAM: [u8; 110] = [
            0xf1, 0xea, 0x00, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00, 0x0e, 0x9c, 0xc0, 0x00, 0x00,
            0x00, 0x03, 0xe2, 0x82, 0x22, 0x00, 0xb4, 0x24, 0x00, 0x08, 0x19, 0x7f, 0xff, 0x39,
            0x00, 0x02, 0xb1, 0xda, 0x71, 0xda, 0x1a, 0xab, 0x05, 0x19, 0x60, 0xbb, 0x40, 0x13,
            0x31, 0xd9, 0x15, 0x2c, 0x12, 0x20, 0x18, 0xea, 0x3a, 0x01, 0x00, 0x00, 0x03, 0x7c,
            0x78, 0x4e, 0x5f, 0x91, 0x24, 0x1b, 0xc7, 0xfc, 0x50, 0x66, 0xbe, 0xee, 0xc1, 0x2d,
            0x10, 0x00, 0x19, 0x5e, 0xd6, 0x01, 0xfb, 0x97, 0x67, 0xab, 0x30, 0xf8, 0xe5, 0x12,
            0x09, 0x4d, 0xfc, 0x2f, 0x49, 0xc2, 0xf2, 0x84, 0x88, 0x69, 0xd1, 0x19, 0xbf, 0x83,
            0xfd, 0x61, 0x6e, 0x06, 0xcb, 0xf0, 0xa1, 0xb8, 0xca, 0x00, 0x30, 0x37,
        ];
        let (body, tail) = SUBSTREAM.split_at(SUBSTREAM.len() - 2);
        assert_eq!(parity(body) ^ PARITY_XOR, tail[0], "parity");
        assert_eq!(substream(body), tail[1], "checksum");
    }

    /// The one test here that is not self-referential: twenty-eight bytes of
    /// major sync out of a stream this project did not write — the one FFmpeg
    /// writes for any 48 kHz stereo input — whose last two bytes are the
    /// checksum of the first twenty-six.
    ///
    /// Every other test in this file checks the port against itself. This one
    /// checks it against the format, and it is the reason the major sync
    /// checksum is right: the first version of it returned the two bytes the
    /// other way round, passed everything else, and would have produced a
    /// stream no decoder would sync to.
    #[test]
    fn the_major_sync_checksum_matches_a_real_stream() {
        // A stereo 48 kHz TrueHD access unit's major sync, as FFmpeg writes it.
        const SYNC: [u8; 28] = [
            0xf8, 0x72, 0x6f, 0xba, 0x00, 0x50, 0xa0, 0x01, //
            0xb7, 0x52, 0x00, 0x00, 0x00, 0x00, 0x8c, 0x7f, //
            0x10, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x34, 0x28,
        ];
        assert_eq!(major_sync(&SYNC[..26]), [SYNC[26], SYNC[27]]);
    }

    /// A checksum that ignores its input would pass every structural test.
    #[test]
    fn every_byte_reaches_every_checksum() {
        let base: Vec<u8> = (0..32u8).collect();
        for index in 0..base.len() {
            let mut changed = base.clone();
            changed[index] ^= 0x40;
            assert_ne!(
                major_sync(&base),
                major_sync(&changed),
                "major sync ignored byte {index}"
            );
            assert_ne!(
                substream(&base),
                substream(&changed),
                "substream ignored byte {index}"
            );
        }
    }

    #[test]
    fn parity_is_the_xor_of_everything() {
        assert_eq!(parity(&[0x0F, 0xF0]), 0xFF);
        assert_eq!(parity(&[0xAB, 0xAB]), 0x00);
        assert_eq!(parity(&[]), 0x00);
    }

    /// The restart header's range starts two bits in, so the top two bits of
    /// the first byte must not change the answer — that is the whole point of
    /// seeding with them.
    #[test]
    fn the_restart_checksum_ignores_the_two_bits_before_it() {
        let mut data = [0x3Fu8, 0xAA, 0x55, 0xC3, 0x00];
        let bits = 30;
        let first = restart_header(&data, bits);
        for top in [0x00u8, 0x40, 0x80, 0xC0] {
            data[0] = (data[0] & 0x3F) | top;
            assert_eq!(
                restart_header(&data, bits),
                first,
                "the two leading bits changed the checksum"
            );
        }
    }
}
