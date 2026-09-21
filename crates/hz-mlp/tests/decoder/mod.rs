// SPDX-License-Identifier: GPL-3.0-or-later
//
// Follows the shape of a decoder in FFmpeg, which is licensed under the GNU
// Lesser General Public License, version 2.1 or later, and is used here under
// the GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/mlpdec.c — Copyright (c) 2007-2008 Ian Caulfield
// What was taken and what was changed is recorded in docs/provenance.md.

//! A reference decoder, so `decode(encode(x)) == x` is a `cargo test`.
//!
//! The decisive test for a lossless encoder is that somebody else's decoder
//! gets the samples back, and that lives in `cargo xtask mlp` because it needs
//! `harletty` and FFmpeg installed. This is the same question asked without
//! one: a decoder in the test tree, reading the bytes the encoder produced and
//! reconstructing the samples from them.
//!
//! # It is written from the decoder's side, deliberately
//!
//! Not from [`hz_mlp`]'s writer. A decoder that is the writer read backwards
//! agrees with it about every mistake, and the mistakes this format punishes
//! are exactly the ones that parse cleanly: a matrix declared in one substream
//! and not another, a filter said to be unchanged when it is not, a ramp
//! folded down in the wrong order. So the arithmetic here is written out from
//! the description of what a decoder does — accumulate, shift, add, push —
//! and where a seed existed it came from the `reconstruct` and `apply_forward`
//! helpers that used to live in the `filter` and `matrix` tests, which were
//! written the same way and for the same reason.
//!
//! What *is* reused is [`hz_mlp::reader`], for everything down to the block
//! data: the access unit header, the major sync, the substream directory, the
//! restart headers and the four checksums. That code exists to be checked
//! against streams this project did not write, and it is a reader rather than
//! a writer, so it is not the encoder agreeing with itself.
//!
//! # Stopping early is the point
//!
//! A TrueHD stream is several presentations sharing one buffer of channels,
//! and a decoder may stop after any of them. [`decode`] takes which one to
//! stop at, applies **that substream's** matrices and no others, and returns
//! its channels — which is what catches a matrix that one substream declares
//! and another does not, the one class of mistake that leaves every checksum
//! in the unit intact.

/// The codec's domain, which the reference decoder enforces on every stored
/// sample it reconstructs — under restart sync words A and B; under C the
/// filters run in thirty-two bits and the bound is the whole word.
const DOMAIN: i64 = 1 << 23;

use hz_core::bits_read::BitReader;
use hz_mlp::format::RestartSync;
use hz_mlp::{huffman, reader};

/// The most channels a stream carries, and so the width of the sample buffer
/// every substream shares.
const MAX_CHANNELS: usize = 16;
/// The longest first filter the format has, and so how much signal history a
/// decoder keeps.
const MAX_FIR_ORDER: usize = 8;
/// And the longest second filter, over past residuals.
const MAX_IIR_ORDER: usize = 4;
/// Sources a matrix may read: the matrix channels, plus the two noise
/// channels restart sync word A tells a decoder to synthesise.
const MAX_SOURCES: usize = MAX_CHANNELS + 2;
/// The most matrices one access unit may carry.
///
/// The count is a four-bit field, and the two matrix syntaxes read it
/// differently: the first writes it as itself, so it stops at fifteen, and the
/// immersive one writes it as one less than itself — which is why that syntax
/// cannot say "no matrices" that way and can say sixteen, one per channel of
/// the widest programme there is.
const MAX_MATRICES: usize = 15;
const MAX_MATRICES_IMMERSIVE: usize = 16;
/// The most samples an access unit holds, at four times the base rate.
const MAX_BLOCK: usize = 160;

/// Fractional bits the format counts matrix coefficients in.
const FRACTION: u32 = 14;
/// Where the matrix accumulator has its point, which is not where the
/// coefficients have theirs.
const ACCUMULATOR: u32 = 18;
const SCALE: u32 = ACCUMULATOR - FRACTION;

/// The marker a shortened final access unit carries.
const END_OF_STREAM: u32 = 0xd234;

/// Which parameters a block may restate. A restart header sets them all.
const PARAM_PRESENCE: u8 = 1 << 0;
const PARAM_HUFFOFFSET: u8 = 1 << 1;
const PARAM_IIR: u8 = 1 << 2;
const PARAM_FIR: u8 = 1 << 3;
const PARAM_QUANTSTEP: u8 = 1 << 4;
const PARAM_OUTSHIFT: u8 = 1 << 5;
const PARAM_MATRIX: u8 = 1 << 6;
const PARAM_BLOCKSIZE: u8 = 1 << 7;

/// What a decoder hands back.
pub struct Decoded {
    /// Interleaved samples in the stream's own channel order, which is not a
    /// WAV file's past 5.1 — see `Encoder::channel_order`.
    pub samples: Vec<i32>,
    /// How many channels the presentation stopped at carries.
    pub channels: usize,
}

