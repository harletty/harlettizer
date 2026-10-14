//! DTS Coherent Acoustics for harlettizer.
//!
//! Track C of `PLAN.md`: DTS-HD Master Audio with the DTS:X extension. This
//! is phase C0, the groundwork — the headers every later phase writes into,
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
//! Nothing here encodes audio yet. The core encoder is phase C1, the lossless
//! layer C2, the DTS:X extension C3.

pub mod bits;
pub mod core;
pub mod crc;
pub mod exss;
pub mod navigation;
pub mod split;
