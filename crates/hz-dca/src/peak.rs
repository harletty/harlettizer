//! Peak bit rate: the decoder's smoothing buffer.
//!
//! A lossless frame need not sit whole in the substream that carries it.
//! The lossless component of a substream is a run of bytes from one stream
//! of lossless frames laid end to end, and a decoder appends each run to a
//! buffer and takes one frame out of it per substream (ETSI TS 102 114
//! §7.5, `nuPeakBRCntrlBuffSzkB`; FFmpeg's `dca_xll.c` and the `dca` crate
//! both implement it so). A large frame can therefore travel partly in the
//! substreams before its own, ahead of time, and no substream has to carry
//! more than the channel allows: that is peak bit rate smoothing.
//!
//! [`DecoderBuffer`] is that buffer, kept exactly as both decoders keep it,
//! to check a stream against. [`Smoother`] is the encoder's side: which of
//! the lossless stream's bytes each substream carries.

use crate::exss::XllSync;
use crate::xll;
use std::collections::VecDeque;

/// The largest smoothing buffer the format states: 240 kB, the field's
/// fifteen steps of 16 kB. It is also FFmpeg's buffer.
pub const MAX_BUFFER: usize = 240 << 10;

/// The ceiling a stream is held under by default, core included: 24.5 Mb/s.
pub const CEILING: u32 = 24_500_000;

/// What a stream is held under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Bits a second for the whole stream, the core and every header
    /// included, frame by frame.
    pub ceiling: u32,
    /// The decoder's smoothing buffer the stream assumes, in steps of 16 kB
    /// (`nuPeakBRCntrlBuffSzkB`), 0 to 15: 0 sends nothing ahead.
    pub buffer_code: u8,
    /// Frames the encoder looks ahead when it may have to send ahead.
    pub lookahead: usize,
}

impl Default for Config {
    /// [`CEILING`]; the 128 kB buffer the smoothing stream measured states;
    /// 1024 frames ahead, about eleven seconds at 48 kHz.
    fn default() -> Self {
        Self {
            ceiling: CEILING,
            buffer_code: 8,
            lookahead: 1024,
        }
    }
}

/// The bytes of a buffer stated as `code`.
pub fn buffer_bytes(code: u8) -> usize {
    usize::from(code) << 14
}

/// The most bytes a frame of `samples` at `rate` Hz may take under
/// `ceiling` bits a second, rounded down.
pub fn frame_budget(ceiling: u32, samples: u32, rate: u32) -> usize {
    (u64::from(ceiling) * u64::from(samples) / (8 * u64::from(rate))) as usize
}

/// What a substream did to the decoder's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// A lossless frame of this many bytes was decoded.
    Frame(usize),
    /// The stated initial delay is still running: nothing decoded.
    Delayed,
}

/// Why a substream left the decoder without a frame to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Starved {
    /// The buffer is empty and the data neither starts a frame nor says
    /// where one starts.
    Unsynchronised,
    /// The frame due is not all there yet.
    Incomplete { frame: usize, held: usize },
    /// The data does not fit the decoder's buffer.
    Overflow { held: usize },
    /// The frame's common header does not read.
    Unreadable(String),
}

/// The decoder's smoothing buffer: what of the lossless stream it holds
/// between substreams, and the delay it still waits out.
#[derive(Debug, Default, Clone)]
pub struct DecoderBuffer {
    held: Vec<u8>,
    delay: u32,
    /// The frame last decoded.
    frame: Vec<u8>,
    /// The most bytes held at once: a substream's data appended to what
    /// was left, before the frame due is taken out.
    peak: usize,
}

fn starts_frame(data: &[u8]) -> bool {
    data.len() >= 4 && u32::from_be_bytes([data[0], data[1], data[2], data[3]]) == xll::SYNC
}

fn frame_size(data: &[u8]) -> Result<usize, Starved> {
    xll::CommonHeader::read(data)
        .map(|c| c.frame_size as usize)
        .map_err(|e| Starved::Unreadable(e.0))
}

