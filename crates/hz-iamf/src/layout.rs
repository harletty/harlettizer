//! The loudspeaker layouts an audio element can be coded as.
//!
//! A channel-based element states its layout as a code, and from that code a
//! decoder knows three things the stream never spells out: which channel is
//! which, in what order the substreams carry them, and how to render them to
//! a stereo pair. All three are tables, and a writer that disagrees with the
//! decoder about any of them writes a stream that decodes cleanly to the
//! wrong speakers — so they live together, one row per layout.

/// One loudspeaker layout of IAMF §3.6.2, single layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub name: &'static str,
    /// `loudspeaker_layout`, as the audio element carries it.
    pub loudspeaker_layout: u8,
    /// `sound_system`, as a mix presentation names the same layout when it
    /// states the loudness measured on it.
    pub sound_system: u8,
    /// ADM common-definition labels, in the order the rest of this
    /// workspace interleaves a layout — which for every layout here is also
    /// the decoder's rendering order (IAMF §3.6.2, "loudspeaker location
    /// ordering").
    pub labels: &'static [&'static str],
    /// The channels, as indices into `labels`, in the order the substreams
    /// carry them: the coupled pairs first — front, side, rear, top front,
    /// top back — then centre, then LFE (IAMF §3.6.3.3).
    pub decoding: &'static [usize],
    /// How many of the substreams are coupled pairs; the rest are mono.
    pub coupled: usize,
    /// What each channel contributes to the left and right of a stereo
    /// rendering, in the order of `labels`.
    ///
    /// The stream has no say in this — an element without demixing
    /// parameters is rendered to stereo by the decoder's own direct-speakers
    /// renderer (IAMF §7.3.2.1.1, BS.2127) — so it is what the stereo
    /// loudness has to be measured through, or the figure describes a
    /// rendering nobody plays.
    pub stereo: &'static [[f64; 2]],
}

/// 0.7071…, the −3 dB of a channel shared between two.
const HALF: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// 7.1.4: 7.1 with four height channels. `loudspeaker_layout` 7, sound
/// system J (4+7+0) of BS.2051.
///
/// The stereo row is the reference decoder's (libiamf v1.1.0 `m2m_rdr.c`,
/// as iamf-rs tabulates it): fronts and front heights straight to their own
/// side, centre, sides, rears and rear heights at −3 dB to theirs, the LFE
/// dropped.
pub const SEVEN_ONE_FOUR: Layout = Layout {
    name: "7.1.4",
    loudspeaker_layout: 7,
    sound_system: 9,
    labels: &[
        "M+030", "M-030", "M+000", "LFE1", "M+090", "M-090", "M+135", "M-135", "U+030", "U-030",
        "U+135", "U-135",
    ],
    decoding: &[0, 1, 4, 5, 6, 7, 8, 9, 10, 11, 2, 3],
    coupled: 5,
    stereo: &[
        [1.0, 0.0],
        [0.0, 1.0],
        [HALF, HALF],
        [0.0, 0.0],
        [HALF, 0.0],
        [0.0, HALF],
        [HALF, 0.0],
        [0.0, HALF],
        [1.0, 0.0],
        [0.0, 1.0],
        [HALF, 0.0],
        [0.0, HALF],
    ],
};

/// The `sound_system` of a stereo pair, sound system A (0+2+0), which every
/// sub-mix has to state a loudness for (IAMF §3.7.4).
pub const STEREO_SOUND_SYSTEM: u8 = 0;

impl Layout {
    pub fn channels(&self) -> usize {
        self.labels.len()
    }

    /// How many substreams carry it.
    pub fn substreams(&self) -> usize {
        self.channels() - self.coupled
    }

    /// The channels substream `index` carries, as indices into `labels`: two
    /// for a coupled one, one otherwise.
    pub fn substream_channels(&self, index: usize) -> &'static [usize] {
        if index < self.coupled {
            &self.decoding[2 * index..2 * index + 2]
        } else {
            let at = self.coupled + index;
            &self.decoding[at..at + 1]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every channel is carried, once.
    #[test]
    fn the_substreams_carry_every_channel_exactly_once() {
        let layout = SEVEN_ONE_FOUR;
        let mut seen = vec![0; layout.channels()];
        for substream in 0..layout.substreams() {
            for &channel in layout.substream_channels(substream) {
                seen[channel] += 1;
            }
        }
        assert!(seen.iter().all(|&n| n == 1), "{seen:?}");
        assert_eq!(layout.substreams(), 7);
        assert_eq!(layout.stereo.len(), layout.channels());
    }

    /// Pairs before mono, and the pairs in the order §3.6.3.3 names them.
    #[test]
    fn the_pairs_are_front_side_rear_top_front_top_back() {
        let layout = SEVEN_ONE_FOUR;
        let pair = |i: usize| -> Vec<&str> {
            layout
                .substream_channels(i)
                .iter()
                .map(|&c| layout.labels[c])
                .collect()
        };
        assert_eq!(pair(0), ["M+030", "M-030"]);
        assert_eq!(pair(1), ["M+090", "M-090"]);
        assert_eq!(pair(2), ["M+135", "M-135"]);
        assert_eq!(pair(3), ["U+030", "U-030"]);
        assert_eq!(pair(4), ["U+135", "U-135"]);
        assert_eq!(pair(5), ["M+000"]);
        assert_eq!(pair(6), ["LFE1"]);
    }
}