impl Decoded {
    /// One frame, as a slice of channels.
    pub fn frame(&self, index: usize) -> &[i32] {
        &self.samples[index * self.channels..(index + 1) * self.channels]
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }
}

type Result<T> = std::result::Result<T, String>;

/// Decode a whole stream, stopping after substream `presentation`.
///
/// `presentation` is the substream index: 0 is the two-channel presentation,
/// the last is the widest. A decoder that stops there applies that
/// substream's matrices and hands back its channels, which is what a
/// two-channel decoder does with an eight-channel stream.
pub fn decode(stream: &[u8], presentation: usize) -> Result<Decoded> {
    let mut decoder = Decoder::new(presentation);
    let mut at = 0usize;
    while at < stream.len() {
        let consumed = decoder.access_unit(&stream[at..])?;
        at += consumed;
    }
    decoder.finish()
}

/// One prediction filter, as the decoder holds it between blocks.
#[derive(Clone, Copy, Default)]
struct Taps {
    order: usize,
    coefficients: [i32; MAX_FIR_ORDER],
    /// How far right the accumulator is shifted. Both filters carry one and a
    /// decoder uses the first filter's, which is why it is kept per filter and
    /// read from only one of them.
    shift: u32,
}

/// What a decoder holds for one channel.
#[derive(Clone, Copy)]
struct Channel {
    fir: Taps,
    iir: Taps,
    codebook: u8,
    huff_lsbs: u32,
    huff_offset: i32,
    /// Reconstructed samples, most recent first.
    signal: [i32; MAX_FIR_ORDER],
    /// And the residuals that produced them.
    residual: [i32; MAX_IIR_ORDER],
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            fir: Taps::default(),
            iir: Taps::default(),
            codebook: 0,
            // What a restart header leaves behind, which is the whole sample
            // written raw.
            huff_lsbs: 24,
            huff_offset: 0,
            signal: [0; MAX_FIR_ORDER],
            residual: [0; MAX_IIR_ORDER],
        }
    }
}

/// One primitive matrix, as the stream states it.
#[derive(Clone, Copy)]
struct Matrix {
    dest: usize,
    /// In [`FRACTION`] fractional bits, indexed by source channel.
    coefficients: [i32; MAX_SOURCES],
    /// How far each coefficient moves per access unit, in the same bits.
    delta: [i32; MAX_SOURCES],
}

impl Default for Matrix {
    fn default() -> Self {
        Self {
            dest: 0,
            coefficients: [0; MAX_SOURCES],
            delta: [0; MAX_SOURCES],
        }
    }
}

/// One substream's decoding state, which a restart header resets.
struct Substream {
    restart_seen: bool,
    sync: RestartSync,
    min_channel: usize,
    max_channel: usize,
    max_matrix_channel: usize,
    /// Matrix channel per output channel.
    ch_assign: [usize; MAX_CHANNELS],
    param_presence: u8,
    blocksize: usize,
    matrices: Vec<Matrix>,
    /// Access units since the matrices were stated, which is how far along
    /// its ramp each coefficient has walked.
    matrix_age: usize,
    output_shift: [i32; MAX_CHANNELS],
    quant_step: [u32; MAX_CHANNELS],
    max_shift: u32,
    max_huff_lsbs: u32,
    /// The check accumulated since the last restart header, which the next one
    /// certifies.
    lossless_check: u32,
    /// Samples decoded into the buffer for this access unit.
    blockpos: usize,
    /// How many of them the unit says are padding.
    shorten_by: usize,
}

impl Default for Substream {
    fn default() -> Self {
        Self {
            restart_seen: false,
            sync: RestartSync::A,
            min_channel: 0,
            max_channel: 0,
            max_matrix_channel: 0,
            ch_assign: [0; MAX_CHANNELS],
            param_presence: 0xff,
            blocksize: 8,
            matrices: Vec::new(),
            matrix_age: 0,
            output_shift: [0; MAX_CHANNELS],
            quant_step: [0; MAX_CHANNELS],
            max_shift: 0,
            max_huff_lsbs: 24,
            lossless_check: 0,
            blockpos: 0,
            shorten_by: 0,
        }
    }
}

struct Decoder {
    /// Which substream to stop after.
    presentation: usize,
    substreams: Vec<Substream>,
    /// Channel state is per channel and not per substream: substreams code
    /// disjoint ranges of one shared buffer, and a restart in one of them
    /// says nothing about the channels another codes.
    channels: [Channel; MAX_CHANNELS],
    /// The access unit being decoded, frame-major, as a decoder holds it: the
    /// filters write into it and the matrices read every channel of it.
    buffer: Vec<[i32; MAX_CHANNELS]>,
    out: Vec<i32>,
    out_channels: usize,
    /// What the last major sync said, since a unit without one does not say.
    declared_substreams: usize,
    evolution: bool,
    /// How many lossless checks were verified, so a test can insist the
    /// stream actually carried some.
    checks_verified: u64,
}

