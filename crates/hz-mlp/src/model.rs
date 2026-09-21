//! What a decoder holds, as the writer left it.
//!
//! Almost every field in an access unit is optional, and what makes it
//! optional is that a decoder *keeps* what it was last given: a channel whose
//! filters, codebook and width are all what the decoder already holds costs
//! one bit instead of eleven. So an encoder that wants to say nothing has to
//! know exactly what the decoder is holding — and being wrong about that does
//! not fail. It produces a stream that parses, whose every checksum verifies,
//! and which decodes to different samples.
//!
//! That model used to be eight parallel vectors in the encoder, updated by
//! hand next to each decision. This is the same model in one place, updated by
//! the writer as it emits: a field that is written is a field the model
//! records, and there is no second copy to forget.
//!
//! # What resets it
//!
//! A restart header. It clears, for the channels the substream codes, both
//! filters, the coding, the block size, the matrices and the output shifts —
//! which is what makes a restart a point a decoder can join the stream at, and
//! would be pointless otherwise. [`DecoderModel::restart`] is that clearing,
//! and the writer calls it where the header is written.

use crate::filter::{Coding, Filter, Taps};
use crate::format::{DynamicRange, MAX_CHANNELS, MAX_SUBSTREAMS, Substream};

/// The width a restart header leaves a channel's residuals at: the whole
/// sample, written raw.
const RESET_LSBS: u32 = crate::frame::CODEC_BITS;

/// What a decoder holds for one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held {
    /// Both filters, without the history a second one may arrive with: a
    /// history is installed once and is not part of what is kept.
    pub fir: Taps,
    pub iir: Taps,
    /// The accumulator shift, which both filters share and only the first
    /// states.
    pub shift: u32,
    pub codebook: u8,
    pub huff_lsbs: u32,
    /// The dead low bits the decoder puts back at the very end.
    pub output_shift: u8,
}

impl Default for Held {
    fn default() -> Self {
        Self {
            fir: Taps::default(),
            iir: Taps::default(),
            shift: 0,
            codebook: crate::huffman::NONE,
            huff_lsbs: RESET_LSBS,
            output_shift: 0,
        }
    }
}

/// And what it holds for one substream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubstreamHeld {
    /// Samples in a block, which a block states only when it differs.
    blocksize: usize,
    /// The presence flags, which say which of the optional fields a block may
    /// carry at all.
    presence: u8,
    /// The gain the substream directory last stated, which is not a decoding
    /// parameter but is held the same way.
    range: Option<DynamicRange>,
}

impl Default for SubstreamHeld {
    fn default() -> Self {
        Self {
            // A restart header sets it to eight, which is also the longest
            // filter the format has.
            blocksize: crate::encoder::RESTART_BLOCK,
            presence: crate::frame::PARAMS_DEFAULT,
            range: None,
        }
    }
}

/// The decoder's state, as the stream written so far leaves it.
#[derive(Debug, Clone)]
pub(crate) struct DecoderModel {
    channels: [Held; MAX_CHANNELS],
    substreams: [SubstreamHeld; MAX_SUBSTREAMS],
}

impl Default for DecoderModel {
    fn default() -> Self {
        Self {
            channels: [Held::default(); MAX_CHANNELS],
            substreams: [SubstreamHeld::default(); MAX_SUBSTREAMS],
        }
    }
}

impl Held {
    /// The same four questions the model answers for a channel, asked of one
    /// channel's state alone — so a per-channel walk can ask them without the
    /// model, and get the same answers.
    pub(crate) fn restates_fir(&self, filter: &Filter) -> bool {
        !filter.fir.same_taps(&self.fir) || filter.shift != self.shift
    }

    pub(crate) fn restates_iir(&self, filter: &Filter) -> bool {
        !filter.iir.same_taps(&self.iir) || filter.iir.state.is_some() || filter.shift != self.shift
    }

    pub(crate) fn restates(&self, coding: &Coding) -> bool {
        self.restates_fir(&coding.filter)
            || self.restates_iir(&coding.filter)
            || coding.codebook != self.codebook
            || coding.huff_lsbs != self.huff_lsbs
    }

    pub(crate) fn state(&mut self, coding: &Coding) {
        self.fir = coding.filter.fir;
        self.iir = Taps {
            state: None,
            ..coding.filter.iir
        };
        self.shift = coding.filter.shift;
        self.codebook = coding.codebook;
        self.huff_lsbs = coding.huff_lsbs;
    }

    pub(crate) fn in_force(&self) -> Filter {
        Filter {
            fir: self.fir,
            iir: self.iir,
            shift: self.shift,
        }
    }
}

impl DecoderModel {
    /// What a restart header leaves behind.
    ///
    /// Per substream, because that is what a restart header is: substream 1
    /// restarting says nothing about the channels substream 0 codes. The
    /// output shifts go with the matrix span rather than the coded range,
    /// since that is the span the field covers.
    pub(crate) fn restart(&mut self, index: usize, range: &Substream) {
        // Not the dynamic range gain: that rides in the substream directory
        // rather than in the decoding parameters, and a restart header says
        // nothing about it. The schedule is refreshed on its own deadline.
        self.substreams[index] = SubstreamHeld {
            range: self.substreams[index].range,
            ..SubstreamHeld::default()
        };
        for channel in range.first..=range.last {
            self.channels[channel] = Held::default();
        }
        for channel in 0..=range.max_matrix {
            self.channels[channel].output_shift = 0;
        }
    }

