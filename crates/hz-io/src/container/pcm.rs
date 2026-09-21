//! Linear PCM: how samples are laid out, and how to get them in and out of
//! bytes.
//!
//! Shared by every container here, because the containers differ in their
//! headers and agree on their payload. Conversion is deliberately exact:
//! samples are sign-extended at their native width, so a 24-bit file yields
//! 24-bit values rather than values rescaled to 32 bits. Rescaling would be
//! convenient and would quietly destroy the exactness the lossless track is
//! built on.

use hz_core::{Error, Result};
use std::path::Path;

/// Whether samples are integers or floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Integer,
    Float,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endianness {
    Big,
    Little,
}

/// Everything needed to interpret a block of PCM.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PcmFormat {
    /// Carried as a float because that is how CAF stores it. Every container
    /// that wants an integer rate rounds on the way out.
    pub sample_rate: f64,
    pub channels: u32,
    pub bits_per_channel: u32,
    pub sample_format: SampleFormat,
    pub endianness: Endianness,
}

impl PcmFormat {
    /// The 24-bit big-endian layout a master's audio component uses.
    pub fn master(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            channels,
            bits_per_channel: 24,
            sample_format: SampleFormat::Integer,
            endianness: Endianness::Big,
        }
    }

    /// The 24-bit little-endian layout a WAV file uses.
    pub fn wav(sample_rate: f64, channels: u32, bits_per_channel: u32) -> Self {
        Self {
            sample_rate,
            channels,
            bits_per_channel,
            sample_format: SampleFormat::Integer,
            endianness: Endianness::Little,
        }
    }

    pub const fn bytes_per_sample(&self) -> usize {
        self.bits_per_channel.div_ceil(8) as usize
    }

    pub const fn bytes_per_frame(&self) -> usize {
        self.bytes_per_sample() * self.channels as usize
    }

    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate.round() as u32
    }

    /// The magnitude a full-scale sample decodes to.
    ///
    /// Uniform across formats because [`decode`] sign-extends at the native
    /// width and carries float payloads at 24 bits, so dividing by this always
    /// gives a value in ±1 whatever the file was.
    pub fn full_scale(&self) -> f32 {
        match self.sample_format {
            SampleFormat::Float => 8_388_608.0,
            SampleFormat::Integer => (1u64 << (self.bits_per_channel - 1)) as f32,
        }
    }

    /// Reject a layout this crate cannot convert, rather than half-reading it.
    pub fn check_supported(&self, path: &Path) -> Result<()> {
        if self.channels == 0 {
            return Err(Error::malformed(path, "no channels"));
        }
        let ok = match self.sample_format {
            SampleFormat::Integer => matches!(self.bits_per_channel, 16 | 24 | 32),
            SampleFormat::Float => self.bits_per_channel == 32,
        };
        if ok {
            Ok(())
        } else {
            Err(Error::unsupported(
                path,
                format!(
                    "{}-bit {:?} samples",
                    self.bits_per_channel, self.sample_format
                ),
            ))
        }
    }
}