impl Decoder {
    fn new(presentation: usize) -> Self {
        Self {
            presentation,
            substreams: Vec::new(),
            channels: [Channel::default(); MAX_CHANNELS],
            buffer: vec![[0; MAX_CHANNELS]; MAX_BLOCK],
            out: Vec::new(),
            out_channels: 0,
            declared_substreams: 0,
            evolution: false,
            checks_verified: 0,
        }
    }

    fn finish(self) -> Result<Decoded> {
        if self.out_channels == 0 {
            return Err("the stream decoded to no channels at all".into());
        }
        Ok(Decoded {
            samples: self.out,
            channels: self.out_channels,
        })
    }

    /// Decode one access unit, and return the bytes it occupied.
    fn access_unit(&mut self, stream: &[u8]) -> Result<usize> {
        let unit = reader::access_unit(stream, self.declared_substreams, self.evolution)
            .map_err(|e| format!("the unit did not parse: {e}"))?;
        if !unit.parity_nibble_ok {
            return Err("the access unit header's parity nibble is wrong".into());
        }
        if let Some(sync) = &unit.major_sync {
            if !sync.checksum_ok {
                return Err("the major sync's checksum is wrong".into());
            }
            self.declared_substreams = sync.substreams;
            self.evolution = sync.flags & (1 << 12) != 0;
        }
        let count = unit.directory.len();
        if count == 0 {
            return Err("an access unit with no substreams".into());
        }
        if self.presentation >= count {
            return Err(format!(
                "the stream carries {count} substreams, so there is no presentation {}",
                self.presentation
            ));
        }
        while self.substreams.len() < count {
            self.substreams.push(Substream::default());
        }

        // Where the payload starts: past the header, the major sync if there
        // is one, and the directory — whose entries are four bytes rather
        // than two when they carry a dynamic range word.
        let directory_at = 4 + unit.major_sync.as_ref().map_or(0, |sync| sync.size);
        let directory_bytes: usize = unit
            .directory
            .iter()
            .map(|entry| if entry.extra_word { 4 } else { 2 })
            .sum();
        let mut start = directory_at + directory_bytes;

        for index in 0..count {
            let entry = &unit.directory[index];
            let end = directory_at + directory_bytes + usize::from(entry.end_words) * 2;
            let bytes = stream
                .get(start..end)
                .ok_or_else(|| format!("substream {index} runs past the access unit"))?;
            let parsed = &unit.substreams[index];
            if !parsed.parity_ok || !parsed.checksum_ok {
                return Err(format!("substream {index} does not check out"));
            }
            // A decoder only reads what it needs: everything up to the
            // presentation it stops at.
            if index <= self.presentation {
                self.substream(index, bytes, parsed, entry.checkdata_present)
                    .map_err(|e| format!("substream {index}: {e}"))?;
            }
            start = end;
        }

        self.emit()?;
        for substream in &mut self.substreams[..count] {
            substream.matrix_age += 1;
        }
        Ok(unit.bytes)
    }

    /// One substream: its blocks, and the samples they carry.
    fn substream(
        &mut self,
        index: usize,
        bytes: &[u8],
        parsed: &reader::Substream,
        checkdata: bool,
    ) -> Result<()> {
        let mut bits = BitReader::new(bytes);
        // The parity and checksum are the last two bytes and are not payload.
        let payload_bits = bytes.len() * 8 - if checkdata { 16 } else { 0 };
        self.substreams[index].blockpos = 0;
        self.substreams[index].shorten_by = 0;

        loop {
            if bits.get_bit().map_err(text)? {
                if bits.get_bit().map_err(text)? {
                    let header = parsed
                        .restart
                        .as_ref()
                        .ok_or("a restart header the reader did not see")?;
                    self.restart(index, header)?;
                    bits.skip(restart_header_bits(header.channel_assignment.len()))
                        .map_err(text)?;
                }
                if !self.substreams[index].restart_seen {
                    return Err("a block before any restart header".into());
                }
                self.decoding_params(index, &mut bits)?;
            }
            if !self.substreams[index].restart_seen {
                return Err("a block before any restart header".into());
            }
            self.block_data(index, &mut bits)?;
            if bits.bit_position() > payload_bits {
                return Err("a block overran its substream".into());
            }
            if bits.get_bit().map_err(text)? {
                break;
            }
        }

        // The substream is measured in 16-bit words, and a shortened final
        // unit says so in two of them after the last block. Anything between
        // the last block and that boundary is padding and has to be zero.
        let over = bits.bit_position() % 16;
        if over != 0 {
            let padding = bits.get(16 - over as u32).map_err(text)?;
            if padding != 0 {
                return Err("the padding after the last block is not zero".into());
            }
        }
        if payload_bits.saturating_sub(bits.bit_position()) >= 32 {
            let marker = bits.get(16).map_err(text)?;
            if marker != END_OF_STREAM {
                return Err(format!("{marker:#x} is not the end-of-stream marker"));
            }
            let shorten = bits.get(16).map_err(text)?;
            // The high bits mark it as a TrueHD shortening rather than a plain
            // end of stream; the low thirteen are the count.
            if shorten & 0x2000 != 0 {
                let by = (shorten & 0x1fff) as usize;
                self.substreams[index].shorten_by = by.min(self.substreams[index].blockpos);
            }
        }
        Ok(())
    }

