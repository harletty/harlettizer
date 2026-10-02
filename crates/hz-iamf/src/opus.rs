//! Opus packets, as an IA sequence carries them (IAMF §3.11.1), through
//! libopus.
//!
//! The one codec in this crate that is not written here. A lossy coder is
//! judged by ear and by years of tuning, and libopus is the reference
//! encoder the format was tuned with; the pure-Rust ports in circulation are
//! young, and an encoder that sounds worse is not one to ship for the sake of
//! a toolchain. So this module is behind the `opus` feature, links the
//! system's libopus through `pkg-config`, and is the only `unsafe` in the
//! crate: five functions of a C API, wrapped so that nothing outside this
//! file can misuse them.
//!
//! IAMF's constraints are few. Each audio frame is one Opus packet of one or
//! two channels at 48 kHz; the codec config is the Ogg Opus identification
//! header without its magic, big-endian, with two channels, no output gain
//! and mapping family 0; and the pre-skip — the encoder's lookahead — is the
//! number of samples every substream trims off its start.

use std::ffi::{CStr, c_char, c_int};
use std::fmt;

/// The rate Opus runs at, and the rate IAMF counts its samples in.
pub const SAMPLE_RATE: u32 = 48_000;

/// An Opus packet's most samples a channel: 60 ms.
const MAX_FRAME: usize = 2880;

/// The largest packet libopus will write, per its own documentation's
/// recommendation for a buffer.
const MAX_PACKET: usize = 4000;

// The subset of `opus_defines.h` this uses.
const OPUS_OK: c_int = 0;
const OPUS_APPLICATION_AUDIO: c_int = 2049;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_MAX_BANDWIDTH_REQUEST: c_int = 4004;
const OPUS_SET_VBR_REQUEST: c_int = 4006;
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;
const OPUS_SET_LSB_DEPTH_REQUEST: c_int = 4036;
const OPUS_SIGNAL_MUSIC: c_int = 3002;
const OPUS_BANDWIDTH_NARROWBAND: c_int = 1101;

#[repr(C)]
struct OpusEncoder {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    fn opus_encode_float(
        st: *mut OpusEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    fn opus_encoder_destroy(st: *mut OpusEncoder);
    fn opus_strerror(error: c_int) -> *const c_char;
}

/// What libopus refused, in its own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusError {
    pub what: &'static str,
    pub message: String,
}

impl fmt::Display for OpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "libopus: {}: {}", self.what, self.message)
    }
}

impl std::error::Error for OpusError {}

fn check(what: &'static str, code: c_int) -> Result<c_int, OpusError> {
    if code >= OPUS_OK {
        return Ok(code);
    }
    // SAFETY: `opus_strerror` returns a pointer to a static, nul-terminated
    // string for any code, known or not.
    let message = unsafe { CStr::from_ptr(opus_strerror(code)) }
        .to_string_lossy()
        .into_owned();
    Err(OpusError { what, message })
}

/// How a substream is coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub channels: usize,
    /// Bits a second for the whole substream.
    pub bitrate: u32,
    /// An LFE: band-limited to the narrowest band, which is all it carries.
    pub lfe: bool,
}

/// One substream's encoder.
///
/// Stateful, unlike the lossless ones: a packet depends on the samples
/// before it, so each substream has an encoder of its own for the length of
/// the sequence.
pub struct Encoder {
    raw: *mut OpusEncoder,
    channels: usize,
    lookahead: usize,
    packet: Vec<u8>,
}

