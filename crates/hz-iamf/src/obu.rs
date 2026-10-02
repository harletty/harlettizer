//! The OBU framing every part of an IA sequence travels in (IAMF §3.2).
//!
//! An OBU is a one-byte header — a five-bit type and three flags — then its
//! payload's length as a LEB128, then the payload. Nothing in the framing
//! knows what a payload means, so the descriptors and the audio frames are
//! assembled as plain bytes first and wrapped here.

/// The OBU types this writer emits (IAMF §3.2, `obu_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObuType {
    CodecConfig,
    AudioElement,
    MixPresentation,
    TemporalDelimiter,
    /// An audio frame of substream `0..=17`, whose id the type itself
    /// carries.
    AudioFrameId(u8),
    SequenceHeader,
}

impl ObuType {
    pub fn code(self) -> u8 {
        match self {
            Self::CodecConfig => 0,
            Self::AudioElement => 1,
            Self::MixPresentation => 2,
            Self::TemporalDelimiter => 4,
            Self::AudioFrameId(id) => {
                assert!(id <= 17, "substream {id} has no implicit frame type");
                6 + id
            }
            Self::SequenceHeader => 31,
        }
    }
}

/// Append `value` as an unsigned LEB128: seven bits a byte, least
/// significant first, the top bit saying another byte follows.
///
/// The minimal form, which is the only one written: IAMF lets a writer pad a
/// LEB128 to eight bytes, and nothing here has a reason to.
pub fn put_leb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Bytes [`put_leb128`] spends on `value`.
pub fn leb128_len(value: u64) -> usize {
    let bits = 64 - value.leading_zeros() as usize;
    bits.div_ceil(7).max(1)
}

/// Append one OBU: header, size, payload.
///
/// `trim` is `(at_end, at_start)` in samples, in the order the header
/// extension carries them, and only an audio frame may have one. It is part
/// of what `obu_size` counts.
pub fn put_obu(out: &mut Vec<u8>, kind: ObuType, trim: Option<(u32, u32)>, payload: &[u8]) {
    assert!(
        trim.is_none() || matches!(kind, ObuType::AudioFrameId(_)),
        "only an audio frame carries a trim"
    );
    // obu_type (5) | obu_redundant_copy (1) | obu_trimming_status_flag (1) |
    // obu_extension_flag (1).
    out.push((kind.code() << 3) | (u8::from(trim.is_some()) << 1));
    let trim_len = trim.map_or(0, |(end, start)| {
        leb128_len(u64::from(end)) + leb128_len(u64::from(start))
    });
    put_leb128(out, (trim_len + payload.len()) as u64);
    if let Some((end, start)) = trim {
        put_leb128(out, u64::from(end));
        put_leb128(out, u64::from(start));
    }
    out.extend_from_slice(payload);
}

/// Append a string as IAMF writes one: UTF-8, then a terminating nul.
pub fn put_string(out: &mut Vec<u8>, value: &str) {
    assert!(!value.contains('\0'), "an IAMF string cannot hold a nul");
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

/// A signed decibel value as the Q7.8 the descriptors carry: eight
/// fractional bits, rounded, and clamped to the field — an integrated
/// loudness of silence is minus infinity, and the field's floor of −128 dB
/// is the nearest thing it can say.
pub fn q7_8(db: f64) -> i16 {
    if db.is_nan() {
        return i16::MIN;
    }
    (db * 256.0)
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_is_seven_bits_a_byte_low_first() {
        let cases: [(u64, &[u8]); 5] = [
            (0, &[0x00]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (300, &[0xac, 0x02]),
            (48_000, &[0x80, 0xf7, 0x02]),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            put_leb128(&mut out, value);
            assert_eq!(out, expected, "{value}");
            assert_eq!(leb128_len(value), expected.len(), "{value}");
        }
    }

    /// The spec's own example: a sequence header with neither flag set starts
    /// `0xF8 0x06` — type 31, and six bytes of payload.
    #[test]
    fn a_sequence_header_starts_the_way_the_spec_says() {
        let mut out = Vec::new();
        put_obu(&mut out, ObuType::SequenceHeader, None, b"iamf\0\0");
        assert_eq!(&out[..2], &[0xf8, 0x06]);
    }

    /// A trim is counted in the size and set in the header, end first.
    #[test]
    fn a_trimmed_frame_counts_its_trim_in_its_size() {
        let mut out = Vec::new();
        put_obu(
            &mut out,
            ObuType::AudioFrameId(3),
            Some((200, 0)),
            &[1, 2, 3],
        );
        assert_eq!(out[0], (9 << 3) | 0b10);
        assert_eq!(out[1], 2 + 1 + 3);
        assert_eq!(&out[2..5], &[0xc8, 0x01, 0x00]);
        assert_eq!(&out[5..], &[1, 2, 3]);
    }

    #[test]
    fn q7_8_rounds_and_saturates() {
        assert_eq!(q7_8(-23.0), -23 * 256);
        assert_eq!(q7_8(-0.5), -128);
        assert_eq!(q7_8(f64::NEG_INFINITY), i16::MIN);
        assert_eq!(q7_8(500.0), i16::MAX);
    }
}