    /// A restart header: where a decoder may join, and so what it throws away.
    fn restart(&mut self, index: usize, header: &reader::RestartHeader) -> Result<()> {
        if !header.checksum_ok {
            return Err("the restart header's checksum is wrong".into());
        }
        if header.max_channel >= MAX_CHANNELS || header.max_matrix_channel >= MAX_CHANNELS {
            return Err("a restart header names a channel past the format's sixteen".into());
        }
        if header.min_channel > header.max_channel || header.max_channel > header.max_matrix_channel
        {
            return Err("a restart header's channel range does not make sense".into());
        }

        // The check the *previous* interval accumulated is what this header
        // certifies, and only the presentation being decoded has a complete
        // one — the substreams past it were never read.
        let previous = std::mem::take(&mut self.substreams[index].lossless_check);
        if index == self.presentation
            && self.substreams[index].restart_seen
            && xor_to_byte(previous) != header.lossless_check as u8
        {
            return Err(format!(
                "the lossless check is {:#04x}, the header says {:#04x}",
                xor_to_byte(previous),
                header.lossless_check
            ));
        }
        if self.substreams[index].restart_seen {
            self.checks_verified += 1;
        }

        let substream = &mut self.substreams[index];
        substream.restart_seen = true;
        substream.sync = header.sync;
        substream.min_channel = header.min_channel;
        substream.max_channel = header.max_channel;
        substream.max_matrix_channel = header.max_matrix_channel;
        for (slot, assignment) in substream
            .ch_assign
            .iter_mut()
            .zip(&header.channel_assignment)
        {
            *slot = *assignment as usize;
            if *slot > header.max_matrix_channel {
                return Err("a channel assignment points past the matrix span".into());
            }
        }
        substream.param_presence = 0xff;
        substream.blocksize = 8;
        substream.matrices.clear();
        substream.matrix_age = 0;
        substream.output_shift = [0; MAX_CHANNELS];
        substream.quant_step = [0; MAX_CHANNELS];
        substream.max_shift = header.max_shift;
        substream.max_huff_lsbs = header.max_huff_lsbs;

        // A restart is a point the stream can be joined at, so nothing after
        // it may depend on what came before: the channels this substream
        // codes lose their filters, their coding and their history.
        for channel in header.min_channel..=header.max_channel {
            self.channels[channel] = Channel::default();
        }
        Ok(())
    }

    /// What this block changes about how the substream is decoded.
    fn decoding_params(&mut self, index: usize, bits: &mut BitReader<'_>) -> Result<()> {
        if self.substreams[index].param_presence & PARAM_PRESENCE != 0
            && bits.get_bit().map_err(text)?
        {
            self.substreams[index].param_presence = bits.get(8).map_err(text)? as u8;
        }
        let flags = self.substreams[index].param_presence;

        if flags & PARAM_BLOCKSIZE != 0 && bits.get_bit().map_err(text)? {
            let blocksize = bits.get(9).map_err(text)? as usize;
            if blocksize == 0 || blocksize > MAX_BLOCK {
                return Err(format!("a block of {blocksize} samples"));
            }
            self.substreams[index].blocksize = blocksize;
        }
        if flags & PARAM_MATRIX != 0 && bits.get_bit().map_err(text)? {
            self.matrix_params(index, bits)?;
        }
        if flags & PARAM_OUTSHIFT != 0 && bits.get_bit().map_err(text)? {
            let span = self.substreams[index].max_matrix_channel;
            for channel in 0..=span {
                let shift = bits.get_signed(4).map_err(text)?;
                if shift < 0 {
                    return Err("a negative output shift throws away bits".into());
                }
                if shift as u32 > self.substreams[index].max_shift {
                    return Err(format!(
                        "an output shift of {shift} over the {} the restart header declared",
                        self.substreams[index].max_shift
                    ));
                }
                self.substreams[index].output_shift[channel] = shift;
            }
        }
        if flags & PARAM_QUANTSTEP != 0 && bits.get_bit().map_err(text)? {
            let span = self.substreams[index].max_channel;
            for channel in 0..=span {
                self.substreams[index].quant_step[channel] = bits.get(4).map_err(text)?;
            }
        }

        // One bit per channel *this substream codes*, whatever the flags say.
        let (first, last) = {
            let substream = &self.substreams[index];
            (substream.min_channel, substream.max_channel)
        };
        for channel in first..=last {
            if bits.get_bit().map_err(text)? {
                self.channel_params(index, channel, bits)?;
            }
        }
        Ok(())
    }