impl DecoderBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one substream's lossless data, with its sync fields when it has
    /// them, and decode the frame due — as `ff_dca_xll_parse` does.
    pub fn push(&mut self, data: &[u8], sync: Option<&XllSync>) -> Result<Taken, Starved> {
        if self.held.is_empty() && self.delay == 0 {
            // Nothing buffered: the data is decoded in place, from its
            // first byte or from the sync offset it states.
            let data = if starts_frame(data) {
                data
            } else {
                match sync {
                    Some(s) if (s.sync_offset as usize) < data.len() => {
                        let from = &data[s.sync_offset as usize..];
                        if s.delay_frames > 0 {
                            self.held.extend_from_slice(from);
                            self.delay = s.delay_frames;
                            self.peak = self.peak.max(self.held.len());
                            return Ok(Taken::Delayed);
                        }
                        from
                    }
                    _ => return Err(Starved::Unsynchronised),
                }
            };
            self.peak = self.peak.max(data.len());
            let size = frame_size(data)?;
            if size > data.len() {
                return Err(Starved::Incomplete {
                    frame: size,
                    held: data.len(),
                });
            }
            self.frame.clear();
            self.frame.extend_from_slice(&data[..size]);
            self.held.extend_from_slice(&data[size..]);
            return Ok(Taken::Frame(size));
        }
        if data.len() > MAX_BUFFER - self.held.len() {
            let held = self.held.len() + data.len();
            self.clear();
            return Err(Starved::Overflow { held });
        }
        self.held.extend_from_slice(data);
        self.peak = self.peak.max(self.held.len());
        if self.delay > 0 {
            self.delay -= 1;
            if self.delay > 0 {
                return Ok(Taken::Delayed);
            }
        }
        let size = match frame_size(&self.held) {
            Ok(size) if size <= self.held.len() => size,
            Ok(size) => {
                let held = self.held.len();
                self.clear();
                return Err(Starved::Incomplete { frame: size, held });
            }
            Err(e) => {
                self.clear();
                return Err(e);
            }
        };
        self.frame.clear();
        self.frame.extend_from_slice(&self.held[..size]);
        self.held.drain(..size);
        Ok(Taken::Frame(size))
    }

    /// The frame last decoded.
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    /// Bytes held for the frames after the one last decoded.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The most bytes held at once so far, the frame being decoded
    /// included.
    pub fn peak(&self) -> usize {
        self.peak
    }

    /// Forget everything held, as both decoders do after an error.
    pub fn clear(&mut self) {
        self.held.clear();
        self.delay = 0;
    }
}

/// The width a stream states for an initial delay of `delay` frames: one
/// bit up to a delay of one, then one more than the bits of `delay` − 1, as
/// every one of the 1.1 million substreams of the smoothing stream measured
/// has it (a delay of 2 in two bits, 3 and 4 in three, 5 to 8 in four, 9 to
/// 14 in five).
pub fn delay_bits(delay: u32) -> u8 {
    if delay <= 1 {
        1
    } else {
        ((delay - 1).ilog2() + 2) as u8
    }
}

/// Why a frame cannot reach the decoder in time under the ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overrun {
    /// The lossless data still to send up to the end of the frame is
    /// `excess` bytes more than the `substreams` up to its own can carry,
    /// each as much as the ceiling allows.
    Ceiling { excess: u64, substreams: u64 },
    /// Carrying it in time would have the decoder hold `held` bytes, more
    /// than the `capacity` of the buffer the stream states.
    Buffer { held: u64, capacity: usize },
}

impl std::fmt::Display for Overrun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Overrun::Ceiling {
                excess,
                substreams: 1,
            } => write!(
                f,
                "its lossless data is {excess} bytes more than its substream can carry under \
                 the ceiling"
            ),
            Overrun::Ceiling { excess, substreams } => write!(
                f,
                "the lossless data still to send up to it is {excess} bytes more than the \
                 {substreams} substreams up to its own can carry under the ceiling"
            ),
            Overrun::Buffer { held, capacity } => write!(
                f,
                "carrying it in time would hold {held} bytes in the decoder's smoothing buffer, \
                 which the stream states as {capacity}"
            ),
        }
    }
}

