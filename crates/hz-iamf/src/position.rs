//! Where an object is, as IAMF v2.0 codes it.
//!
//! An object element carries audio only. Its position is a parameter the mix
//! presentation declares for it — a definition with a default, in polar or
//! Cartesian coordinates — and parameter blocks animate it: each block a run
//! of subblocks, each subblock a step to one place or a line from one place
//! to another over its duration.
//!
//! Positions arrive here in ADM Cartesian — the cube a master's positions
//! live in, x to the right, y to the front, z up, each −1 to 1 — and leave in
//! whichever coding the element was given:
//!
//! - **Cartesian**, 8 or 16 bits a coordinate, the cube as it is: the coded
//!   value is the coordinate times 127 or 32767. Nothing is converted, so
//!   nothing a master says about where an object sits in the room is lost.
//! - **Polar**, azimuth (9 bits, degrees, positive to the left), elevation
//!   (8 bits, degrees, positive up) and distance (7 bits, /127): the cube put
//!   onto the sphere by BS.2127's point conversion, which is a statement
//!   about directions rather than about a room.
//!
//! Every field is packed most significant bit first with no padding between
//! them, and the whole padded to a byte: libiamf's own vector `test_000800`
//! codes an object at +90° as `2d 00 7f`, which the tests here reproduce.
//!
//! The syntax was read from libiamf's test vectors and their iamf-tools
//! descriptions, and from the iamf-rs fork's v2.0 parser that decodes them;
//! the IAMF v2.0 text itself was not to hand.

use crate::obu::put_leb128;
use hz_core::bits::BitWriter;

/// How an element's positions are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionKind {
    Polar,
    Cart8,
    Cart16,
}

impl PositionKind {
    /// `param_definition_type` of the single-object form.
    pub fn param_definition_type(self) -> u64 {
        match self {
            Self::Polar => 3,
            Self::Cart8 => 4,
            Self::Cart16 => 5,
        }
    }

    fn widths(self) -> [u32; 3] {
        match self {
            Self::Polar => [9, 8, 7],
            Self::Cart8 => [8, 8, 8],
            Self::Cart16 => [16, 16, 16],
        }
    }

    /// The coded values of an ADM Cartesian position.
    pub fn quantise(self, position: [f64; 3]) -> [i32; 3] {
        match self {
            Self::Cart8 => position.map(|v| (v.clamp(-1.0, 1.0) * 127.0).round() as i32),
            Self::Cart16 => position.map(|v| (v.clamp(-1.0, 1.0) * 32767.0).round() as i32),
            Self::Polar => {
                let [x, y, z] = position;
                let (azimuth, elevation, distance) = hz_core::coords::cartesian_to_polar(x, y, z);
                [
                    // −180 and 180 are the same direction; the field holds
                    // −256..255, and 180 reads back as it went in.
                    azimuth.round().clamp(-180.0, 180.0) as i32,
                    elevation.round().clamp(-90.0, 90.0) as i32,
                    (distance.clamp(0.0, 1.0) * 127.0).round() as i32,
                ]
            }
        }
    }

    /// Pack one position's coded values, most significant bit first.
    fn put(self, w: &mut BitWriter, coded: [i32; 3]) {
        for (value, bits) in coded.into_iter().zip(self.widths()) {
            w.put(bits, (value as u32) & ((1u32 << bits) - 1));
        }
    }
}

/// How a subblock moves its object.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Animation {
    /// At one place for the whole subblock.
    Step([f64; 3]),
    /// From one place to another, linearly over the subblock.
    Linear([f64; 3], [f64; 3]),
}

/// One subblock of a position parameter block: how long, and what it does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Subblock {
    /// In parameter ticks, which are samples here: the rate is the
    /// element's sample rate.
    pub duration: u32,
    pub animation: Animation,
}

/// Append a position parameter definition: the common fields with no
/// durations (mode 1 — every block states its own), then the default.
pub(crate) fn put_definition(
    out: &mut Vec<u8>,
    kind: PositionKind,
    parameter_id: u64,
    rate: u32,
    default: [f64; 3],
) {
    put_leb128(out, parameter_id);
    put_leb128(out, u64::from(rate));
    out.push(1 << 7); // param_definition_mode 1
    let mut w = BitWriter::new();
    kind.put(&mut w, kind.quantise(default));
    out.extend_from_slice(&w.finish());
}

