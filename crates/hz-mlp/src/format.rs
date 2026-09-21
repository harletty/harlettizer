// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from the `truehd` crate
// (https://github.com/truehdd/truehdd) — Copyright (c) the truehdd authors —
// under the Apache License, Version 2.0, whose text is reproduced in
// LICENSES/Apache-2.0.txt.
// What was taken and what was changed is recorded in docs/provenance.md.

//! What the format lets a stream say about itself.
//!
//! Everything here is a coded field in the major sync, so the values are the
//! format's rather than ours, and the tables are small enough to state in
//! full rather than compute.

use std::fmt;

/// The largest dead-bit shift a block may declare.
///
/// Re-exported here because it is a fact about the *format* that a caller
/// outside this crate legitimately needs: it is the point below which rounding
/// a signal coarser buys nothing, since the stream can no longer decline to
/// write the zeroes it made. `hz-cli` reads it to bound `--fold-depth`.
pub use crate::frame::MAX_OUTPUT_SHIFT;

/// The most channels any MLP or TrueHD stream carries.
///
/// Eight for a channel-based stream, sixteen for an immersive one: the
/// sixteen-element presentation is a bed and its objects sharing one buffer of
/// channels, and every substream before it is a downmix a decoder may stop at.
pub const MAX_CHANNELS: usize = 16;

/// The first channel of an object programme's fourth substream.
///
/// Everything below it is the 7.1 bed the first three substreams carry, which
/// is fixed; everything from here up is elements, and how many there are is
/// what varies between one immersive stream and another.
pub const FIRST_ELEMENT: usize = 8;

/// Whether this channel count is an object programme rather than a channel
/// layout.
///
/// Nine is the smallest that leaves anything for the fourth substream to
/// carry. Shipped streams use twelve, fourteen and sixteen; the odd counts
/// between have never been seen and are not refused, because nothing in the
/// syntax distinguishes them.
pub const fn is_immersive(channels: usize) -> bool {
    channels > FIRST_ELEMENT && channels <= MAX_CHANNELS
}

/// Samples per access unit at the base rate. Doubles with each rate factor.
const BASE_FRAME_SIZE: usize = 40;

/// The largest access unit the format carries, at four times the base rate.
pub const MAX_BLOCK: usize = BASE_FRAME_SIZE << 2;

/// The peak bitrate every real stream declares, in bits per second.
///
/// A declaration, not a measurement: the field says what a decoder must be
/// able to keep up with, and a decoder sizes its input buffer from it. Every
/// stream FFmpeg writes declares this figure, and so does this one — unless
/// the coding in use cannot honour it, in which case understating it would be
/// a lie a decoder acts on. See [`worst_case_bitrate`].
const NOMINAL_PEAK_BITRATE: u64 = 9_600_000;

/// The highest peak the format lets a stream declare, in bits per second.
///
/// A decoder reads the field, multiplies it back out and refuses anything
/// above this. It is a real ceiling and not a formality: sixteen channels of
/// twenty-four-bit audio at 48 kHz come to 18.4 Mbit/s uncompressed, which is
/// over it — so an immersive stream is one the format requires to compress,
/// and an encoder that could not would have nothing legal to declare.
const MAX_PEAK_BITRATE: u64 = 18_000_000;

/// Bytes of framing around a block: the access unit header, the major sync,
/// the substream header, the restart header, the decoding parameters, and the
/// parity and checksum. Rounded up; it is used to declare a ceiling.
const FRAMING_BYTES: u64 = 64;

/// How wide the input samples are.
///
/// The codec works in 24 bits internally whichever this is; sixteen-bit input
/// is shifted up on the way in and back down on the way out, losslessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleBits {
    Sixteen,
    TwentyFour,
}

impl SampleBits {
    /// How far to shift an input word to reach the codec's 24-bit domain.
    pub(crate) fn shift_into_codec(self) -> u32 {
        match self {
            Self::Sixteen => 8,
            Self::TwentyFour => 0,
        }
    }
}