    /// One channel's filters and coding.
    fn channel_params(
        &mut self,
        index: usize,
        channel: usize,
        bits: &mut BitReader<'_>,
    ) -> Result<()> {
        let flags = self.substreams[index].param_presence;
        if flags & PARAM_FIR != 0 && bits.get_bit().map_err(text)? {
            let taps = filter_params(bits, MAX_FIR_ORDER, false)?;
            apply_taps(&mut self.channels[channel].fir, taps);
        }
        if flags & PARAM_IIR != 0 && bits.get_bit().map_err(text)? {
            let taps = filter_params(bits, MAX_IIR_ORDER, true)?;
            if let Some(state) = taps.state {
                self.channels[channel].residual = state;
            }
            apply_taps(&mut self.channels[channel].iir, taps);
        }
        if flags & PARAM_HUFFOFFSET != 0 && bits.get_bit().map_err(text)? {
            self.channels[channel].huff_offset = bits.get_signed(15).map_err(text)?;
        }
        let codebook = bits.get(2).map_err(text)? as u8;
        let huff_lsbs = bits.get(5).map_err(text)?;
        if huff_lsbs > 24 {
            return Err(format!("{huff_lsbs} low bits is wider than a sample"));
        }
        if huff_lsbs > self.substreams[index].max_huff_lsbs {
            return Err(format!(
                "{huff_lsbs} low bits over the {} the restart header declared",
                self.substreams[index].max_huff_lsbs
            ));
        }
        self.channels[channel].codebook = codebook;
        self.channels[channel].huff_lsbs = huff_lsbs;
        Ok(())
    }

    /// The primitive matrices, in whichever of the three syntaxes this
    /// substream's restart sync word speaks.
    fn matrix_params(&mut self, index: usize, bits: &mut BitReader<'_>) -> Result<()> {
        let substream = &mut self.substreams[index];
        substream.matrices.clear();
        substream.matrix_age = 0;
        let span = substream.max_matrix_channel;
        let sources = match substream.sync {
            // Plus the two noise channels a decoder synthesises.
            RestartSync::A => span + 3,
            RestartSync::B | RestartSync::C => span + 1,
        };

        if substream.sync == RestartSync::C {
            return immersive_matrix_params(substream, bits, sources);
        }

        let count = bits.get(4).map_err(text)? as usize;
        if count > MAX_MATRICES {
            return Err(format!("{count} matrices is more than the format has"));
        }
        for _ in 0..count {
            let dest = bits.get(4).map_err(text)? as usize;
            let frac_bits = bits.get(4).map_err(text)?;
            if dest > span {
                return Err("a matrix writes a channel past the matrix span".into());
            }
            if frac_bits > FRACTION {
                return Err(format!("{frac_bits} fractional bits"));
            }
            if bits.get_bit().map_err(text)? {
                return Err("a matrix with bypassed low bits is not decoded here".into());
            }
            let mut matrix = Matrix {
                dest,
                ..Matrix::default()
            };
            for source in 0..sources {
                if bits.get_bit().map_err(text)? {
                    let stored = bits.get_signed(frac_bits + 2).map_err(text)?;
                    matrix.coefficients[source] = stored * (1 << (FRACTION - frac_bits));
                }
            }
            if substream.sync == RestartSync::B {
                let dither = bits.get(4).map_err(text)?;
                if dither != 0 {
                    return Err("a matrix that adds dither is not decoded here".into());
                }
            }
            check_noise_sources(&matrix, span, sources)?;
            substream.matrices.push(matrix);
        }
        Ok(())
    }