/// Where a substream's lossless data lets a decoder that starts there
/// synchronise: the first frame that starts in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sync {
    /// Bytes from the start of the data to that frame.
    pub offset: u32,
    /// Substreams after this one whose data the decoder takes that frame
    /// out with: 0 when it is this substream's own frame.
    pub delay: u32,
}

/// What one substream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Carry {
    /// The frame the decoder takes out of its buffer with this substream,
    /// counted from 0.
    pub frame: u64,
    /// Bytes of the lossless stream the substream carries, from where the
    /// one before stopped.
    pub bytes: usize,
    /// Absent when no frame starts in them.
    pub sync: Option<Sync>,
}

/// The encoder's side of the smoothing buffer: given the lossless frames'
/// sizes, how many bytes of them each substream carries.
///
/// The schedule is the one the smoothing stream measured follows, and the
/// latest one there is: a frame goes in its own substream, whole, unless a
/// frame after it is too large for its own — then the substreams before
/// that one carry its excess, as late as they can, each as much as the
/// ceiling allows. So the decoder holds as little as it can, and a stream
/// that never needs smoothing has every frame whole in its own substream.
///
/// It looks a fixed number of frames ahead. A frame whose excess the
/// substreams in that window cannot carry, or that would have the decoder
/// hold more than the buffer the stream states, is refused ([`Overrun`]):
/// it is never cut or coded coarser.
#[derive(Debug, Clone)]
pub struct Smoother {
    /// The most lossless bytes a substream may carry, less one: the byte
    /// that moves a boundary off a frame's start.
    room: u64,
    capacity: usize,
    lookahead: usize,
    /// Where each frame waiting ends in the lossless stream.
    ends: VecDeque<u64>,
    /// The frame at the front, and where it starts.
    front: u64,
    start: u64,
    /// Bytes of the lossless stream sent.
    sent: u64,
    /// The most bytes the decoder has held at once, and the substreams that
    /// carried other than exactly their own frame.
    peak: u64,
    ahead: u64,
}

impl Smoother {
    /// Substreams carrying `room` lossless bytes at most, for a decoder
    /// whose buffer holds `capacity` (0: no frame is sent ahead), looking
    /// `lookahead` frames ahead.
    ///
    /// # Panics
    /// If `room` is under 2 bytes.
    pub fn new(room: usize, capacity: usize, lookahead: usize) -> Self {
        assert!(
            room >= 2,
            "a substream carries a byte of lossless data at least"
        );
        let lookahead = if capacity == 0 { 0 } else { lookahead };
        Self {
            room: room as u64 - 1,
            capacity,
            lookahead,
            ends: VecDeque::with_capacity(lookahead + 1),
            front: 0,
            start: 0,
            sent: 0,
            peak: 0,
            ahead: 0,
        }
    }

    /// The next lossless frame, of `bytes`.
    pub fn push(&mut self, bytes: usize) {
        let end = self.ends.back().copied().unwrap_or(self.start) + bytes as u64;
        self.ends.push_back(end);
    }

    /// Whether the front frame's substream can be decided: the frames after
    /// it fill the window, or none can follow.
    pub fn ready(&self) -> bool {
        self.ends.len() > self.lookahead
    }

    /// Frames waiting for their substream.
    pub fn waiting(&self) -> usize {
        self.ends.len()
    }

