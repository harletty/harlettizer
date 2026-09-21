//! Audio containers.
//!
//! Every container here agrees on its payload and differs only in its header,
//! so the sample layout and the byte conversion live in [`pcm`] and the
//! container modules deal with framing.

pub mod caf;
pub mod mono;
pub mod pcm;
pub mod wav;

pub use pcm::{Endianness, PcmFormat, SampleFormat};

use hz_core::Result;

/// Interleaved integer frames out of a container, whichever container it is.
///
/// The readers here differ in their headers and agree on everything after
/// them: same `pcm` decode, same sign-extended `i32` at the file's own width,
/// same "how many frames landed" contract. A caller that has opened one and
/// only wants its audio should not have to know which — and one that matches
/// on the container to call the same method twice is carrying the distinction
/// past the point where it means anything.
/// `Send`, for the same reason a file handle is: a reader is a handle, a
/// format and a decode buffer, and nothing about it is tied to the thread that
/// opened it. Requiring it here is what lets a folding encode read and mix on
/// a thread of its own while another writes.
pub trait FrameSource: Send {
    /// The sample layout, which is what a caller needs to size a buffer and to
    /// scale the values.
    fn pcm_format(&self) -> &PcmFormat;

    /// Total frames in the file.
    fn frame_count(&self) -> u64;

    /// Fill `out` with interleaved samples, returning how many frames landed.
    ///
    /// Zero means the end of the file, not an empty read to retry.
    fn read_frames(&mut self, out: &mut [i32]) -> Result<usize>;
}

/// A chunk a reader did not interpret, kept so a writer can put it back.
///
/// A channel layout, an ADM payload, a maker's note — dropping one on a
/// rewrite is silent corruption that surfaces only in somebody else's
/// decoder, so containers here carry them through rather than skip them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub kind: [u8; 4],
    pub body: Vec<u8>,
}
