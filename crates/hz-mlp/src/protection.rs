//! The Evolution frame's protection field, and the keyed digest that fills it.
//!
//! An Evolution frame closes with a protection field whose computation the
//! format leaves to the implementation and which a decoder is told it may
//! ignore. What goes in it is a keyed digest — [`HmacSha256`] — over the
//! access unit and the frame, truncated to the field's length: a byte, for
//! the eight-bit primary field every reference stream writes.
//!
//! # What the digest covers
//!
//! ```text
//! ┌──────────────────────────────────────────┬────────────┬──────────────┬─────────┬────────┐
//! │ access unit: header, major sync,         │ length,    │ Evolution    │ zeroes  │ parity │
//! │ directory, substreams                    │ frame hdr  │ frame        │         │        │
//! └──────────────────────────────────────────┴────────────┴──────────────┴─────────┴────────┘
//! └──────────────── signed ─────────────────┘ └ 4 bytes ┘ └── the declared span, signed ──┘
//!                                               not signed   with the protection bits zeroed
//! ```
//!
//! Two things about it are easy to get wrong, and both are checked against
//! a reference the tests hold:
//!
//! - **The access unit is signed with its header final.** The unit's own
//!   length and parity nibble and the substream directory are patched in after
//!   the substreams are written, and the digest is over the patched bytes, so
//!   it is the last thing computed.
//! - **The frame is signed at its declared length with the field zeroed.**
//!   The four bytes between the audio and the frame — the block's length word
//!   and the frame's own header — are left out; the frame's declared span goes
//!   in whole, its padding included, with every bit from the first protection
//!   bit to the end of the span cleared. The two length codes ahead of the
//!   field stay. The parity byte is outside the span and is recomputed after
//!   the field is written.
//!
//! # The key
//!
//! Is not part of this project, which neither distributes one nor helps
//! anyone obtain one — see `LEGAL.md` §4. Whoever has one hands it to the
//! encoder; a stream written without one carries a constant in the field,
//! which parses identically and verifies against nothing. The key's
//! identifier in the frame is written as zero either way.

use hz_core::hmac::{DIGEST_BYTES, HmacSha256};

/// The bytes of the block ahead of the frame the digest leaves out: the
/// length word, and the frame's reserved nibble and declared length.
pub const FRAME_HEADER_BYTES: usize = 4;

/// The digest over one access unit.
///
/// `unit` is the whole unit with its header final; `block_at` where its extra
/// data block starts; `declared` the byte length the Evolution header
/// declares; and `protection_bit` the bit, counted from the unit's start, at
/// which the primary protection field's own bits begin — after its two length
/// codes.
///
/// No allocation: the frame is fed to the digest in three pieces — the bytes
/// ahead of the field, the byte the field starts in with its tail masked, and
/// zeroes to the end of the span — rather than copied and cleared.
///
/// # Panics
/// If the span or the bit lie outside the unit.
pub fn digest(
    key: &HmacSha256,
    unit: &[u8],
    block_at: usize,
    declared: usize,
    protection_bit: usize,
) -> [u8; DIGEST_BYTES] {
    let frame_at = block_at + FRAME_HEADER_BYTES;
    let frame = &unit[frame_at..frame_at + declared];
    let within = protection_bit
        .checked_sub(frame_at * 8)
        .expect("the protection field lies inside the frame");
    let whole = within / 8;
    assert!(
        whole < frame.len(),
        "the protection field lies inside the declared span"
    );

    let mut signer = key.sign();
    signer.update(&unit[..block_at]);
    signer.update(&frame[..whole]);
    // The byte the field starts in keeps the bits ahead of the field — the
    // length codes are in it — and loses the rest.
    let keep = (within % 8) as u32;
    let mask = if keep == 0 { 0 } else { 0xffu8 << (8 - keep) };
    signer.update(&[frame[whole] & mask]);
    let mut zeroes = frame.len() - whole - 1;
    while zeroes > 0 {
        let take = zeroes.min(ZEROES.len());
        signer.update(&ZEROES[..take]);
        zeroes -= take;
    }
    signer.finish()
}

const ZEROES: [u8; 64] = [0; 64];