/// A stream this encoder can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub sample_rate: u32,
    pub channels: usize,
    pub bits: SampleBits,
}

/// A configuration the encoder does not implement.
///
/// Its own type rather than [`hz_core::Error`], because every variant of that
/// carries the file the failure is about and a configuration has no file. The
/// caller that does have one — `hz-cli` — attaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported(pub String);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not supported yet: {}", self.0)
    }
}

impl std::error::Error for Unsupported {}

/// The major sync's fields, resolved from a [`Config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Coded {
    /// The 4-bit sample rate code.
    pub rate: u32,
    /// Samples in one access unit.
    pub frame_size: usize,
    /// The declared peak bitrate, already in the field's units.
    pub peak_bitrate: u32,
    /// The six-channel presentation's arrangement bitmap, five bits.
    pub arrangement_6ch: u32,
    /// The eight-channel presentation's, thirteen. Equal to the six-channel
    /// one below 7.1, and not equal to it in an immersive stream, where the
    /// two presentations really are different shapes.
    pub arrangement_8ch: u32,
    /// `thd_substream_info`: which substreams each of the first three
    /// presentations is made of.
    pub substream_info: u8,
    /// Its extension, which is what says the sixteen-element presentation
    /// exists and how many substreams it spans.
    pub extended_substream_info: u32,
    /// The presentation modifier, used for all three presentations.
    pub presentation_mod: u32,
    /// The major sync's flags word.
    pub flags: u32,
    /// How many substreams the access unit carries.
    pub substreams: usize,
    /// What each substream codes and what its matrices may reach.
    pub ranges: [Substream; MAX_SUBSTREAMS],
    /// What the sixteen-element presentation is, when there is one. Written
    /// as the major sync's extra channel meaning, which is also what makes an
    /// immersive major sync longer than a channel-based one.
    pub immersive: Option<Immersive>,
    /// Stream position to input channel.
    ///
    /// The format's channel order is the order of the arrangement's groups —
    /// left and right, centre, low frequency, *side* surrounds, then rear
    /// surrounds. A WAV file's is the order of its channel mask, which puts
    /// the rears first. They agree up to 5.1 and disagree at 7.1, and an
    /// encoder that ignores the difference produces a stream whose surrounds
    /// are swapped and whose every check passes.
    pub order: [usize; MAX_CHANNELS],
}

/// The most substreams this encoder writes, and the most the format has.
pub const MAX_SUBSTREAMS: usize = 4;

/// Presentations a stream declares: two channels, six, eight, and sixteen
/// elements. One per substream at most, and never more than there are.
pub const MAX_PRESENTATIONS: usize = 4;

/// The restart header's sync word, of which there are three.
///
/// Not thirteen bits of sync and a noise-type flag — that reading works for
/// the first two and refuses the third, which is how FFmpeg's decoder comes to
/// be unable to read an immersive stream at all. The three words select three
/// different matrix syntaxes, and which one a substream may use depends on
/// where it sits:
///
/// - **A** is substream 0's, and substream 1's when substream 1 terminates the
///   six-channel presentation. Its matrices take two synthetic noise channels
///   as extra sources.
/// - **B** is substream 1's and substream 2's. No noise sources; each matrix
///   carries a dither scale of its own instead.
/// - **C** belongs to substream 3 and to nothing else. Its matrices are
///   described differently again — a coefficient mask, a per-matrix shift, and
///   coefficients that interpolate across the access unit — and its samples
///   may be thirty-two bits wide rather than twenty-four.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartSync {
    #[default]
    A,
    B,
    C,
}

impl RestartSync {
    /// The fourteen bits it is written as.
    pub fn word(self) -> u32 {
        match self {
            Self::A => 0x31ea,
            Self::B => 0x31eb,
            Self::C => 0x31ec,
        }
    }

