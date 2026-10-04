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
    /// `loudspeaker_layout`, as the audio element carries it: 15 for an
    /// expanded layout, which `expanded` then names.
    pub loudspeaker_layout: u8,
    /// `expanded_loudspeaker_layout`, for a layout that is a subset of a
    /// larger one (IAMF v1.1 §3.6.2).
    pub expanded: Option<u8>,
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

/// Mono: one channel, `loudspeaker_layout` 0, which a decoder plays from the
/// centre and folds to a stereo pair at −3 dB a side.
pub const MONO: Layout = Layout {
    name: "1.0",
    loudspeaker_layout: 0,
    expanded: None,
    sound_system: 12,
    labels: &["M+000"],
    decoding: &[0],
    coupled: 0,
    stereo: &[[HALF, HALF]],
};

/// Stereo, `loudspeaker_layout` 1, sound system A (0+2+0).
pub const STEREO: Layout = Layout {
    name: "2.0",
    loudspeaker_layout: 1,
    expanded: None,
    sound_system: STEREO_SOUND_SYSTEM,
    labels: &["M+030", "M-030"],
    decoding: &[0, 1],
    coupled: 1,
    stereo: &[[1.0, 0.0], [0.0, 1.0]],
};

/// 5.1, `loudspeaker_layout` 2, sound system B (0+5+0): the surrounds at
/// ±110°.
pub const FIVE_ONE: Layout = Layout {
    name: "5.1",
    loudspeaker_layout: 2,
    expanded: None,
    sound_system: 1,
    labels: &["M+030", "M-030", "M+000", "LFE1", "M+110", "M-110"],
    decoding: &[0, 1, 4, 5, 2, 3],
    coupled: 2,
    stereo: &[
        [1.0, 0.0],
        [0.0, 1.0],
        [HALF, HALF],
        [0.0, 0.0],
        [HALF, 0.0],
        [0.0, HALF],
    ],
};

/// 5.1.2, `loudspeaker_layout` 3, sound system C (2+5+0): 5.1 and a top
/// front pair.
pub const FIVE_ONE_TWO: Layout = Layout {
    name: "5.1.2",
    loudspeaker_layout: 3,
    expanded: None,
    sound_system: 2,
    labels: &[
        "M+030", "M-030", "M+000", "LFE1", "M+110", "M-110", "U+030", "U-030",
    ],
    decoding: &[0, 1, 4, 5, 6, 7, 2, 3],
    coupled: 3,
    stereo: &[
        [1.0, 0.0],
        [0.0, 1.0],
        [HALF, HALF],
        [0.0, 0.0],
        [HALF, 0.0],
        [0.0, HALF],
        [1.0, 0.0],
        [0.0, 1.0],
    ],
};

/// 5.1.4, `loudspeaker_layout` 4, sound system D (4+5+0): 5.1, a top front
/// pair and a top back pair at ±110°.
pub const FIVE_ONE_FOUR: Layout = Layout {
    name: "5.1.4",
    loudspeaker_layout: 4,
    expanded: None,
    sound_system: 3,
    labels: &[
        "M+030", "M-030", "M+000", "LFE1", "M+110", "M-110", "U+030", "U-030", "U+110", "U-110",
    ],
    decoding: &[0, 1, 4, 5, 6, 7, 8, 9, 2, 3],
    coupled: 4,
    stereo: &[
        [1.0, 0.0],
        [0.0, 1.0],
        [HALF, HALF],
        [0.0, 0.0],
        [HALF, 0.0],
        [0.0, HALF],
        [1.0, 0.0],
        [0.0, 1.0],
        [HALF, 0.0],
        [0.0, HALF],
    ],
};

/// 7.1, `loudspeaker_layout` 5, sound system I (0+7+0): side surrounds at
/// ±90° and rear surrounds at ±135°.
pub const SEVEN_ONE: Layout = Layout {
    name: "7.1",
    loudspeaker_layout: 5,
    expanded: None,
    sound_system: 8,
    labels: &[
        "M+030", "M-030", "M+000", "LFE1", "M+090", "M-090", "M+135", "M-135",
    ],
    decoding: &[0, 1, 4, 5, 6, 7, 2, 3],
    coupled: 3,
    stereo: &[
        [1.0, 0.0],
        [0.0, 1.0],
        [HALF, HALF],
        [0.0, 0.0],
        [HALF, 0.0],
        [0.0, HALF],
        [HALF, 0.0],
        [0.0, HALF],
    ],
};