    /// The front frame's substream: what it carries. `None` when no frame
    /// is waiting; on an overrun, the frame that cannot be carried.
    pub fn carry(&mut self) -> Option<Result<Carry, (u64, Overrun)>> {
        let own_end = *self.ends.front()?;
        // What must be sent by this substream for every frame waiting to be
        // whole by its own, the substreams after this carrying all they can.
        let latest = |i: usize, end: u64| end.saturating_sub(i as u64 * self.room);
        let need = self
            .ends
            .iter()
            .enumerate()
            .map(|(i, &end)| latest(i, end))
            .max()?;
        // A byte at least: the component's size is coded less one.
        let mut bytes = need.saturating_sub(self.sent).max(1);
        let capacity = self.capacity as u64;
        if bytes > self.room || (capacity > 0 && need - self.start > capacity) {
            // The first frame that cannot be carried: past the ceiling if
            // any is, else past the buffer.
            let frames = || {
                self.ends
                    .iter()
                    .enumerate()
                    .map(|(i, &end)| (i, latest(i, end)))
            };
            let refusal = frames()
                .find(|&(_, at)| at > self.sent + self.room)
                .map(|(i, at)| {
                    (
                        i,
                        Overrun::Ceiling {
                            excess: at - self.sent - self.room,
                            substreams: i as u64 + 1,
                        },
                    )
                })
                .or_else(|| {
                    frames()
                        .find(|&(_, at)| at.saturating_sub(self.start) > capacity)
                        .map(|(i, at)| {
                            (
                                i,
                                Overrun::Buffer {
                                    held: at - self.start,
                                    capacity: self.capacity,
                                },
                            )
                        })
                })
                .expect("a frame past the ceiling or the buffer");
            return Some(Err((self.front + refusal.0 as u64, refusal.1)));
        }
        // Data that starts exactly where a later frame starts would be
        // decoded in place by a decoder that synchronises there, a frame or
        // more early. One byte more puts the boundary inside that frame —
        // the byte the room keeps, and one the buffer has.
        let next = self.sent + bytes;
        let last = *self.ends.back()?;
        if next > own_end
            && next < last
            && (capacity == 0 || next + 1 - self.start <= capacity)
            && self.ends.binary_search(&next).is_ok()
        {
            bytes += 1;
        }
        debug_assert!(
            self.sent + bytes <= last,
            "a frame waiting is a byte at least"
        );
        // The first frame that starts in the data.
        let first = (self.start >= self.sent)
            .then_some((self.start, 0))
            .or_else(|| {
                self.ends
                    .iter()
                    .position(|&end| end >= self.sent)
                    .map(|i| (self.ends[i], i as u32 + 1))
            });
        let sync = first
            .filter(|&(at, _)| at < self.sent + bytes)
            .map(|(at, delay)| Sync {
                offset: (at - self.sent) as u32,
                delay,
            });
        self.peak = self.peak.max(self.sent + bytes - self.start);
        self.ahead += u64::from(self.sent != self.start || self.sent + bytes != own_end);
        let carry = Carry {
            frame: self.front,
            bytes: bytes as usize,
            sync,
        };
        self.sent += bytes;
        self.start = own_end;
        self.front += 1;
        self.ends.pop_front();
        Some(Ok(carry))
    }

    /// The most bytes the decoder has held at once, the frame it takes out
    /// included.
    pub fn peak(&self) -> u64 {
        self.peak
    }

    /// Substreams that carried other than exactly their own frame.
    pub fn ahead(&self) -> u64 {
        self.ahead
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hz_core::bits::BitWriter;

    /// A lossless frame of `size` bytes as far as the buffer is concerned:
    /// a common header stating the size, then zeroes, then its index.
    fn frame(index: u32, size: usize) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(32, xll::SYNC);
        w.put(4, 0);
        w.put(8, 15);
        w.put(5, 19);
        w.put(20, size as u32 - 1);
        w.put(4, 0);
        w.put(4, 1);
        w.put(4, 8);
        w.put(5, 15);
        w.put(2, 0);
        w.put(1, 0);
        w.put(5, 7);
        let mut bytes = w.padded();
        bytes.resize(size - 4, 0);
        bytes.extend(index.to_be_bytes());
        assert_eq!(
            xll::CommonHeader::read(&bytes).unwrap().frame_size as usize,
            size
        );
        bytes
    }

    /// Each substream's lossless data and sync fields.
    type Substreams = Vec<(Vec<u8>, Option<XllSync>)>;