/// Turn `bytes` into interleaved samples, sign-extended at their native width.
pub fn decode(bytes: &[u8], format: &PcmFormat, out: &mut [i32]) {
    let big = format.endianness == Endianness::Big;

    match (format.sample_format, format.bits_per_channel) {
        (SampleFormat::Integer, 16) => {
            for (i, sample) in out.iter_mut().enumerate() {
                let b = &bytes[i * 2..i * 2 + 2];
                let raw = if big {
                    i16::from_be_bytes([b[0], b[1]])
                } else {
                    i16::from_le_bytes([b[0], b[1]])
                };
                *sample = raw as i32;
            }
        }
        (SampleFormat::Integer, 24) => {
            for (i, sample) in out.iter_mut().enumerate() {
                let b = &bytes[i * 3..i * 3 + 3];
                let (hi, mid, lo) = if big {
                    (b[0], b[1], b[2])
                } else {
                    (b[2], b[1], b[0])
                };
                // Sign-extend from 24 bits by landing the value in the top
                // three bytes and shifting back down arithmetically.
                let raw = ((hi as i32) << 24) | ((mid as i32) << 16) | ((lo as i32) << 8);
                *sample = raw >> 8;
            }
        }
        (SampleFormat::Integer, _) => {
            for (i, sample) in out.iter_mut().enumerate() {
                let b: [u8; 4] = bytes[i * 4..i * 4 + 4].try_into().unwrap();
                *sample = if big {
                    i32::from_be_bytes(b)
                } else {
                    i32::from_le_bytes(b)
                };
            }
        }
        (SampleFormat::Float, _) => {
            for (i, sample) in out.iter_mut().enumerate() {
                let b: [u8; 4] = bytes[i * 4..i * 4 + 4].try_into().unwrap();
                let value = if big {
                    f32::from_be_bytes(b)
                } else {
                    f32::from_le_bytes(b)
                };
                // Float components are nominally ±1; carry them at 24 bits so
                // one integer path serves every container. Rounded to nearest
                // rather than truncated: truncation is a bias towards zero of
                // up to a whole least significant bit on every sample, which
                // is a quieter signal and not a rounder one. Clamped, because
                // a float payload is not bound to ±1 and the codec's domain
                // is.
                *sample = (value * FLOAT_SCALE).round().clamp(FLOOR_24, CEILING_24) as i32;
            }
        }
    }
}

/// Write interleaved samples back out in `format`'s layout.
pub fn encode(samples: &[i32], format: &PcmFormat, out: &mut [u8]) {
    let big = format.endianness == Endianness::Big;

    match (format.sample_format, format.bits_per_channel) {
        (SampleFormat::Integer, 16) => {
            for (i, &sample) in samples.iter().enumerate() {
                // Clamped, not truncated. `as i16` wraps, which turns a sample
                // one step over full scale into full scale *with the other
                // sign* — an impulse of the loudest kind, in the one place a
                // file ever overflows.
                let b = (sample.clamp(i16::MIN.into(), i16::MAX.into()) as i16).to_be_bytes();
                let b = if big { b } else { [b[1], b[0]] };
                out[i * 2..i * 2 + 2].copy_from_slice(&b);
            }
        }
        (SampleFormat::Integer, 24) => {
            for (i, &sample) in samples.iter().enumerate() {
                let sample = sample.clamp(FLOOR_24 as i32, CEILING_24 as i32);
                let hi = ((sample >> 16) & 0xff) as u8;
                let mid = ((sample >> 8) & 0xff) as u8;
                let lo = (sample & 0xff) as u8;
                let b = if big { [hi, mid, lo] } else { [lo, mid, hi] };
                out[i * 3..i * 3 + 3].copy_from_slice(&b);
            }
        }
        (SampleFormat::Integer, _) => {
            for (i, &sample) in samples.iter().enumerate() {
                let b = if big {
                    sample.to_be_bytes()
                } else {
                    sample.to_le_bytes()
                };
                out[i * 4..i * 4 + 4].copy_from_slice(&b);
            }
        }
        (SampleFormat::Float, _) => {
            for (i, &sample) in samples.iter().enumerate() {
                let value = sample as f32 / FLOAT_SCALE;
                let b = if big {
                    value.to_be_bytes()
                } else {
                    value.to_le_bytes()
                };
                out[i * 4..i * 4 + 4].copy_from_slice(&b);
            }
        }
    }
}

/// Full scale for a 24-bit sample, which is the width float payloads are
/// carried at so one integer path serves every container.
const FLOAT_SCALE: f32 = 8_388_608.0;