// SAFETY: an `OpusEncoder` is a plain heap block owned by this value alone;
// libopus keeps no global or thread-local state tied to it.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(settings: Settings) -> Result<Self, OpusError> {
        assert!(
            matches!(settings.channels, 1 | 2),
            "an IAMF Opus substream is mono or stereo"
        );
        let mut error = 0;
        // SAFETY: the arguments are plain integers and a valid out-pointer.
        let raw = unsafe {
            opus_encoder_create(
                SAMPLE_RATE as i32,
                settings.channels as c_int,
                OPUS_APPLICATION_AUDIO,
                &mut error,
            )
        };
        check("creating an encoder", error)?;
        if raw.is_null() {
            return Err(OpusError {
                what: "creating an encoder",
                message: "no encoder returned".into(),
            });
        }
        let mut encoder = Self {
            raw,
            channels: settings.channels,
            lookahead: 0,
            packet: vec![0; MAX_PACKET],
        };
        let bitrate = settings.bitrate.min(i32::MAX as u32) as c_int;
        encoder.set("setting the bitrate", OPUS_SET_BITRATE_REQUEST, bitrate)?;
        // Unconstrained VBR: a file, not a channel with a fixed pipe.
        encoder.set("setting VBR", OPUS_SET_VBR_REQUEST, 1)?;
        encoder.set("setting the complexity", OPUS_SET_COMPLEXITY_REQUEST, 10)?;
        encoder.set(
            "setting the signal",
            OPUS_SET_SIGNAL_REQUEST,
            OPUS_SIGNAL_MUSIC,
        )?;
        // The samples arrive from a 24-bit bed.
        encoder.set("setting the depth", OPUS_SET_LSB_DEPTH_REQUEST, 24)?;
        if settings.lfe {
            encoder.set(
                "limiting the band",
                OPUS_SET_MAX_BANDWIDTH_REQUEST,
                OPUS_BANDWIDTH_NARROWBAND,
            )?;
        }
        let mut lookahead: c_int = 0;
        // SAFETY: GET_LOOKAHEAD takes one `opus_int32 *`, which this is.
        let code = unsafe {
            opus_encoder_ctl(
                encoder.raw,
                OPUS_GET_LOOKAHEAD_REQUEST,
                &mut lookahead as *mut c_int,
            )
        };
        check("reading the lookahead", code)?;
        encoder.lookahead = lookahead as usize;
        Ok(encoder)
    }

    fn set(&mut self, what: &'static str, request: c_int, value: c_int) -> Result<(), OpusError> {
        // SAFETY: every SET request used here takes one `opus_int32` by value.
        let code = unsafe { opus_encoder_ctl(self.raw, request, value) };
        check(what, code).map(|_| ())
    }

    /// Samples a channel the decoder's output lags the input by: the
    /// pre-skip.
    pub fn lookahead(&self) -> usize {
        self.lookahead
    }

    /// Code one frame of interleaved samples, full scale ±1. The packet is
    /// the encoder's until the next call.
    pub fn encode(&mut self, interleaved: &[f32]) -> Result<&[u8], OpusError> {
        let frame = interleaved.len() / self.channels;
        assert_eq!(
            frame * self.channels,
            interleaved.len(),
            "whole frames only"
        );
        assert!(
            is_frame_size(frame) && frame <= MAX_FRAME,
            "{frame} samples is not an Opus frame"
        );
        // SAFETY: `interleaved` holds `frame` frames of `channels` samples,
        // and `packet` is `MAX_PACKET` writable bytes, as the call is told.
        let written = unsafe {
            opus_encode_float(
                self.raw,
                interleaved.as_ptr(),
                frame as c_int,
                self.packet.as_mut_ptr(),
                MAX_PACKET as i32,
            )
        };
        let written = check("encoding a frame", written)? as usize;
        Ok(&self.packet[..written])
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `opus_encoder_create` and is freed once.
        unsafe { opus_encoder_destroy(self.raw) };
    }
}

/// Whether `samples` at 48 kHz is a duration an Opus packet can have: 2.5,
/// 5, 10, 20, 40 or 60 ms.
pub fn is_frame_size(samples: usize) -> bool {
    matches!(samples, 120 | 240 | 480 | 960 | 1920 | 2880)
}

/// The codec config's `decoder_config`: the identification header of RFC
/// 7845 without its magic, every field big-endian (IAMF §3.11.1).
pub fn decoder_config(pre_skip: u16, input_sample_rate: u32) -> [u8; 11] {
    let mut out = [0u8; 11];
    out[0] = 1; // version
    out[1] = 2; // output channel count, which IAMF fixes at two
    out[2..4].copy_from_slice(&pre_skip.to_be_bytes());
    out[4..8].copy_from_slice(&input_sample_rate.to_be_bytes());
    // Output gain nought, mapping family nought.
    out
}

