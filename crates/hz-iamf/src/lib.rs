//! IAMF — Immersive Audio Model and Formats — written as a standalone IA
//! sequence.
//!
//! IAMF is the Alliance for Open Media's immersive container: a bed, a scene
//! or (from v2.0) objects, coded by an ordinary codec one or two channels at
//! a time and described by a handful of small descriptors. What this crate
//! writes is a channel bed — losslessly as LPCM or as FLAC through an encoder
//! of its own, or as Opus through libopus behind the `opus` feature — under
//! the v1.1 simple profile, which is what browsers and televisions decode
//! today.
//!
//! ```no_run
//! use hz_iamf::{Codec, Config, Headphones, Loudness, Writer, layout};
//!
//! let config = Config {
//!     layout: layout::SEVEN_ONE_FOUR,
//!     codec: Codec::Flac,
//!     sample_rate: 48_000,
//!     bits: 24,
//!     frame: 4096,
//!     headphones: Headphones::Stereo,
//! };
//! let mut writer = Writer::new(std::fs::File::create("programme.iamf")?, config)?;
//! writer.push(&vec![0i32; 4096 * 12])?;
//! let silence = Loudness { integrated: f64::NEG_INFINITY, digital_peak: f64::NEG_INFINITY, true_peak: f64::NEG_INFINITY };
//! writer.finish([silence; 2])?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod flac;
pub mod layout;
pub mod obu;
#[cfg(feature = "opus")]
pub mod opus;
pub mod stream;

pub use layout::Layout;
pub use stream::{Codec, Config, Error, Headphones, Loudness, Writer};
