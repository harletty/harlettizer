//! What the encoder needs to know about where a sound goes.
//!
//! An encoding job carries several presentations — 7.1, 5.1, a stereo pair —
//! and only the largest is authored. The rest are folds, and this crate is
//! what the encoder knows about folding: the [`Layout`]s a presentation is
//! laid out on, the fold model measured off shipped streams
//! ([`ObjectFold`]), and the [`Panner`] — vector-base amplitude panning over
//! a triangulated layout — that the clustering metric judges the immersive
//! presentation on.

pub mod fold;
pub mod keyframe;
pub mod layout;
pub mod panner;
pub mod render;

pub use fold::{FoldMode, ObjectFold, elevation_scale};
pub use keyframe::{Keyframe, Mode};
pub use layout::Layout;
pub use panner::{Panner, PointPanner, speakers_of};
pub use render::Renderer;