    /// The samples of one block: residuals, then the filters that turn them
    /// back into samples.
    fn block_data(&mut self, index: usize, bits: &mut BitReader<'_>) -> Result<()> {
        let (first, last, blocksize, at) = {
            let substream = &self.substreams[index];
            (
                substream.min_channel,
                substream.max_channel,
                substream.blocksize,
                substream.blockpos,
            )
        };
        if at + blocksize > MAX_BLOCK {
            return Err("the blocks of this unit hold more than an access unit can".into());
        }

        // Residuals are interleaved: one sample of every channel, then the
        // next sample of every channel.
        for frame in at..at + blocksize {
            for channel in first..=last {
                let state = &self.channels[channel];
                let quant = self.substreams[index].quant_step[channel];
                let lsb_bits = state
                    .huff_lsbs
                    .checked_sub(quant)
                    .ok_or("a quantisation step wider than the residual")?;
                let mut value = 0i64;
                if state.codebook > 0 {
                    value = symbol(bits, state.codebook)? as i64;
                }
                if lsb_bits > 0 {
                    value = (value << lsb_bits) + i64::from(bits.get(lsb_bits).map_err(text)?);
                }
                value += huffman::sign_offset(state.codebook, lsb_bits, state.huff_offset);
                self.buffer[frame][channel] = (value * (1i64 << quant)) as i32;
            }
        }

        // Then the prediction, channel by channel, each carrying its history
        // from whatever block came before.
        for channel in first..=last {
            let quant = self.substreams[index].quant_step[channel];
            let mask = !((1i32 << quant) - 1);
            let state = &mut self.channels[channel];
            for frame in at..at + blocksize {
                let mut accumulator = 0i64;
                for (coefficient, past) in state.fir.coefficients[..state.fir.order]
                    .iter()
                    .zip(&state.signal)
                {
                    accumulator += i64::from(*coefficient) * i64::from(*past);
                }
                for (coefficient, past) in state.iir.coefficients[..state.iir.order]
                    .iter()
                    .zip(&state.residual)
                {
                    accumulator += i64::from(*coefficient) * i64::from(*past);
                }
                // Both filters carry a shift and a decoder uses the first
                // one's, which is why the second filter's precision is not its
                // own to choose.
                accumulator >>= state.fir.shift;

                // The reference decoder's own bound, and the one thing a
                // stream whose every checksum verifies can still be refused
                // for: a stored sample is reconstructed in twenty-four bits,
                // and one past them is a saturated recorrelator.
                let reconstructed =
                    (accumulator & i64::from(mask)) + i64::from(self.buffer[frame][channel]);
                let domain = if matches!(self.substreams[index].sync, RestartSync::C) {
                    1i64 << 31
                } else {
                    DOMAIN
                };
                if !(-domain..domain).contains(&reconstructed) {
                    return Err(format!(
                        "channel {channel} left the codec's domain at {reconstructed}: a \
                         saturated recorrelator, which the reference refuses"
                    ));
                }
                let sample = reconstructed as i32;
                // The sample goes on one history and the prediction error on
                // the other, which is the one thing about the second filter
                // that is easy to get backwards.
                let error = sample.wrapping_sub(accumulator as i32);
                state.signal.copy_within(0..MAX_FIR_ORDER - 1, 1);
                state.signal[0] = sample;
                state.residual.copy_within(0..MAX_IIR_ORDER - 1, 1);
                state.residual[0] = error;
                self.buffer[frame][channel] = sample;
            }
        }

        self.substreams[index].blockpos = at + blocksize;
        Ok(())
    }

    /// Apply the presentation's matrices and hand back its channels.
    fn emit(&mut self) -> Result<()> {
        let substream = &mut self.substreams[self.presentation];
        let frames = substream.blockpos;
        let span = substream.max_matrix_channel;

        // The ramp runs over the whole access unit, padding included, because
        // that is the length the coefficients were placed against — so the
        // matrices are applied before a short final unit is shortened.
        for frame in 0..frames {
            for matrix in &substream.matrices {
                let mut accumulator = 0i64;
                let mut drift = 0i64;
                for source in 0..=span {
                    let value = i64::from(self.buffer[frame][source]);
                    let delta = i64::from(matrix.delta[source]);
                    let carried = i64::from(matrix.coefficients[source])
                        + substream.matrix_age as i64 * delta;
                    accumulator += (carried << SCALE) * value;
                    drift += (delta << SCALE) * value;
                }
                if drift != 0 {
                    // The decoder's own integer reciprocal, then shifted up by
                    // two — not a division by the frame count.
                    let reciprocal = (1i64 << 16) / frames.max(1) as i64;
                    accumulator += (drift >> ACCUMULATOR) * frame as i64 * (reciprocal << 2);
                }
                self.buffer[frame][matrix.dest] = (accumulator >> ACCUMULATOR) as i32;
            }
        }

        let kept = frames - substream.shorten_by;
        self.out_channels = span + 1;
        for frame in 0..kept {
            for out in 0..=span {
                // The assignment says, for each matrix channel, which output
                // it is — `ch_assign[matrix] = output` — so an output is the
                // matrix channel that names it. Reading it the other way round
                // is invisible on a permutation that is its own inverse, which
                // every presentation written before the elements were
                // permuted happened to be.
                let channel = (0..=span)
                    .find(|matrix| substream.ch_assign[*matrix] == out)
                    .unwrap_or(out);
                let sample = self.buffer[frame][channel] << substream.output_shift[channel];
                // The check the next restart header certifies. The shift wraps
                // at eight channels, which is invisible below sixteen of them.
                substream.lossless_check ^= (sample as u32 & 0x00ff_ffff) << (channel & 7);
                self.out.push(sample);
            }
        }
        Ok(())
    }
}