#[cfg(test)]
mod tests {
    use super::*;

    /// The extra data block this encoder writes for a sixteen-element
    /// programme, unkeyed, behind a hundred bytes standing in for the audio
    /// ahead of it.
    ///
    /// The expected digest was computed independently, over the same bytes
    /// with the same throwaway key, by a tool whose framing reproduces every
    /// protection field of the three reference streams to hand: 18 764 of
    /// 18 764, on streams this project did not write. So this pins the
    /// framing — what is left out, where the zeroing starts — to the one known
    /// to be the reference's, without the reference's key.
    #[test]
    fn the_framing_is_the_one_that_reproduces_the_reference() {
        let block: [u8; 76] = [
            0x80, 0x25, 0x00, 0x47, 0x02, 0xc2, 0x82, 0x1f, 0x84, 0x4b, 0x80, 0x00, 0xa2, 0x71,
            0x3b, 0x98, 0x40, 0xe4, 0x71, 0x60, 0x81, 0xcc, 0xd7, 0x21, 0x03, 0xa1, 0x97, 0x02,
            0x07, 0x52, 0xf8, 0x04, 0x0e, 0xc5, 0x93, 0x08, 0x1d, 0xca, 0x6c, 0x10, 0x3c, 0x13,
            0x64, 0x20, 0x79, 0x23, 0xe0, 0x40, 0xf4, 0x41, 0x00, 0x81, 0xec, 0x76, 0x61, 0x03,
            0xe0, 0xd5, 0x82, 0x07, 0xd1, 0x7c, 0x84, 0x0f, 0xc2, 0x9c, 0x08, 0x1f, 0xc4, 0x60,
            0x10, 0x00, 0x02, 0x2d, 0x00, 0xfe,
        ];
        let mut unit: Vec<u8> = (0..100u32).map(|n| (n * 7 + 3) as u8).collect();
        unit.extend_from_slice(&block);
        let key: Vec<u8> = (0..32).collect();
        let key = HmacSha256::new(&key);

        // The frame is 561 bits from bit 32 of the block, so its eight-bit
        // field is the last eight of them: bits 585 to 593, after the two
        // length codes at 581. Not byte-aligned, which is the case that
        // matters.
        let digest = digest(&key, &unit, 100, 71, 100 * 8 + 585);
        assert_eq!(
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            EXPECTED
        );
    }

    const EXPECTED: &str = "f90683b70c95b2e57ca3cc3b924a1bbc548929630d8833fe5b68d8cec929cf4c";

    /// Whatever the field holds is not in the digest: a frame with the field
    /// filled signs the same as one with it clear, and a bit ahead of the
    /// field is.
    #[test]
    fn the_field_itself_is_outside_the_digest() {
        let key = HmacSha256::new(b"k");
        let mut unit = vec![0x11u8; 40];
        // A block of a fake frame: four header bytes, then a span of ten
        // with the field starting mid-way through the eighth.
        unit.extend_from_slice(&[
            0xb0, 0x06, 0x00, 0x0b, 0xaa, 0xaa, 0xaa, 0xa5, 0xa5, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        let block_at = 40;
        let span = 10;
        let bit = (block_at + 4) * 8 + 7 * 8 + 4;
        let clear = digest(&key, &unit, block_at, span, bit);

        // Fill the field's byte and everything after it in the span.
        unit[block_at + 4 + 7] |= 0x0f;
        unit[block_at + 4 + 8] = 0xff;
        unit[block_at + 4 + 9] = 0xff;
        assert_eq!(digest(&key, &unit, block_at, span, bit), clear);

        // But a bit ahead of the field — in the same byte — is signed.
        unit[block_at + 4 + 7] ^= 0x10;
        assert_ne!(digest(&key, &unit, block_at, span, bit), clear);
        unit[block_at + 4 + 7] ^= 0x10;

        // As is the audio, and not the four header bytes.
        unit[0] ^= 1;
        assert_ne!(digest(&key, &unit, block_at, span, bit), clear);
        unit[0] ^= 1;
        unit[block_at + 1] ^= 1;
        assert_eq!(digest(&key, &unit, block_at, span, bit), clear);
    }
}