    /// The substreams for frames of `sizes`, or the refusal.
    fn smooth(
        sizes: &[usize],
        room: usize,
        capacity: usize,
        lookahead: usize,
    ) -> Result<Substreams, (u64, Overrun)> {
        let stream: Vec<u8> = sizes
            .iter()
            .enumerate()
            .flat_map(|(i, &s)| frame(i as u32, s))
            .collect();
        let mut smoother = Smoother::new(room, capacity, lookahead);
        let mut at = 0;
        let mut out = Vec::new();
        let mut take = |carry: Carry, out: &mut Vec<_>| {
            let data = stream[at..at + carry.bytes].to_vec();
            at += carry.bytes;
            let sync = carry.sync.map(|s| XllSync {
                peak_buffer_code: 8,
                delay_bits: delay_bits(s.delay),
                delay_frames: s.delay,
                sync_offset: s.offset,
            });
            out.push((data, sync));
        };
        for &size in sizes {
            smoother.push(size);
            while smoother.ready() {
                take(smoother.carry().unwrap()?, &mut out);
            }
        }
        while let Some(carry) = smoother.carry() {
            take(carry?, &mut out);
        }
        assert_eq!(at, stream.len());
        Ok(out)
    }

    /// Every substream's data goes through the decoder's buffer, and the
    /// frame it takes out with substream `n` is frame `n`, whole; never
    /// more than `capacity` held; and a decoder that starts at any
    /// substream stating a sync gets frame `n + delay` after its delay.
    fn decode(substreams: &[(Vec<u8>, Option<XllSync>)], sizes: &[usize], capacity: usize) {
        let mut buffer = DecoderBuffer::new();
        for (n, (data, sync)) in substreams.iter().enumerate() {
            assert_eq!(
                buffer.push(data, sync.as_ref()),
                Ok(Taken::Frame(sizes[n])),
                "substream {n}"
            );
            assert_eq!(buffer.frame(), frame(n as u32, sizes[n]), "substream {n}");
        }
        if capacity > 0 {
            assert!(buffer.peak() <= capacity, "{} held", buffer.peak());
        }
        for (n, (data, sync)) in substreams.iter().enumerate() {
            let Some(s) = sync else { continue };
            let mut joining = DecoderBuffer::new();
            let mut taken = joining.push(data, Some(s));
            for (later, (data, sync)) in substreams.iter().enumerate().skip(n + 1) {
                if later > n + s.delay_frames as usize {
                    break;
                }
                taken = joining.push(data, sync.as_ref());
            }
            let due = n + s.delay_frames as usize;
            assert_eq!(taken, Ok(Taken::Frame(sizes[due])), "joined at {n}");
            assert_eq!(
                joining.frame(),
                frame(due as u32, sizes[due]),
                "joined at {n}"
            );
        }
    }

    #[test]
    fn frames_that_fit_go_whole_in_their_own_substream() {
        let sizes = [500, 999, 64, 999, 300];
        let substreams = smooth(&sizes, 1000, 1 << 14, 8).unwrap();
        for ((data, sync), &size) in substreams.iter().zip(&sizes) {
            assert_eq!(data.len(), size);
            assert_eq!(
                sync.as_ref().map(|s| (s.sync_offset, s.delay_frames)),
                Some((0, 0))
            );
        }
        decode(&substreams, &sizes, 1 << 14);
    }

    /// A frame too large for its substream is sent ahead as late as it can
    /// be: the substreams just before it carry it, the first in part and
    /// then each as much as it can, as the smoothing stream measured does.
    #[test]
    fn a_large_frame_is_sent_ahead_as_late_as_it_can_be() {
        let sizes = [100, 100, 100, 2500, 100];
        let substreams = smooth(&sizes, 1000, 1 << 14, 8).unwrap();
        let carried: Vec<usize> = substreams.iter().map(|(d, _)| d.len()).collect();
        // 999 a substream, one byte kept to move a boundary.
        assert_eq!(carried, [100, 702, 999, 999, 100]);
        let syncs: Vec<_> = substreams
            .iter()
            .map(|(_, s)| s.as_ref().map(|s| (s.sync_offset, s.delay_frames)))
            .collect();
        // The third and fourth start inside the large frame, and no frame
        // starts in them.
        assert_eq!(
            syncs,
            [Some((0, 0)), Some((0, 0)), None, None, Some((0, 0))]
        );
        decode(&substreams, &sizes, 1 << 14);
    }