/// Append a parameter block's payload for a mode-1 definition: the id, the
/// duration and how it divides, then each subblock — its own duration first
/// when they are not all alike, then its animation.
pub(crate) fn put_block(
    out: &mut Vec<u8>,
    kind: PositionKind,
    parameter_id: u64,
    subblocks: &[Subblock],
) {
    assert!(!subblocks.is_empty(), "a block has a subblock");
    assert!(
        subblocks.iter().all(|s| s.duration > 0),
        "a subblock has a duration"
    );
    put_leb128(out, parameter_id);
    let duration: u64 = subblocks.iter().map(|s| u64::from(s.duration)).sum();
    put_leb128(out, duration);
    let listed = subblocks.len() > 1;
    if listed {
        put_leb128(out, 0);
        put_leb128(out, subblocks.len() as u64);
    } else {
        // One subblock as long as the block: constant, and implied.
        put_leb128(out, duration);
    }
    for subblock in subblocks {
        if listed {
            put_leb128(out, u64::from(subblock.duration));
        }
        let mut w = BitWriter::new();
        match subblock.animation {
            Animation::Step(at) => {
                put_leb128(out, 0);
                kind.put(&mut w, kind.quantise(at));
            }
            Animation::Linear(from, to) => {
                put_leb128(out, 1);
                // Per coordinate, its start then its end.
                let (from, to) = (kind.quantise(from), kind.quantise(to));
                for (axis, bits) in kind.widths().into_iter().enumerate() {
                    for value in [from[axis], to[axis]] {
                        w.put(bits, (value as u32) & ((1u32 << bits) - 1));
                    }
                }
            }
        }
        out.extend_from_slice(&w.finish());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of libiamf's `test_000800`: a step to +90° azimuth, on the
    /// horizon, at full distance, is `03 2d 00 7f` after the id — written
    /// there as an inter-linear block, whose one value is coded the same.
    #[test]
    fn a_polar_position_packs_as_libiamf_codes_it() {
        let mut w = BitWriter::new();
        PositionKind::Polar.put(&mut w, [90, 0, 127]);
        assert_eq!(w.finish(), [0x2d, 0x00, 0x7f]);
        let mut w = BitWriter::new();
        PositionKind::Polar.put(&mut w, [180, 0, 127]);
        assert_eq!(w.finish(), [0x5a, 0x00, 0x7f]);
        let mut w = BitWriter::new();
        PositionKind::Polar.put(&mut w, [-90, 0, 127]);
        assert_eq!(w.finish(), [0xd3, 0x00, 0x7f]);
    }

    /// The cube's front-left corner is 30° to the left on the horizon, as
    /// BS.2127 maps it; Cartesian codes keep the corner itself.
    #[test]
    fn the_codings_agree_on_where_front_left_is() {
        assert_eq!(PositionKind::Polar.quantise([-1.0, 1.0, 0.0]), [30, 0, 127]);
        assert_eq!(
            PositionKind::Cart8.quantise([-1.0, 1.0, 0.0]),
            [-127, 127, 0]
        );
        assert_eq!(
            PositionKind::Cart16.quantise([-1.0, 1.0, 1.0]),
            [-32767, 32767, 32767]
        );
    }

    #[test]
    fn a_block_states_its_subblocks() {
        let mut out = Vec::new();
        put_block(
            &mut out,
            PositionKind::Cart8,
            7,
            &[
                Subblock {
                    duration: 100,
                    animation: Animation::Step([0.0, 1.0, 0.0]),
                },
                Subblock {
                    duration: 860,
                    animation: Animation::Linear([0.0, 1.0, 0.0], [1.0, 1.0, 0.0]),
                },
            ],
        );
        assert_eq!(
            out,
            [
                7, // parameter id
                0xc0, 0x07, // duration 960
                0,    // constant_subblock_duration: none
                2,    // subblocks
                100,  // the first one's duration
                0, 0, 127, 0, // step: x, y, z
                0xdc, 0x06, // the second one's
                1, 0, 127, 127, 127, 0, 0, // linear: x 0→127, y 127→127, z 0→0
            ]
        );
    }
}
