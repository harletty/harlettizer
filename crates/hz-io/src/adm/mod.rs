//! The Audio Definition Model: what a BW64 file says about its own tracks.
//!
//! Two chunks carry it. `chna` is a fixed-width binary table mapping each
//! track to three identifiers; `axml` is a BS.2076 XML document describing
//! what those identifiers mean.

pub mod bw64;
pub mod chna;
pub mod model;
pub mod time;
pub mod xml;

pub use bw64::Description;
pub use chna::{AudioId, Chna};
pub use model::{
    AudioBlockFormat, AudioChannelFormat, AudioContent, AudioFormatExtended, AudioObject,
    AudioPackFormat, AudioProgramme, AudioStreamFormat, AudioTrackFormat, AudioTrackUid, Frequency,
    JumpPosition, Position, TypeDefinition,
};
pub use time::Time;
pub use xml::{Ignored, Parsed};
