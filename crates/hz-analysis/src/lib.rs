//! Measurement: how loud something is, and how far it peaks.
//!
//! The encoding jobs this engine has to reproduce all carry measured
//! metadata — a dialogue normalisation figure, a dynamic range profile, a
//! true peak — and every one of them is a number some other tool will check.
//! So this crate is written to be checkable: the filter design is asserted
//! against the coefficients the recommendation publishes, and the meter is
//! asserted against closed forms rather than against remembered answers.

pub mod bands;
pub mod drc;
mod fft;
pub mod kweighting;
pub mod loudness;
pub mod partial;
pub mod speech;
pub mod truepeak;

pub use drc::{Characteristic, Compr, DialNorm, Dynrng};
pub use loudness::{ChannelWeight, Meter, SILENCE_LUFS};
pub use speech::{Analysis, DialogueMeter, SpeechGate, Thresholds};
pub use truepeak::TruePeak;