/// The immersive syntax: a coefficient mask, a shift on the whole matrix, and
/// coefficients that may move across the access unit.
fn immersive_matrix_params(
    substream: &mut Substream,
    bits: &mut BitReader<'_>,
    sources: usize,
) -> Result<()> {
    let span = substream.max_matrix_channel;
    if !bits.get_bit().map_err(text)? {
        // No matrix — and the bit that says whether anything interpolates is
        // read anyway, whether or not there is anything to interpolate.
        if bits.get_bit().map_err(text)? {
            return Err("nothing to interpolate, and something does".into());
        }
        return Ok(());
    }
    if !bits.get_bit().map_err(text)? {
        return Err("re-coefficiented matrices are not decoded here".into());
    }
    // This syntax writes the count as one less than itself, which is why it
    // cannot say "no matrices" that way and says it with the leading bit.
    let count = bits.get(4).map_err(text)? as usize + 1;
    if count > MAX_MATRICES_IMMERSIVE {
        return Err(format!("{count} matrices is more than the format has"));
    }

    let mut masks = Vec::with_capacity(count);
    let mut fracs = Vec::with_capacity(count);
    for _ in 0..count {
        let dest = bits.get(4).map_err(text)? as usize;
        let frac_bits = bits.get(4).map_err(text)?;
        // Read as one less than the field, so one is none. It scales the
        // whole matrix on top of its fractional bits.
        let shift = bits.get(3).map_err(text)?;
        if bits.get(2).map_err(text)? != 0 {
            return Err("a matrix with bypassed low bits is not decoded here".into());
        }
        if bits.get(4).map_err(text)? != 0 {
            return Err("a matrix that adds dither is not decoded here".into());
        }
        if dest > span {
            return Err("a matrix writes a channel past the matrix span".into());
        }
        if frac_bits > FRACTION {
            return Err(format!("{frac_bits} fractional bits"));
        }
        // Read as one less than the field: one is none, and a halving —
        // which loses the destination's low bit unless the stream carries it
        // bypassed — is not decoded here.
        if shift == 0 {
            return Err("a matrix halved".into());
        }
        if shift - 1 > frac_bits {
            return Err(format!(
                "a matrix scaled by 2^{} at {frac_bits} fractional bits cannot keep its \
                 destination at one",
                shift - 1
            ));
        }
        masks.push(bits.get(sources as u32).map_err(text)?);
        fracs.push((frac_bits, shift - 1));
        substream.matrices.push(Matrix {
            dest,
            ..Matrix::default()
        });
    }

    // The coefficients follow all the headers rather than each its own.
    for (matrix, (mask, (frac_bits, shift))) in
        substream.matrices.iter_mut().zip(masks.iter().zip(&fracs))
    {
        for source in 0..sources {
            if mask >> source & 1 == 1 {
                let stored = bits.get_signed(frac_bits + 2).map_err(text)?;
                // The shift scales the whole matrix: the decoder's
                // `coefficient << (18 + shift - frac_bits)` against an
                // accumulator shifted down by eighteen, which in these
                // fourteen-bit units is this.
                matrix.coefficients[source] = stored * (1 << (FRACTION - frac_bits + shift));
            }
        }
    }

    if bits.get_bit().map_err(text)? {
        let shifts: Vec<u32> = fracs.iter().map(|(_, shift)| *shift).collect();
        interpolation(&mut substream.matrices, bits, &masks, &shifts, sources)?;
    }
    for matrix in &substream.matrices {
        check_noise_sources(matrix, span, sources)?;
    }
    Ok(())
}

/// How far each matrix moves per access unit.
///
/// A decoder ramps the coefficient across the unit and adds the whole step at
/// the end of it, so one statement tracks a trend for as long as the matrix
/// stands.
fn interpolation(
    matrices: &mut [Matrix],
    bits: &mut BitReader<'_>,
    masks: &[u32],
    shifts: &[u32],
    sources: usize,
) -> Result<()> {
    if !bits.get_bit().map_err(text)? {
        return Err("steps that are somewhere else are not decoded here".into());
    }
    if !bits.get_bit().map_err(text)? {
        return Err("steps re-using an earlier statement's widths are not decoded here".into());
    }
    let mut widths = Vec::with_capacity(matrices.len());
    for _ in 0..matrices.len() {
        widths.push(bits.get(4).map_err(text)?);
        if bits.get(2).map_err(text)? != 0 {
            return Err("a step scaled beyond its own fractional bits".into());
        }
    }
    for (((matrix, width), mask), shift) in matrices.iter_mut().zip(&widths).zip(masks).zip(shifts)
    {
        // A matrix that does not move says so by its width and states no steps
        // at all.
        if *width == 0 {
            continue;
        }
        // A decoder scales a moving matrix's steps by its shift as well, and
        // this one does not: the encoder never writes both on one matrix, so
        // a stream that carries both is refused rather than misread.
        if *shift != 0 {
            return Err("a scaled matrix that moves is not decoded here".into());
        }
        for source in 0..sources {
            if mask >> source & 1 == 1 {
                matrix.delta[source] = bits.get_signed(width + 1).map_err(text)?;
            }
        }
    }
    Ok(())
}

