//! A raw stream cut into frames.
//!
//! A DTS-HD stream is a sequence of `[core][extension substream]` pairs, or
//! of extension substreams alone when the asset has no core. The core says
//! how long it is in its header; the substream in its own. This finds each
//! from the sync word at the current position — never by scanning, since
//! both sync words also occur by chance inside audio data.

use crate::{core, exss};
use hz_core::bits_read::Ran;

/// One frame's two halves, either of which may be absent.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// Byte offset of the frame in the stream.
    pub at: usize,
    pub core: Option<&'a [u8]>,
    pub exss: Option<&'a [u8]>,
}

/// The frames of a stream, in order.
pub struct Frames<'a> {
    stream: &'a [u8],
    at: usize,
}

/// Iterate over the frames of `stream`. A frame that cannot be cut ends the
/// iteration with an error naming its offset.
pub fn frames(stream: &[u8]) -> Frames<'_> {
    Frames { stream, at: 0 }
}

fn sync_at(stream: &[u8], at: usize) -> Option<u32> {
    stream
        .get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

impl<'a> Iterator for Frames<'a> {
    type Item = Result<Frame<'a>, Ran>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.stream.len() {
            return None;
        }
        let start = self.at;
        let result = (|| {
            let mut at = start;
            let mut core = None;
            if sync_at(self.stream, at) == Some(core::SYNC) {
                let header = core::CoreHeader::read(&self.stream[at..])?;
                let end = at + header.frame_size as usize;
                core = Some(
                    self.stream
                        .get(at..end)
                        .ok_or_else(|| Ran(format!("the core frame at {at} runs past the end")))?,
                );
                at = end;
            }
            let mut exss = None;
            if sync_at(self.stream, at) == Some(exss::SYNC) {
                let end = at + exss::substream_size(&self.stream[at..])?;
                exss = Some(self.stream.get(at..end).ok_or_else(|| {
                    Ran(format!("the extension substream at {at} runs past the end"))
                })?);
                at = end;
            }
            if at == start {
                return Err(Ran(format!("no sync word at byte {start}")));
            }
            Ok((
                at,
                Frame {
                    at: start,
                    core,
                    exss,
                },
            ))
        })();
        match result {
            Ok((next, frame)) => {
                self.at = next;
                Some(Ok(frame))
            }
            Err(error) => {
                self.at = self.stream.len();
                Some(Err(error))
            }
        }
    }
}