/// 7.1.2, `loudspeaker_layout` 6, IAMF's own sound system 10: 7.1 and a
/// **top front** pair — not the top side pair a 7.1.2 bed elsewhere puts
/// overhead.
pub const SEVEN_ONE_TWO: Layout = Layout {
    name: "7.1.2",
    loudspeaker_layout: 6,
    expanded: None,
    sound_system: 10,
    labels: &[
        "M+030", "M-030", "M+000", "LFE1", "M+090", "M-090", "M+135", "M-135", "U+030", "U-030",
    ],
    decoding: &[0, 1, 4, 5, 6, 7, 8, 9, 2, 3],
    coupled: 4,
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
    ],
};

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
    expanded: None,
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

/// 3.1.2, `loudspeaker_layout` 8, IAMF's own sound system 11: the front
/// three, the LFE and a top front pair.
pub const THREE_ONE_TWO: Layout = Layout {
    name: "3.1.2",
    loudspeaker_layout: 8,
    expanded: None,
    sound_system: 11,
    labels: &["M+030", "M-030", "M+000", "LFE1", "U+030", "U-030"],
    decoding: &[0, 1, 4, 5, 2, 3],
    coupled: 2,
    stereo: &[
        [1.0, 0.0],
        [0.0, 1.0],
        [HALF, HALF],
        [0.0, 0.0],
        [1.0, 0.0],
        [0.0, 1.0],
    ],
};

/// Every single-layer loudspeaker layout of IAMF v1.1 a decoder renders
/// (`loudspeaker_layout` 0 to 8), fewest channels first, and in code order
/// among layouts of one width: the order [`Layout::smallest_holding`] tries
/// them in.
///
/// Not here: binaural (9), which is not a loudspeaker layout, and the
/// expanded layouts of v1.1 other than the LFE — the side, rear and height
/// pairs, 3.0 and 9.1.6 — which the decoders to hand do not render.
pub const LOUDSPEAKER_LAYOUTS: [Layout; 9] = [
    MONO,
    STEREO,
    FIVE_ONE,
    THREE_ONE_TWO,
    FIVE_ONE_TWO,
    SEVEN_ONE,
    FIVE_ONE_FOUR,
    SEVEN_ONE_TWO,
    SEVEN_ONE_FOUR,
];