    /// Sending ahead over several frames: the data of a substream starts
    /// inside a frame, and says where the next frame starts and how many
    /// substreams later the decoder takes it out.
    #[test]
    fn a_run_of_large_frames_states_where_a_decoder_can_join() {
        let sizes = [
            300, 700, 800, 900, 1400, 1500, 1300, 200, 900, 1100, 1200, 100, 100, 3000, 50, 60,
        ];
        let substreams = smooth(&sizes, 1000, 1 << 14, 16).unwrap();
        assert!(
            substreams
                .iter()
                .any(|(_, s)| s.as_ref().is_some_and(|s| s.delay_frames > 1))
        );
        assert!(substreams.iter().any(|(_, s)| s.is_none()));
        for (data, _) in &substreams {
            assert!(data.len() <= 1000);
        }
        decode(&substreams, &sizes, 1 << 14);
    }

    /// Data that would start exactly where a later frame starts is moved
    /// one byte into it: a decoder starting there would otherwise decode
    /// that frame in place, early.
    #[test]
    fn no_substream_starts_on_a_later_frame() {
        // The third frame needs the first substream to carry the first two
        // frames whole: 200 bytes, ending where the third starts.
        let sizes = [100, 100, 1998];
        let substreams = smooth(&sizes, 1000, 1 << 14, 8).unwrap();
        let carried: Vec<usize> = substreams.iter().map(|(d, _)| d.len()).collect();
        assert_eq!(carried, [201, 998, 999]);
        decode(&substreams, &sizes, 1 << 14);
    }

    #[test]
    fn a_frame_that_cannot_be_carried_is_refused_naming_the_first() {
        // Too large for its own substream with nothing before it.
        assert_eq!(
            smooth(&[1500, 100], 1000, 1 << 14, 8).unwrap_err(),
            (
                0,
                Overrun::Ceiling {
                    excess: 501,
                    substreams: 1
                }
            )
        );
        // More than the three substreams up to it carry; the fifth would be
        // too, but the fourth is named.
        assert_eq!(
            smooth(&[900, 900, 900, 1400, 5000], 1000, 1 << 14, 8).unwrap_err(),
            (
                3,
                Overrun::Ceiling {
                    excess: 104,
                    substreams: 4
                }
            )
        );
        // Without a buffer, nothing is sent ahead.
        assert_eq!(
            smooth(&[100, 1200], 1000, 0, 8).unwrap_err(),
            (
                1,
                Overrun::Ceiling {
                    excess: 201,
                    substreams: 1
                }
            )
        );
        // Carried in time, but over a 2 kB buffer.
        assert_eq!(
            smooth(&[100, 100, 100, 2900], 1000, 2000, 8).unwrap_err(),
            (
                3,
                Overrun::Buffer {
                    held: 2001,
                    capacity: 2000
                }
            )
        );
        decode(
            &smooth(&[100, 100, 100, 2900], 1000, 3000, 8).unwrap(),
            &[100, 100, 100, 2900],
            3000,
        );
    }

    /// A frame past the lookahead is not seen in time: the window is the
    /// domain.
    #[test]
    fn the_lookahead_bounds_how_early_a_frame_is_sent() {
        let sizes = [600, 600, 600, 600, 2000];
        assert!(smooth(&sizes, 1000, 1 << 14, 4).is_ok());
        assert_eq!(
            smooth(&sizes, 1000, 1 << 14, 1).unwrap_err().0,
            4,
            "two substreams cannot carry its excess"
        );
    }

    #[test]
    fn the_delay_width_is_the_measured_one() {
        let widths: Vec<u8> = (0..=17).map(delay_bits).collect();
        assert_eq!(
            widths,
            [1, 1, 2, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 5, 5, 5, 5, 6]
        );
    }

    #[test]
    fn the_budget_rounds_down() {
        assert_eq!(frame_budget(CEILING, 512, 48000), 32666);
        assert_eq!(buffer_bytes(8), 131_072);
        assert_eq!(buffer_bytes(15), MAX_BUFFER);
    }
}