    pub fn from_word(word: u32) -> Option<Self> {
        match word {
            0x31ea => Some(Self::A),
            0x31eb => Some(Self::B),
            0x31ec => Some(Self::C),
            _ => None,
        }
    }
}

/// One substream's share of the channels.
///
/// TrueHD past two channels is two substreams: the first carries a stereo
/// presentation a two-channel decoder can stop after, the second carries the
/// rest. They share one buffer of channels, which is why the second's matrices
/// may reach back into the first's — `max_matrix` spans everything, while only
/// `first..=last` is actually coded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Substream {
    pub first: usize,
    pub last: usize,
    pub max_matrix: usize,
    /// Which of the three restart sync words this substream carries, and so
    /// which matrix syntax it speaks.
    pub sync: RestartSync,
}

/// Above this many matrix channels sync word A is refused and B has to be
/// written instead. A decoder rejects the stream outright otherwise, saying it
/// has more channels than it supports — which is what A's two extra sources
/// add up to.
///
/// This is why FFmpeg's encoder has a `TODO 0x31eb` where it writes that word,
/// and stops at six channels.
const MAX_MATRIX_FOR_SYNC_A: usize = 5;

impl Substream {
    pub fn channels(&self) -> usize {
        self.last + 1 - self.first
    }

    /// How many source channels a matrix in this substream writes
    /// coefficients for.
    pub fn matrix_sources(&self) -> usize {
        match self.sync {
            // Plus the two noise channels a decoder synthesises. Their
            // coefficients are always zero here; the presence bits are not
            // optional.
            RestartSync::A => self.max_matrix + 3,
            RestartSync::B | RestartSync::C => self.max_matrix + 1,
        }
    }

    /// Whether each matrix here carries a dither scale of its own.
    pub fn matrix_dither(&self) -> bool {
        matches!(self.sync, RestartSync::B)
    }
}

/// The dynamic range control one presentation asks for.
///
/// It rides in the substream directory, in a second word each entry may carry
/// — which is what makes a directory entry four bytes rather than two. It is
/// metadata and not coding: a decoder asked for full dynamic range ignores it
/// entirely, and the samples come back the same either way.
///
/// The reference stream sends one about once in a hundred access units, per
/// substream, and the two-channel presentation asks for markedly more
/// attenuation than the wider ones — −7.6 dB against −3.6 — which is what
/// folding sixteen elements into two costs in headroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DynamicRange {
    /// The gain, in sixty-fourths of a power of two: `2^(gain/64)`, so a step
    /// is about 0.094 dB and the field spans ±24. Nine bits, signed.
    pub gain: i32,
    /// How long this value is good for, as a power of two access units. The
    /// next word has to arrive within `2^refresh` of this one or a decoder
    /// says the schedule has lapsed. Three bits.
    pub refresh: u32,
}

impl DynamicRange {
    /// The widest the gain field goes, either way.
    pub const MAX_GAIN: i32 = 1 << 8;
    /// And the longest a value may stand, as the exponent.
    pub const MAX_REFRESH: u32 = 7;

    /// A gain in decibels, rounded to the field's step.
    ///
    /// `None` if it does not fit: the field spans about ±24 dB.
    pub fn from_db(db: f64, refresh: u32) -> Option<Self> {
        let steps = (db / 6.020_599_913_279_624 * 64.0).round();
        let gain = steps as i32;
        (steps.is_finite()
            && (-Self::MAX_GAIN..Self::MAX_GAIN).contains(&gain)
            && refresh <= Self::MAX_REFRESH)
            .then_some(Self { gain, refresh })
    }

    /// What it comes to in decibels.
    pub fn db(&self) -> f64 {
        f64::from(self.gain) / 64.0 * 6.020_599_913_279_624
    }

    /// How many access units may pass before the next word is due.
    pub fn deadline(&self) -> u64 {
        1 << self.refresh
    }
}

