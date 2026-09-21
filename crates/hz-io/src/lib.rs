//! Reading and writing the file formats an encoding job starts and ends with.
//!
//! Phase 1 covers the master file set — the config, the per-object event
//! stream, and the audio component that goes with them. Container and ADM
//! support follow in the same phase.
//!
//! # Why this crate owns its own model of the master format
//!
//! A model of this format already exists in the decoder's `damf` crate, and
//! reusing it was the obvious first thought. It does not work, for three
//! reasons that are worth writing down so nobody re-litigates them:
//!
//! 1. **It is a writer.** Its fields are private and its constructors project
//!    from decoded object metadata. An encoder builds masters from ADM and
//!    from PCM, which is the other direction entirely.
//! 2. **It is lossy on input.** Its top-level type has no `language` field, so
//!    reading a master that has one and writing it back silently drops it.
//!    Nothing in serde's defaults would have told us; the field would just be
//!    gone.
//! 3. **It cannot read what it writes.** `fps: 24` is a YAML number and its
//!    frame-rate type only accepts strings.
//!
//! So the model here is our own, and it is strict: every struct is
//! `deny_unknown_fields`, which turns a key we failed to model into a loud
//! failure instead of silent data loss. Run over every real master seen, that
//! is a completeness proof rather than an assertion — and it caught four
//! fields (`sourceCodec`, `warpMode`, `trimMode`, `language`) that a
//! permissive model would have quietly discarded.

pub mod adm;
pub mod container;
pub mod master;
pub mod project;
pub mod settings;
mod yaml;

pub use yaml::{Num, Real};