/// The widest a 24-bit sample goes, either way.
const CEILING_24: f32 = 8_388_607.0;
const FLOOR_24: f32 = -8_388_608.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(format: PcmFormat, samples: &[i32]) -> Vec<i32> {
        let mut bytes = vec![0u8; samples.len() * format.bytes_per_sample()];
        encode(samples, &format, &mut bytes);
        let mut out = vec![0i32; samples.len()];
        decode(&bytes, &format, &mut out);
        out
    }

    #[test]
    fn twenty_four_bit_extremes_survive_both_ways() {
        let samples = [0, 1, -1, 8_388_607, -8_388_608];
        for endianness in [Endianness::Big, Endianness::Little] {
            let format = PcmFormat {
                endianness,
                ..PcmFormat::master(48000.0, 1)
            };
            assert_eq!(round_trip(format, &samples), samples, "{endianness:?}");
        }
    }

    #[test]
    fn sixteen_and_thirty_two_bit_survive() {
        let format = PcmFormat::wav(48000.0, 1, 16);
        assert_eq!(round_trip(format, &[0, 32767, -32768]), [0, 32767, -32768]);

        let format = PcmFormat::wav(48000.0, 1, 32);
        let samples = [0, i32::MAX, i32::MIN];
        assert_eq!(round_trip(format, &samples), samples);
    }

    #[test]
    fn float_carries_a_twenty_four_bit_payload() {
        let format = PcmFormat {
            sample_format: SampleFormat::Float,
            bits_per_channel: 32,
            ..PcmFormat::wav(48000.0, 1, 32)
        };
        let samples = [0, 4_194_304, -4_194_304];
        assert_eq!(round_trip(format, &samples), samples);
    }

    #[test]
    fn an_unconvertible_layout_is_refused() {
        let format = PcmFormat::wav(48000.0, 2, 12);
        let err = format
            .check_supported(Path::new("t.wav"))
            .expect_err("12-bit was accepted");
        assert!(err.to_string().contains("12-bit"), "{err}");
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    fn format(bits: u32, float: bool) -> PcmFormat {
        PcmFormat {
            sample_format: if float {
                SampleFormat::Float
            } else {
                SampleFormat::Integer
            },
            bits_per_channel: bits,
            endianness: Endianness::Little,
            channels: 1,
            sample_rate: 48_000.0,
        }
    }

    /// A sample one step over full scale is full scale, not full scale with
    /// the other sign.
    ///
    /// Wrapping here is the loudest possible defect: it turns the top of a
    /// waveform into an impulse to the bottom of the range, and it happens
    /// exactly where a file is loudest.
    #[test]
    fn a_sample_over_full_scale_is_clamped_rather_than_wrapped() {
        let samples = [40_000i32, -40_000, 32_767, -32_768, 0];
        let mut out = vec![0u8; samples.len() * 2];
        encode(&samples, &format(16, false), &mut out);
        let mut back = vec![0i32; samples.len()];
        decode(&out, &format(16, false), &mut back);
        assert_eq!(back, vec![32_767, -32_768, 32_767, -32_768, 0]);
    }

    /// And the same at twenty-four bits, where the low bits are taken by hand.
    #[test]
    fn twenty_four_bits_clamp_too() {
        let samples = [9_000_000i32, -9_000_000, 8_388_607, -8_388_608, -1];
        let mut out = vec![0u8; samples.len() * 3];
        encode(&samples, &format(24, false), &mut out);
        let mut back = vec![0i32; samples.len()];
        decode(&out, &format(24, false), &mut back);
        assert_eq!(back, vec![8_388_607, -8_388_608, 8_388_607, -8_388_608, -1]);
    }

    /// Float components round to the nearest integer sample rather than
    /// towards zero, which is a bias of up to a whole bit on every sample.
    #[test]
    fn float_components_round_rather_than_truncate() {
        // Three quarters of a least significant bit: it rounds up, and
        // truncation would lose it.
        let value = 0.75f32 / FLOAT_SCALE;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&(-value).to_le_bytes());
        let mut back = vec![0i32; 2];
        decode(&bytes, &format(32, true), &mut back);
        assert_eq!(back, vec![1, -1], "0.75 of a bit should round to one");
    }

    /// A float payload is not bound to ±1, and the codec's domain is.
    #[test]
    fn a_float_over_one_lands_at_full_scale() {
        let mut bytes = Vec::new();
        for value in [2.0f32, -2.0, 1.0, -1.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let mut back = vec![0i32; 4];
        decode(&bytes, &format(32, true), &mut back);
        assert_eq!(
            back,
            vec![8_388_607, -8_388_608, 8_388_607, -8_388_608],
            "an over-range float should saturate rather than wrap"
        );
    }
}