/// The gain a decoder applies from a restart until the first update reaches
/// it, in sixteenths of a power of two — and it may not exceed the gain any
/// substream is already running at, which a decoder checks.
///
/// So it follows the gains rather than being chosen: the smallest of them,
/// divided by the four that separates the two fields' units, rounded the way
/// that keeps the inequality true.
///
/// 🔴 Of the gains a decoder is *running*, `running`, as well as those this
/// unit states, `stated`. A decoder that never left compares the start-up
/// gain against the word it read last, not against the one in the unit it is
/// reading; so a unit that raises a substream's gain at its restart, and said
/// a start-up gain that followed the new word, said one a step louder than
/// the decoder was running — which the reference decoder warned about on
/// every such restart, and went on. The smaller of the two is under both.
pub(crate) fn start_up_gain(
    stated: &[Option<DynamicRange>],
    running: &[Option<DynamicRange>],
) -> i32 {
    stated
        .iter()
        .chain(running)
        .flatten()
        .map(|drc| gain_floor_div_4(drc.gain))
        .min()
        .unwrap_or(0)
        .clamp(-64, 63)
}

/// `gain / 4`, rounded towards negative infinity so the result is never
/// louder than the gain it came from.
fn gain_floor_div_4(gain: i32) -> i32 {
    gain.div_euclid(4)
}

/// What the sixteen-element presentation says about itself.
///
/// The one in the reference stream is fifteen dynamic objects and a low
/// frequency channel — `dyn_object_only` with `lfe_present` — which is what an
/// immersive presentation is when nothing in it is a fixed speaker feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Immersive {
    /// Dialogue normalisation for this presentation, in negative decibels.
    pub dialogue_norm: u32,
    /// Its mix level, in decibels above 70.
    pub mix_level: u32,
    /// Dynamic objects in it, which is the element count less the low
    /// frequency channel.
    pub objects: u32,
    pub lfe: bool,
}

impl Coded {
    /// How many channels each declared presentation carries, or zero where it
    /// declares none.
    ///
    /// Worked out the way a decoder does, from `substream_info` and its
    /// extension: each presentation is a *mask* of substreams rather than a
    /// count, and it carries everything up to the highest substream in its
    /// mask. Two fields, four masks, and nothing checks them — a stream whose
    /// masks are wrong does not fail, it reconstructs a presentation from the
    /// wrong substreams.
    pub fn presentation_channels(&self) -> [usize; MAX_PRESENTATIONS] {
        let info = u32::from(self.substream_info);
        let extended = self.extended_substream_info & 3;
        let masks = [
            1,
            (info >> 2) & 3,
            (info >> 4) & 7,
            ((info >> 4) & 8) | (7 ^ (7 >> extended)),
        ];
        let mut channels = [0usize; MAX_PRESENTATIONS];
        for (slot, mask) in channels.iter_mut().zip(masks) {
            // The highest substream the mask names, which is where the
            // presentation ends and so how far its channels reach.
            *slot = (0..self.substreams)
                .rev()
                .find(|substream| mask >> substream & 1 == 1)
                .map_or(0, |substream| self.ranges[substream].last + 1);
        }
        channels
    }
}