    /// One channel's held state, on its own.
    ///
    /// For [`crate::encoder::Encoder::decide_interval`], which walks a channel
    /// through a whole interval on its own thread: what it needs of the model
    /// is this, and the queries below on it.
    pub(crate) fn channel(&self, channel: usize) -> Held {
        self.channels[channel]
    }

    /// Whether the channel has to say anything at all.
    pub(crate) fn restates(&self, channel: usize, coding: &Coding) -> bool {
        self.channels[channel].restates(coding)
    }

    /// Record what a block states about one channel.
    pub(crate) fn state(&mut self, channel: usize, coding: &Coding) {
        self.channels[channel].state(coding);
    }

    /// Whether the output shifts have to be restated, over the span the field
    /// covers.
    pub(crate) fn restates_shifts(&self, shifts: &[u8], span: usize) -> bool {
        self.channels[..=span]
            .iter()
            .zip(shifts)
            .any(|(held, shift)| held.output_shift != *shift)
    }

    pub(crate) fn state_shifts(&mut self, shifts: &[u8], span: usize) {
        for (held, shift) in self.channels[..=span].iter_mut().zip(shifts) {
            held.output_shift = *shift;
        }
    }

    /// Samples a block holds unless it says otherwise.
    pub(crate) fn blocksize(&self, index: usize) -> usize {
        self.substreams[index].blocksize
    }

    pub(crate) fn state_blocksize(&mut self, index: usize, frames: usize) {
        self.substreams[index].blocksize = frames;
    }

    /// The dynamic range gain the substream directory last stated.
    pub(crate) fn range(&self, index: usize) -> Option<DynamicRange> {
        self.substreams[index].range
    }

    pub(crate) fn state_range(&mut self, index: usize, range: DynamicRange) {
        self.substreams[index].range = Some(range);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::RestartSync;

    fn taps(order: usize, first: i32) -> Taps {
        let mut coefficients = [0i32; crate::lpc::MAX_ORDER];
        coefficients[0] = first;
        Taps {
            order,
            coefficients,
            coeff_bits: 15,
            coeff_shift: 0,
            state: None,
        }
    }

    fn coding(order: usize, first: i32, codebook: u8, lsbs: u32) -> Coding {
        Coding {
            filter: Filter {
                fir: taps(order, first),
                iir: Taps::default(),
                shift: 12,
            },
            codebook,
            huff_lsbs: lsbs,
        }
    }

    /// A model that has just been reset holds what the format says a restart
    /// header leaves: no filters, no codebook, and the whole sample raw.
    #[test]
    fn a_restart_leaves_the_format_defaults() {
        let mut model = DecoderModel::default();
        model.state(0, &coding(2, 4000, 3, 9));
        model.state_shifts(&[4, 4], 1);
        model.state_blocksize(0, 40);

        let range = Substream {
            first: 0,
            last: 1,
            max_matrix: 1,
            sync: RestartSync::A,
        };
        model.restart(0, &range);

        assert_eq!(model.blocksize(0), crate::encoder::RESTART_BLOCK);
        assert!(!model.restates_shifts(&[0, 0], 1), "shifts are cleared");
        let plain = Coding {
            filter: Filter::default(),
            codebook: crate::huffman::NONE,
            huff_lsbs: RESET_LSBS,
        };
        assert!(
            !model.restates(0, &plain),
            "after a restart, the plain coding is what the decoder already holds"
        );
    }

    /// A restart in one substream says nothing about the channels another
    /// codes, which is what makes each presentation independently joinable.
    #[test]
    fn a_restart_reaches_only_its_own_channels() {
        let mut model = DecoderModel::default();
        let loud = coding(3, 5000, 2, 11);
        model.state(0, &loud);
        model.state(4, &loud);

        model.restart(
            1,
            &Substream {
                first: 2,
                last: 5,
                max_matrix: 5,
                sync: RestartSync::B,
            },
        );
        assert!(!model.restates(0, &loud), "channel 0 was not restarted");
        assert!(model.restates(4, &loud), "channel 4 was");
    }

    /// The shift is the first filter's, so a pair whose shift moved has to
    /// restate both filters even when neither one's taps changed.
    #[test]
    fn a_moved_shift_restates_both_filters() {
        let mut model = DecoderModel::default();
        let mut first = coding(2, 4000, 1, 8);
        first.filter.iir = taps(1, 900);
        model.state(0, &first);
        assert!(!model.restates(0, &first));

        let mut moved = first;
        moved.filter.shift = 13;
        assert!(model.channel(0).restates_fir(&moved.filter));
        assert!(model.channel(0).restates_iir(&moved.filter));
    }

    /// A second filter arriving with a history has to be restated, and what is
    /// remembered has no history — so the block after it says nothing.
    #[test]
    fn a_history_is_installed_once() {
        let mut model = DecoderModel::default();
        let mut with_history = coding(2, 4000, 1, 8);
        with_history.filter.iir = Taps {
            state: Some(crate::filter::FilterState {
                values: [1, 2, 3, 4, 0, 0, 0, 0],
                bits: 4,
                shift: 0,
            }),
            ..taps(2, 900)
        };
        assert!(model.channel(0).restates_iir(&with_history.filter));
        model.state(0, &with_history);

        let mut again = with_history;
        again.filter.iir.state = None;
        assert!(
            !model.channel(0).restates_iir(&again.filter),
            "the same taps without a history are what the decoder holds"
        );
    }
}