/// `audio_roll_distance` for Opus frames of `frame` samples: enough frames
/// back to cover the 80 ms a decoder needs to converge, negated (IAMF
/// §3.5).
pub fn roll_distance(frame: usize) -> i16 {
    -(3840usize.div_ceil(frame) as i16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libopus's own decoder, for the round trip. Not independent of the
    /// encoder's library, but independent of everything this file decides —
    /// framing, the pre-skip, the alignment — which is what is under test;
    /// FFmpeg's native decoder checks the whole sequence in docs/iamf.md.
    /// (The pure-Rust `opus-decoder` was the first choice and overflows a
    /// shift in its anti-collapse mask on these very packets.)
    struct Decoder {
        raw: *mut std::ffi::c_void,
        channels: usize,
    }

    unsafe extern "C" {
        fn opus_decoder_create(
            fs: i32,
            channels: c_int,
            error: *mut c_int,
        ) -> *mut std::ffi::c_void;
        fn opus_decode_float(
            st: *mut std::ffi::c_void,
            data: *const u8,
            len: i32,
            pcm: *mut f32,
            frame_size: c_int,
            decode_fec: c_int,
        ) -> c_int;
        fn opus_decoder_destroy(st: *mut std::ffi::c_void);
    }

    impl Decoder {
        fn new(channels: usize) -> Self {
            let mut error = 0;
            // SAFETY: plain integers and a valid out-pointer.
            let raw = unsafe { opus_decoder_create(48_000, channels as c_int, &mut error) };
            assert!(error == OPUS_OK && !raw.is_null());
            Self { raw, channels }
        }

        fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> usize {
            // SAFETY: `out` holds `MAX_FRAME` frames of `channels` samples.
            let n = unsafe {
                opus_decode_float(
                    self.raw,
                    packet.as_ptr(),
                    packet.len() as i32,
                    out.as_mut_ptr(),
                    (out.len() / self.channels) as c_int,
                    0,
                )
            };
            assert!(n > 0, "decode failed: {n}");
            n as usize
        }
    }

    impl Drop for Decoder {
        fn drop(&mut self) {
            // SAFETY: created once, destroyed once.
            unsafe { opus_decoder_destroy(self.raw) };
        }
    }

    fn tone(frames: usize, channels: usize, period: f64) -> Vec<f32> {
        (0..frames * channels)
            .map(|i| {
                let t = (i / channels) as f64;
                let p = period * (1.0 + 0.5 * (i % channels) as f64);
                (0.5 * (std::f64::consts::TAU * t / p).sin()) as f32
            })
            .collect()
    }

    /// What an independent decoder hands back, the pre-skip removed, lines
    /// up with what went in — sample for sample, not just in level.
    #[test]
    fn a_tone_comes_back_aligned_after_the_pre_skip() {
        for channels in [1, 2] {
            let mut encoder = Encoder::new(Settings {
                channels,
                bitrate: 96_000 * channels as u32,
                lfe: false,
            })
            .unwrap();
            let pre_skip = encoder.lookahead();
            assert!(pre_skip > 0 && pre_skip < 960, "{pre_skip}");
            let frames = 960 * 50;
            let input = tone(frames, channels, 97.0);
            let mut decoder = Decoder::new(channels);
            let mut decoded = Vec::new();
            let mut pcm = vec![0f32; MAX_FRAME * 2];
            for chunk in input.chunks(960 * channels) {
                let packet = encoder.encode(chunk).unwrap().to_vec();
                let n = decoder.decode(&packet, &mut pcm);
                decoded.extend_from_slice(&pcm[..n * channels]);
            }
            let aligned = &decoded[pre_skip * channels..];
            // Skip the first frames, where the codec is still converging.
            let start = 960 * 5 * channels;
            let end = aligned.len().min(input.len());
            let (mut signal, mut error) = (0.0f64, 0.0f64);
            for (a, b) in aligned[start..end].iter().zip(&input[start..end]) {
                signal += f64::from(*b).powi(2);
                error += f64::from(a - b).powi(2);
            }
            let snr = 10.0 * (signal / error).log10();
            assert!(snr > 20.0, "{channels} channels: {snr:.1} dB");
        }
    }

    #[test]
    fn the_header_is_the_id_header_big_endian() {
        assert_eq!(
            decoder_config(312, 48_000),
            [1, 2, 0x01, 0x38, 0, 0, 0xbb, 0x80, 0, 0, 0]
        );
        assert_eq!(roll_distance(960), -4);
        assert_eq!(roll_distance(2880), -2);
    }
}