impl Config {
    pub(crate) fn resolve(&self) -> Result<Coded, Unsupported> {
        let (rate, factor) = match self.sample_rate {
            48_000 => (0x0, 0),
            96_000 => (0x1, 1),
            192_000 => (0x2, 2),
            44_100 => (0x8, 0),
            88_200 => (0x9, 1),
            176_400 => (0xA, 2),
            other => {
                return Err(Unsupported(format!(
                    "{other} Hz; the format carries 44100, 48000 and their first two doublings"
                )));
            }
        };

        // The bitmap the format uses for a TrueHD channel arrangement: one bit
        // per *group* of channels, not one per channel. The groups are in a
        // fixed order, and that order is also the order the channels go on the
        // wire in — which is not the order a WAV file puts them in past 5.1.
        //
        // `substream_info` and its extension say, between them, which
        // substreams each presentation is made of: bit 1 upwards of
        // `substream_info >> 2` for the six-channel one, of `>> 4` for the
        // eight-channel one, and the extension for the sixteen-element one.
        // Getting them wrong does not fail — a decoder simply reconstructs a
        // presentation from the wrong substreams.
        let (arrangement_6ch, arrangement_8ch, presentation_mod, substream_info, extended) =
            match self.channels {
                1 => (0x0002, 0x0002, 3, 0x14, 0), // C
                2 => (0x0001, 0x0001, 1, 0x14, 0), // L R
                6 => (0x000f, 0x000f, 0, 0x3c, 0), // L R, C, LFE, Ls Rs
                8 => (0x004f, 0x004f, 0, 0x3c, 0), // and the rear pair
                // An object programme: a 7.1 bed in the first three
                // substreams and the elements after it in the fourth. The
                // description is the same whatever the element count — only
                // the fourth substream's range moves — which is what shipped
                // streams do: they carry twelves, fourteens and sixteens, all
                // with this `substream_info` and extension.
                immersive if is_immersive(immersive) => (0x000f, 0x004f, 0, 0xfc, 3),
                other => {
                    return Err(Unsupported(format!(
                        "{other} channels; this writes 1, 2, 6, 8, and {} to {MAX_CHANNELS} \
                         as an object programme",
                        FIRST_ELEMENT + 1
                    )));
                }
            };

        // Each substream is a presentation a decoder may stop after, and each
        // one adds to the buffer of channels the one before it filled. A
        // substream's matrices may reach back over everything decoded so far,
        // which is why its matrix span is not just its own share.
        let (ranges, substreams) = split(self.channels);

        // Identity except at 7.1, where the two orders differ.
        let mut order = [0usize; MAX_CHANNELS];
        for (position, slot) in order.iter_mut().enumerate() {
            *slot = position;
        }
        if self.channels == 8 {
            order[4] = 6; // side left  is the WAV's seventh channel
            order[5] = 7; // side right
            order[6] = 4; // rear left  is the WAV's fifth
            order[7] = 5; // rear right
        }

        // Bit 12 says what shape the access units' extra data has: set, it is
        // an Evolution frame; clear, an opaque payload a decoder hands on
        // without looking at. It is a property of the stream and not of a
        // unit, so it is decided here — an immersive presentation is the one
        // that has object metadata to carry, and the reference sets exactly
        // this bit and nothing else.
        const FLAG_EVOLUTION: u32 = 1 << 12;
        let flags = if is_immersive(self.channels) {
            FLAG_EVOLUTION
        } else {
            0
        };

        Ok(Coded {
            rate,
            frame_size: BASE_FRAME_SIZE << factor,
            peak_bitrate: peak_bitrate(self.sample_rate, self.channels),
            arrangement_6ch,
            arrangement_8ch,
            substream_info,
            extended_substream_info: extended,
            presentation_mod,
            flags,
            substreams,
            ranges,
            immersive: is_immersive(self.channels).then_some(Immersive {
                dialogue_norm: 31,
                mix_level: 35,
                // The first element is the low frequency channel and the rest
                // are dynamic objects, which is the shape a master set has and
                // the shape every immersive reference stream declares.
                objects: self.channels as u32 - 1,
                lfe: true,
            }),
            order,
        })
    }
}

