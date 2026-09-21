//! The high-resolution output timing field, one bit per restart header.
//!
//! A restart header carries sixteen bits of `output_timing`, which wraps every
//! 65 536 samples — a second and a bit. The field here carries the *other*
//! sixteen, so that a decoder joining mid-stream can say where in the
//! programme it has joined and not merely where in the last second. It is one
//! bit wide, so the value is serialised across many restart headers, and the
//! serialisation is a run-length code rather than the bits themselves.
//!
//! # The code
//!
//! Five zeroes open the preamble and a one starts the field. After that the
//! value is built by shifting: from the "zeroes" group a one appends however
//! many zeroes preceded it, up to four; from the "ones" group a one appends a
//! one and then however many zeroes preceded it, up to four. Four of either
//! sends the next run to the other group, which is how a run longer than four
//! is written. Five zeroes end the field.
//!
//! The preamble is written **once**. A decoder that has just read a field
//! waits at the last state of the preamble rather than the first, so the field
//! after it opens with its one and nothing before it — writing the five
//! zeroes again hands that decoder a sixth, which is the fault this is here to
//! avoid in the first place.
//!
//! **A run of six zeroes anywhere else is a malformed field, not an absent
//! one.** Writing zero in every restart header — which is what this did, and
//! what FFmpeg's encoder does — is therefore not "carrying nothing": it is a
//! field a decoder starts reading and then complains about, once every six
//! restart headers.
//!
//! # What the value has to be
//!
//! `(timing << 16) + output_timing - au_index * samples_per_au` is the
//! programme position the stream starts at, and a decoder checks that
//! successive fields agree about it. This stream starts at zero, so the value
//! is simply the sample position's high half at the access unit the field
//! opened in — and because `output_timing` advances by the frame size and
//! nothing else, that identity holds exactly rather than nearly.

/// Serialises the field, a bit at a time.
#[derive(Debug, Default)]
pub struct Timing {
    /// The field being written, and how much of it has gone.
    field: Vec<bool>,
    at: usize,
    /// Whether the preamble is still owed.
    opened: bool,
}

impl Timing {
    /// The bit the next restart header carries.
    ///
    /// `position` is how many samples the stream has written, which is what
    /// the value is taken from when a field opens.
    pub fn next(&mut self, position: u64) -> bool {
        if self.at == self.field.len() {
            self.field = field((position >> 16) as u32);
            if !self.opened {
                // Only the first field needs the preamble; every field after
                // one leaves a decoder waiting at its end.
                self.field.splice(0..0, std::iter::repeat_n(false, 5));
                self.opened = true;
            }
            self.at = 0;
        }
        let bit = self.field[self.at];
        self.at += 1;
        bit
    }
}

/// One whole field: preamble, value, terminator.
fn field(value: u32) -> Vec<bool> {
    let mut out = vec![true];

    // Most significant bit first and no leading zeroes: a decoder builds the
    // value by shifting into it, so a leading zero would say nothing.
    let bits: Vec<bool> = (0..value.checked_ilog2().map_or(0, |top| top + 1))
        .rev()
        .map(|k| value >> k & 1 == 1)
        .collect();

    // Which group the code is in. The value alternates between runs of zeroes
    // and a one followed by zeroes, and each group writes one of the two.
    let mut zeroes = true;
    let mut at = 0;
    while at < bits.len() {
        if !zeroes {
            debug_assert!(bits[at], "the ones group writes a one");
            at += 1;
        }
        let mut run = 0;
        while run < 4 && at + run < bits.len() && !bits[at + run] {
            run += 1;
        }
        out.extend(std::iter::repeat_n(false, run));
        out.push(true);
        at += run;
        zeroes = run == 4;
    }

    // The terminator is read in the ones group, so a field that ended in the
    // other one steps across first, appending nothing.
    if zeroes {
        out.push(true);
    }
    out.extend(std::iter::repeat_n(false, 5));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoder's own state machine, so that what this writes is checked
    /// against what reads it rather than against itself.
    fn read(bits: &[bool]) -> Result<Vec<u32>, &'static str> {
        let (mut state, mut timing) = (0u8, 0u32);
        let mut out = Vec::new();
        for bit in bits {
            match (state, bit) {
                (0, false) => state = 1,
                (0, true) => {}
                (1..=4, true) => state = 0,
                (1..=4, false) => state += 1,
                (5, true) => {
                    state = 6;
                    timing = 0;
                }
                (5, false) => return Err("a sixth zero, which is a malformed field"),
                (6..=10, true) => {
                    timing <<= state - 6;
                    state = if state == 10 { 6 } else { 11 };
                }
                (10, false) => return Err("a zero the data field cannot hold"),
                (6..=9, false) => state += 1,
                (11..=15, true) => {
                    let width = state - 10;
                    timing = (timing << width) + (1 << (width - 1));
                    state = if state == 15 { 6 } else { 11 };
                }
                (15, false) => {
                    out.push(timing);
                    // The decoder waits at the end of the preamble, not the
                    // start of it. Getting this wrong here is what let a
                    // stream out that this module's own tests called correct
                    // and the decoder called malformed.
                    state = 5;
                }
                (11..=14, false) => state += 1,
                _ => unreachable!("every state is covered"),
            }
        }
        Ok(out)
    }

    #[test]
    fn every_value_a_field_can_hold_comes_back() {
        let preamble = [false; 5];
        for value in (0..=u32::from(u16::MAX)).step_by(37) {
            let mut bits = preamble.to_vec();
            bits.extend(field(value));
            assert_eq!(read(&bits), Ok(vec![value]), "value {value}");
        }
        for value in [0, 1, 2, 3, 0x8000, 0xaaaa, 0x5555, 0xfffe, 0xffff] {
            let mut bits = preamble.to_vec();
            bits.extend(field(value));
            assert_eq!(read(&bits), Ok(vec![value]), "value {value}");
        }
    }

    #[test]
    fn fields_run_into_each_other_without_a_gap() {
        let mut bits = vec![false; 5];
        let values: Vec<u32> = (0..20).map(|k| k * 3271 + 17).collect();
        for value in &values {
            bits.extend(field(*value));
        }
        assert_eq!(read(&bits), Ok(values));
    }

    #[test]
    fn writing_zero_for_ever_is_what_a_decoder_complains_about() {
        // The state this replaces: not an absent field but a broken one.
        assert_eq!(
            read(&[false; 8]),
            Err("a sixth zero, which is a malformed field")
        );
    }

    #[test]
    fn a_field_is_never_wider_than_the_headers_between_two_values() {
        // The value changes every 65 536 samples, which at forty a unit and a
        // restart every sixteen is a hundred and two restart headers. A field
        // has to fit in that or it would describe a position already gone.
        let widest = (0..=u32::from(u16::MAX)).map(|v| field(v).len()).max();
        assert_eq!(widest, Some(26));
    }

    #[test]
    fn the_serialiser_hands_out_one_bit_at_a_time() {
        let mut timing = Timing::default();
        let bits: Vec<bool> = (0..200).map(|_| timing.next(0)).collect();
        // Nothing is read back yet — the point is that it never runs dry.
        assert_eq!(bits.len(), 200);
        assert!(
            bits.iter().any(|b| *b),
            "a field that is all zeroes is the fault"
        );
    }
}
