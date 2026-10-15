//! DTS Coherent Acoustics for harlettizer.
//!
//! Track C of `PLAN.md`: DTS-HD Master Audio with the DTS:X extension.
//! Phase C0 laid the groundwork — the headers every later phase writes into,
//! each with the reader it is checked against:
//!
//! - [`core`]: the core frame header (ETSI TS 102 114 §5.3.1);
//! - [`exss`]: the extension substream header and its asset descriptors
//!   (§7.4–7.5), which is where the lossless component is navigated from;
//! - [`navigation`]: the DTS:X extension's navigation word, read from streams
//!   rather than from a specification, with the limits that implies;
//! - [`crc`]: the header checksum;
//! - [`split`]: a raw stream cut into its frames.
//!
//! Phase C1 is the core encoder, FFmpeg's ported, which writes its header
//! through [`core`]:
//!
//! - [`encoder`]: PCM to core frames, up to 5.1;
//! - [`mdct`]: the fixed-point transform its masking model runs on;
//! - [`adpcm`]: subband prediction, by choice;
//! - [`tables`]: the format's tables and the encoder's own.
//!
//! The lossless layer is phase C2, the DTS:X extension C3.

pub mod adpcm;
pub mod bits;
pub mod core;
pub mod crc;
pub mod encoder;
pub mod exss;
pub mod mdct;
pub mod navigation;
pub mod split;
pub mod tables;