/// How the channels are shared out between substreams.
///
/// The sync word is stated per substream rather than derived. A rule would
/// have to reproduce four facts at once — that substream 0 is always A, that
/// substream 3 is always C, that B is refused on substream 0, and that A is
/// refused on substream 1 unless substream 1 terminates the six-channel
/// presentation — and the reference stream states them directly.
fn split(channels: usize) -> ([Substream; MAX_SUBSTREAMS], usize) {
    let at = |first: usize, last: usize, max_matrix: usize, sync: RestartSync| Substream {
        first,
        last,
        max_matrix,
        sync,
    };
    let mut ranges = [Substream::default(); MAX_SUBSTREAMS];
    let count = match channels {
        immersive if is_immersive(immersive) => {
            ranges[0] = at(0, 1, 1, RestartSync::A);
            ranges[1] = at(2, 5, 5, RestartSync::B);
            ranges[2] = at(6, 7, 7, RestartSync::B);
            // Everything past the bed, however many that is. A reference
            // stream of twelve elements ends this substream at channel 11 and
            // one of sixteen at 15; nothing else about the four differs.
            ranges[3] = at(FIRST_ELEMENT, immersive - 1, immersive - 1, RestartSync::C);
            4
        }
        more if more > 2 => {
            ranges[0] = at(0, 1, 1, RestartSync::A);
            let sync = if more - 1 > MAX_MATRIX_FOR_SYNC_A {
                RestartSync::B
            } else {
                RestartSync::A
            };
            ranges[1] = at(2, more - 1, more - 1, sync);
            2
        }
        one_or_two => {
            ranges[0] = at(0, one_or_two - 1, one_or_two - 1, RestartSync::A);
            1
        }
    };
    (ranges, count)
}

/// The peak bitrate field: bits per sample-period, rounded down, in sixteenths.
fn peak_bitrate(sample_rate: u32, channels: usize) -> u32 {
    let declared = NOMINAL_PEAK_BITRATE
        .max(worst_case_bitrate(sample_rate, channels))
        .min(MAX_PEAK_BITRATE);
    (((declared << 4) - 8) / u64::from(sample_rate)) as u32
}

