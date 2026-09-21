//! Object audio metadata: what the elements of an immersive programme are,
//! where they go, and how loudly.
//!
//! A delivery bitstream carries this as a payload of its own — TrueHD in the
//! extra data of an access unit, E-AC-3 in an Evolution frame of a syncframe —
//! and the payload is the same in both. So it lives here rather than in either
//! codec crate, which is also why this crate depends on neither.
//!
//! What is modelled is the shape a real immersive programme uses and not the
//! whole of the syntax: every element a dynamic object, one of which may be
//! the low frequency channel, with a position, a gain and the handful of
//! rendering flags that go with them. A payload using a shape this does not
//! model is refused by name rather than approximated — see [`oamd::Unreadable`].

pub mod oamd;

pub use oamd::ObjectAudioMetadata;