/// The LFE alone: expanded layout 0, the low-frequency subset of 7.1.4.
///
/// What carries a programme's LFE beside objects, which cannot carry one: an
/// object is panned, and a low-frequency channel has no direction to pan
/// to. One mono substream. Outside the simple and base profiles, which
/// every sequence with objects already is.
pub const LFE: Layout = Layout {
    name: "LFE",
    loudspeaker_layout: 15,
    expanded: Some(0),
    // Not a layout a mix states a loudness on.
    sound_system: 9,
    labels: &["LFE1"],
    decoding: &[0],
    coupled: 0,
    stereo: &[[0.0, 0.0]],
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

    /// Where a channel is in this layout, by its label.
    pub fn index_of(&self, label: &str) -> Option<usize> {
        self.labels.iter().position(|l| *l == label)
    }

    /// The layout with the fewest channels that has every one of `labels`:
    /// the LFE alone as [`LFE`], anything else among
    /// [`LOUDSPEAKER_LAYOUTS`]. `None` when no layout has them all, or when
    /// there are none.
    ///
    /// The labels are matched as they are written: a caller holding another
    /// spelling of a channel — this workspace's `M+090` for a 5.1 surround a
    /// layout calls `M+110` — resolves it first, since which channels count
    /// as one is the caller's knowledge and not the format's.
    pub fn smallest_holding<'a>(
        labels: impl IntoIterator<Item = &'a str> + Clone,
    ) -> Option<Layout> {
        let mut any = false;
        let mut only_lfe = true;
        for label in labels.clone() {
            any = true;
            only_lfe &= label == "LFE1";
        }
        if !any {
            return None;
        }
        if only_lfe {
            return Some(LFE);
        }
        LOUDSPEAKER_LAYOUTS
            .iter()
            .find(|layout| {
                labels
                    .clone()
                    .into_iter()
                    .all(|label| layout.index_of(label).is_some())
            })
            .copied()
    }

    /// Whether substream `index` is a low-frequency channel.
    pub fn is_lfe(&self, index: usize) -> bool {
        let carried = self.substream_channels(index);
        carried.len() == 1 && self.labels[carried[0]].starts_with("LFE")
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

    /// Every layout carries each of its channels once, states a stereo row
    /// for each, and orders its substreams pairs first, then the centre,
    /// then the LFE — the decoding map the reference decoder holds for its
    /// code (iamf-rs `loudspeaker_info`, after libiamf).
    #[test]
    fn every_layout_carries_each_channel_once_in_the_decoders_order() {
        let reference: [(u8, &[usize]); 9] = [
            (0, &[0]),
            (1, &[0, 1]),
            (2, &[0, 1, 4, 5, 2, 3]),
            (3, &[0, 1, 4, 5, 6, 7, 2, 3]),
            (4, &[0, 1, 4, 5, 6, 7, 8, 9, 2, 3]),
            (5, &[0, 1, 4, 5, 6, 7, 2, 3]),
            (6, &[0, 1, 4, 5, 6, 7, 8, 9, 2, 3]),
            (7, &[0, 1, 4, 5, 6, 7, 8, 9, 10, 11, 2, 3]),
            (8, &[0, 1, 4, 5, 2, 3]),
        ];
        for layout in LOUDSPEAKER_LAYOUTS {
            let (_, map) = reference
                .iter()
                .find(|(code, _)| *code == layout.loudspeaker_layout)
                .expect("a code the decoder knows");
            assert_eq!(layout.decoding, *map, "{}", layout.name);
            assert_eq!(layout.stereo.len(), layout.channels(), "{}", layout.name);
            let mut seen = vec![0; layout.channels()];
            for substream in 0..layout.substreams() {
                for &channel in layout.substream_channels(substream) {
                    seen[channel] += 1;
                }
            }
            assert!(seen.iter().all(|&n| n == 1), "{}: {seen:?}", layout.name);
            // The LFE, where there is one, is the last substream, and mono.
            if let Some(lfe) = layout.index_of("LFE1") {
                assert!(layout.is_lfe(layout.substreams() - 1), "{}", layout.name);
                assert_eq!(layout.decoding.last(), Some(&lfe));
            }
        }
    }

    /// The fewest channels that hold a bed: the LFE alone is its own
    /// expanded layout, a 7.1 is a 7.1 and not a 7.1.4, a centre with an
    /// LFE is the first six-channel layout that has both, and a channel no
    /// layout has — a top side — holds nothing.
    #[test]
    fn the_smallest_layout_that_holds_a_bed() {
        let name =
            |labels: &[&str]| Layout::smallest_holding(labels.iter().copied()).map(|l| l.name);
        assert_eq!(name(&["LFE1"]), Some("LFE"));
        assert_eq!(name(&["M+000"]), Some("1.0"));
        assert_eq!(name(&["M-030", "M+030"]), Some("2.0"));
        assert_eq!(name(&["M+000", "LFE1"]), Some("5.1"));
        assert_eq!(
            name(&[
                "M+030", "M-030", "M+000", "LFE1", "M+090", "M-090", "M+135", "M-135"
            ]),
            Some("7.1")
        );
        assert_eq!(name(&["M+030", "LFE1", "U+030"]), Some("3.1.2"));
        assert_eq!(name(&["M+090", "U+030"]), Some("7.1.2"));
        assert_eq!(name(&["M+110", "U+110"]), Some("5.1.4"));
        assert_eq!(name(&["M+135", "U+135"]), Some("7.1.4"));
        assert_eq!(name(&["M+030", "U+090"]), None);
        assert_eq!(name(&[]), None);
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