/// Restart sync word A gives a matrix two synthetic noise channels as extra
/// sources. Nothing here synthesises them, so a stream that actually used one
/// has to be refused rather than decoded as if the noise were silence.
fn check_noise_sources(matrix: &Matrix, span: usize, sources: usize) -> Result<()> {
    for source in span + 1..sources {
        if matrix.coefficients[source] != 0 || matrix.delta[source] != 0 {
            return Err("a matrix reads a synthesised noise channel".into());
        }
    }
    Ok(())
}

/// A filter's taps, and the history a second filter may arrive with.
struct Read {
    order: usize,
    coefficients: [i32; MAX_FIR_ORDER],
    shift: Option<u32>,
    state: Option<[i32; MAX_IIR_ORDER]>,
}

fn apply_taps(taps: &mut Taps, read: Read) {
    taps.order = read.order;
    taps.coefficients = read.coefficients;
    // A filter of order zero states no shift, so the one in force stands.
    if let Some(shift) = read.shift {
        taps.shift = shift;
    }
}

fn filter_params(bits: &mut BitReader<'_>, max_order: usize, second: bool) -> Result<Read> {
    let order = bits.get(4).map_err(text)? as usize;
    if order > max_order {
        return Err(format!("a filter of order {order}"));
    }
    let mut read = Read {
        order,
        coefficients: [0; MAX_FIR_ORDER],
        shift: None,
        state: None,
    };
    if order == 0 {
        return Ok(read);
    }

    read.shift = Some(bits.get(4).map_err(text)?);
    let coeff_bits = bits.get(5).map_err(text)?;
    let coeff_shift = bits.get(3).map_err(text)?;
    if !(1..=16).contains(&coeff_bits) || coeff_bits + coeff_shift > 16 {
        return Err(format!(
            "coefficients of {coeff_bits} bits shifted by {coeff_shift}"
        ));
    }
    for slot in read.coefficients[..order].iter_mut() {
        *slot = bits.get_signed(coeff_bits).map_err(text)? * (1 << coeff_shift);
    }

    // A history to install along with the taps. Only a second filter may
    // carry one: a first filter predicts from reconstructed samples, which a
    // decoder has, and a stream that says otherwise is malformed.
    if bits.get_bit().map_err(text)? {
        if !second {
            return Err("a first filter arriving with a history".into());
        }
        let state_bits = bits.get(4).map_err(text)?;
        let state_shift = bits.get(4).map_err(text)?;
        let mut values = [0i32; MAX_IIR_ORDER];
        for slot in values[..order].iter_mut() {
            *slot = bits.get_signed(state_bits).map_err(text)? * (1 << state_shift);
        }
        read.state = Some(values);
    }
    Ok(read)
}

/// One codeword, walked bit by bit against the codebook.
fn symbol(bits: &mut BitReader<'_>, codebook: u8) -> Result<usize> {
    let table = huffman::table(codebook).ok_or("codebook 0 has no symbols")?;
    let mut code = 0u32;
    for length in 1..=9u32 {
        code = (code << 1) | bits.get(1).map_err(text)?;
        if let Some(index) = table
            .iter()
            .position(|(word, width)| *width == length && *word == code)
        {
            return Ok(index);
        }
    }
    Err(format!(
        "{code:#x} is not a codeword of codebook {codebook}"
    ))
}

/// How wide a restart header is, from what the reader found in it.
///
/// Fixed fields, then six bits of channel assignment each, then the checksum.
fn restart_header_bits(assignments: usize) -> usize {
    121 + 6 * assignments
}

/// The format's way of folding a 32-bit accumulator into the byte it stores.
fn xor_to_byte(mut value: u32) -> u8 {
    value ^= value >> 16;
    value ^= value >> 8;
    value as u8
}

fn text(ran: hz_core::bits_read::Ran) -> String {
    ran.0
}

/// How many substreams the stream declares, which is how many presentations a
/// decoder may stop after.
pub fn substreams(stream: &[u8]) -> Result<usize> {
    let unit = reader::access_unit(stream, 0, false).map_err(|e| e.0)?;
    unit.major_sync
        .map(|sync| sync.substreams)
        .ok_or_else(|| "the stream does not open with a major sync".into())
}
