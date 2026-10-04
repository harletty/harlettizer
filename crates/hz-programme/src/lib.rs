//! What every writer does to a programme before its own format.
//!
//! harlettizer writes a master as a TrueHD stream or as an IAMF sequence,
//! and most of the way there is the same road: open the master and read its
//! frames ([`source`]), sort its tracks into the LFE, bed channels and
//! objects ([`part`]), follow an object's updates as a path through the room
//! ([`path`]) or as a render onto a layout ([`follow`]), mix objects into
//! elements a block at a time — a fold or an overlay ([`mix`],
//! [`overlay`]) — and code what comes out into a writer's integers
//! ([`quantise`]), reporting how far along it is ([`progress`]).
//!
//! None of it knows a format. The dependency rule is the codec crates'
//! own: this crate depends on neither `hz-mlp` nor `hz-iamf`, and a writer
//! hands it whatever its format decides — the grid a position is coded on,
//! the gain a decoder applies to an element, the domain of its integers.

pub mod follow;
pub mod mix;
pub mod overlay;
pub mod part;
pub mod path;
pub mod progress;
pub mod quantise;
pub mod source;

pub use part::{Part, parts_of};
