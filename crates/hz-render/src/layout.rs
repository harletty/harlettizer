//! Speaker layouts, named by the labels the rest of this workspace uses.
//!
//! A layout is an ordered list of speaker labels, and the order is the channel
//! order. That is the only thing that makes a downmix matrix meaningful: a
//! matrix is a map between two orders, and a layout that says which speakers
//! are present without saying in what order says nothing useful.

/// A channel-based layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub name: &'static str,
    /// ADM common-definition speaker labels, in channel order.
    pub speakers: Vec<&'static str>,
}

impl Layout {
    pub fn channels(&self) -> usize {
        self.speakers.len()
    }

    /// Where a speaker sits in this layout, if it is here at all.
    pub fn index_of(&self, label: &str) -> Option<usize> {
        self.speakers.iter().position(|s| *s == label)
    }

    pub fn has(&self, label: &str) -> bool {
        self.index_of(label).is_some()
    }

    /// Stereo, in the order everything writes it.
    pub fn stereo() -> Self {
        Self {
            name: "2.0",
            speakers: vec!["M+030", "M-030"],
        }
    }

    /// 5.1, in the order the master format and WAV both use: L R C LFE Ls Rs.
    ///
    /// Not the order AC-3 puts on the wire, and not the order every tool
    /// prints — which is exactly why the layout carries it explicitly.
    pub fn surround_5_1() -> Self {
        Self {
            name: "5.1",
            speakers: vec!["M+030", "M-030", "M+000", "LFE1", "M+110", "M-110"],
        }
    }

    /// 7.1 with side and rear surrounds: L R C LFE Lss Rss Lrs Rrs.
    pub fn surround_7_1() -> Self {
        Self {
            name: "7.1",
            speakers: vec![
                "M+030", "M-030", "M+000", "LFE1", "M+090", "M-090", "M+135", "M-135",
            ],
        }
    }

    /// 7.1.4: 7.1 with four height channels.
    pub fn surround_7_1_4() -> Self {
        Self {
            name: "7.1.4",
            speakers: vec![
                "M+030", "M-030", "M+000", "LFE1", "M+090", "M-090", "M+135", "M-135", "U+030",
                "U-030", "U+135", "U-135",
            ],
        }
    }

    /// The layout with this many channels, if it is one of the standard ones.
    pub fn for_channel_count(channels: usize) -> Option<Self> {
        match channels {
            2 => Some(Self::stereo()),
            6 => Some(Self::surround_5_1()),
            8 => Some(Self::surround_7_1()),
            12 => Some(Self::surround_7_1_4()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_order_is_part_of_the_layout() {
        let layout = Layout::surround_5_1();
        assert_eq!(layout.index_of("M+030"), Some(0));
        assert_eq!(layout.index_of("LFE1"), Some(3));
        assert_eq!(layout.index_of("M-110"), Some(5));
        assert_eq!(layout.index_of("U+030"), None);
    }

    #[test]
    fn the_standard_layouts_have_the_channel_counts_their_names_claim() {
        assert_eq!(Layout::stereo().channels(), 2);
        assert_eq!(Layout::surround_5_1().channels(), 6);
        assert_eq!(Layout::surround_7_1().channels(), 8);
        assert_eq!(Layout::surround_7_1_4().channels(), 12);
    }

    #[test]
    fn a_layout_can_be_recognised_from_a_channel_count() {
        assert_eq!(Layout::for_channel_count(6), Some(Layout::surround_5_1()));
        assert_eq!(Layout::for_channel_count(21), None);
    }
}
