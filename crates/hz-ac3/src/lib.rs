//! AC-3 encoding.

pub mod bitalloc;
pub mod encoder;
pub mod mdct;
pub mod tables;

pub use encoder::{
    ChannelMode, Config, DELAY, Encoder, FRAME_SAMPLES, Format, Metadata, Unsupported,
};