/// What the coding in use can produce at its worst, in bits per second.
///
/// Which, while nothing is compressed, is every sample at twenty-four bits
/// plus the framing — and above 96 kHz that is more than the 9.6 Mbit/s every
/// real stream declares. Declaring 9.6 anyway is not a harmless white lie: a
/// decoder sizes its input FIFO from this field, and one told the stream peaks
/// lower than it does reports the access units as arriving faster than it can
/// take them. Two decoders said exactly that before this was computed.
///
/// The figure comes down on its own as the coding tools land. Until then it is
/// high, and it is honest — up to [`MAX_PEAK_BITRATE`], past which there is
/// nothing honest to say and the ceiling is declared instead. A stream that
/// then really did produce more than that would be one no decoder accepts,
/// which is why [`crate::Stats::peak_bitrate`] measures what was written.
fn worst_case_bitrate(sample_rate: u32, channels: usize) -> u64 {
    let per_sample = 24 * channels as u64;
    // The framing is per access unit, and there are `sample_rate / frame_size`
    // of those a second — which is the same as spreading it over the samples.
    let frames = u64::from(sample_rate);
    per_sample * frames + FRAMING_BYTES * 8 * frames / 40
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gain field is a power of two in sixty-fourths, so a decibel figure
    /// lands on a step and the round trip is only approximate. What matters is
    /// that it is the *nearest* step and that the field's edges are refused.
    #[test]
    fn a_gain_in_decibels_lands_on_the_nearest_step() {
        let at = |db| DynamicRange::from_db(db, 4).map(|drc| drc.gain);
        assert_eq!(at(0.0), Some(0));
        // Six decibels is one power of two, which is sixty-four steps.
        assert_eq!(at(-6.020_599_913_279_624), Some(-64));
        assert_eq!(at(6.020_599_913_279_624), Some(64));
        assert!(DynamicRange::from_db(-3.5, 4).unwrap().db().abs() - 3.5 < 0.05);

        // The field holds nine signed bits, so it spans about twenty-four
        // decibels either way and nothing beyond.
        assert!(at(-24.0).is_some());
        assert!(at(-25.0).is_none());
        assert!(at(f64::NAN).is_none());
        assert!(DynamicRange::from_db(0.0, 8).is_none(), "three bits of it");
    }

    /// The start-up gain follows the gains rather than being chosen: a decoder
    /// checks that what it applies on joining is no louder than what the
    /// stream is already running at, and refuses to be told otherwise.
    #[test]
    fn the_start_up_gain_never_exceeds_the_running_one() {
        let none: [Option<DynamicRange>; MAX_SUBSTREAMS] = [None; MAX_SUBSTREAMS];
        assert_eq!(start_up_gain(&none, &none), 0);

        for gain in [-256i32, -70, -14, -3, -1, 0, 1, 66, 255] {
            let running = DynamicRange { gain, refresh: 4 };
            let start = start_up_gain(&[Some(running), None, None, None], &none);
            // The start-up gain is in sixteenths of a power of two and the
            // running one in sixty-fourths, so the comparison is over four.
            assert!(
                f64::from(start) / 16.0 <= f64::from(gain) / 64.0,
                "gain {gain} gave a start-up gain of {start}"
            );
        }

        // The smallest of them, since one decoder may stop at any substream.
        let quiet = DynamicRange {
            gain: -80,
            refresh: 4,
        };
        let loud = DynamicRange {
            gain: 0,
            refresh: 4,
        };
        assert_eq!(
            start_up_gain(&[Some(loud), Some(quiet), None, None], &none),
            start_up_gain(&[Some(quiet), None, None, None], &none)
        );

        // And under what a decoder is already running: a unit that raises the
        // gain says a start-up gain no louder than the word before it, and
        // a unit that states no word at all follows the one still in force.
        assert_eq!(
            start_up_gain(
                &[Some(loud), None, None, None],
                &[Some(quiet), None, None, None]
            ),
            start_up_gain(&[Some(quiet), None, None, None], &none)
        );
        assert_eq!(
            start_up_gain(&none, &[Some(quiet), None, None, None]),
            start_up_gain(&[Some(quiet), None, None, None], &none)
        );
    }

    /// The field in a stream FFmpeg wrote at 48 kHz. It is not a round number
    /// and it is not derived from anything else in the header, so it is worth
    /// pinning.
    #[test]
    fn the_peak_bitrate_field_matches_a_real_stream() {
        assert_eq!(peak_bitrate(48_000, 2), 3199);
    }

    /// Above 96 kHz the uncompressed coding needs more than the nominal
    /// figure, and the field has to say so.
    #[test]
    fn a_stream_that_cannot_honour_the_nominal_peak_declares_a_higher_one() {
        assert!(
            peak_bitrate(192_000, 2) > peak_bitrate(192_000, 1),
            "the ceiling depends on how many channels are in the unit"
        );
        let declared = u64::from(peak_bitrate(192_000, 2)) * 192_000 / 16;
        assert!(
            declared >= worst_case_bitrate(192_000, 2) - 192_000,
            "declared {declared} against a worst case of {}",
            worst_case_bitrate(192_000, 2)
        );
    }

    #[test]
    fn a_frame_grows_with_the_rate() {
        let at = |rate| {
            Config {
                sample_rate: rate,
                channels: 2,
                bits: SampleBits::TwentyFour,
            }
            .resolve()
            .unwrap()
            .frame_size
        };
        assert_eq!(at(48_000), 40);
        assert_eq!(at(96_000), 80);
        assert_eq!(at(192_000), 160);
        assert_eq!(at(44_100), 40);
        assert_eq!(at(176_400), 160);
    }

    /// Past two channels the stream splits, and the second substream's matrix
    /// span reaches back over the first's channels rather than starting at its
    /// own.
    #[test]
    fn more_than_two_channels_split_into_two_substreams() {
        for (channels, last, sync) in [(6usize, 5usize, RestartSync::A), (8, 7, RestartSync::B)] {
            let coded = Config {
                sample_rate: 48_000,
                channels,
                bits: SampleBits::TwentyFour,
            }
            .resolve()
            .unwrap();
            assert_eq!(coded.substreams, 2);
            assert_eq!(
                coded.ranges[0],
                Substream {
                    first: 0,
                    last: 1,
                    max_matrix: 1,
                    sync: RestartSync::A,
                }
            );
            assert_eq!(
                coded.ranges[1],
                Substream {
                    first: 2,
                    last,
                    max_matrix: last,
                    sync,
                }
            );
            assert_eq!(
                coded.ranges[0].channels() + coded.ranges[1].channels(),
                channels
            );
        }
    }

    /// The immersive split, which is the reference stream's: four
    /// presentations, each adding to the buffer the one before it filled, and
    /// the last of them speaking the sync word only substream 3 may.
    #[test]
    fn sixteen_channels_split_into_four_substreams() {
        let coded = Config {
            sample_rate: 48_000,
            channels: 16,
            bits: SampleBits::TwentyFour,
        }
        .resolve()
        .unwrap();

        assert_eq!(coded.substreams, 4);
        let shape: Vec<(usize, usize, RestartSync)> = coded.ranges[..4]
            .iter()
            .map(|range| (range.first, range.last, range.sync))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, 1, RestartSync::A),
                (2, 5, RestartSync::B),
                (6, 7, RestartSync::B),
                (8, 15, RestartSync::C),
            ]
        );
        for range in &coded.ranges[..4] {
            assert_eq!(
                range.max_matrix, range.last,
                "each substream's matrices reach everything decoded so far"
            );
        }

        // What tells a decoder which substreams each presentation is made of.
        assert_eq!(coded.substream_info, 0xfc);
        assert_eq!(coded.extended_substream_info, 3);
        assert_eq!(coded.arrangement_6ch, 0x000f);
        assert_eq!(coded.arrangement_8ch, 0x004f);
        assert!(coded.immersive.is_some());
    }

    /// What the two fields say, read back the way a decoder reads them. This
    /// is the whole point of the pair, and neither is checkable on its own.
    #[test]
    fn every_channel_count_declares_the_presentations_it_can_serve() {
        let at = |channels| {
            Config {
                sample_rate: 48_000,
                channels,
                bits: SampleBits::TwentyFour,
            }
            .resolve()
            .unwrap()
        };

        // One and two channels: every presentation is the same two channels,
        // because there is only one substream to point at.
        assert_eq!(at(2).presentation_channels(), [2, 2, 2, 0]);
        assert_eq!(at(1).presentation_channels(), [1, 1, 1, 0]);
        // Six and eight: a stereo downmix, then everything.
        assert_eq!(at(6).presentation_channels(), [2, 6, 6, 0]);
        assert_eq!(at(8).presentation_channels(), [2, 8, 8, 0]);
        // Sixteen: one presentation per substream, each adding to the last.
        assert_eq!(at(16).presentation_channels(), [2, 6, 8, 16]);
    }

    /// And a channel-based stream declares no fourth presentation, which is
    /// the same arithmetic saying nothing.
    #[test]
    fn a_channel_based_stream_declares_no_immersive_presentation() {
        for channels in [1usize, 2, 6, 8] {
            let coded = Config {
                sample_rate: 48_000,
                channels,
                bits: SampleBits::TwentyFour,
            }
            .resolve()
            .unwrap();
            assert_eq!(coded.presentation_channels()[3], 0, "{channels} channels");
            assert!(coded.immersive.is_none(), "{channels} channels");
        }
    }

    #[test]
    fn one_and_two_channels_stay_in_one_substream() {
        for channels in [1usize, 2] {
            let coded = Config {
                sample_rate: 48_000,
                channels,
                bits: SampleBits::TwentyFour,
            }
            .resolve()
            .unwrap();
            assert_eq!(coded.substreams, 1);
            assert_eq!(coded.ranges[0].channels(), channels);
        }
    }

    #[test]
    fn an_unsupported_rate_says_which_ones_exist() {
        let error = Config {
            sample_rate: 32_000,
            channels: 2,
            bits: SampleBits::TwentyFour,
        }
        .resolve()
        .unwrap_err();
        assert!(error.to_string().contains("32000 Hz"), "{error}");
    }
}
