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
//! Phase C2 is the lossless layer — every channel coded as itself (C2a), or
//! the channels the core carries as their difference from it (C2b) — and the
//! stream that carries it beside the core:
//!
//! - [`xll`]: the lossless frame's headers — common header, channel set
//!   sub-headers, navigation table — read and written;
//! - [`core_decode`]: the core as a decoder reconstructs it, which the
//!   lossless layer's residuals are taken against;
//! - [`lossless`]: PCM to lossless frames, 5.1 or 7.1 at 16 or 24 bits, the
//!   7.1's rears folded into the 5.1 below them;
//! - [`master`]: DTS-HD Master Audio, the core and the lossless layer frame
//!   by frame, the core a frame ahead.
//!
//! Phase C3 is the DTS:X extension's standard profile, a fixed 7.1.4: the
//! heights folded into the bed's corners and carried beside it.
//!
//! - [`dtsx`]: the extension block — the fold matrix, the height set — and
//!   the fold; [`lossless`] and [`master`] write it.
//!
//! Phase C4 holds the stream under a peak bit rate ceiling:
//!
//! - [`peak`]: the decoder's smoothing buffer, as both decoders keep it, and
//!   the schedule that sends a frame too large for its substream partly
//!   ahead into it; [`master`] sends its substreams through it.

pub mod adpcm;
pub mod bits;
pub mod core;
pub mod core_decode;
pub mod crc;
pub mod dtsx;
pub mod encoder;
pub mod exss;
mod fields;
pub mod lossless;
pub mod master;
pub mod mdct;
pub mod navigation;
pub mod peak;
pub mod split;
pub mod tables;
pub mod xll;
