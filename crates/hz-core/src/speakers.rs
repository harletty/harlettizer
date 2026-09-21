//! The bed channels both formats know about, and where they are.
//!
//! This table sits in the bottom crate because three others need the same
//! answers and must not disagree: the master↔ADM projection needs the label
//! and the element id, the panner needs the direction, and the meter needs the
//! weight. Two copies of a speaker table is two chances for a channel to end
//! up in a different place depending on which code path reached it.
//!
//! The master-format names come from the decoder's own speaker enumeration.
//! The labels and their nominal positions are the ITU-R BS.2094 common
//! definitions, read out of a reference implementation's copy rather than
//! recalled — a block whose label and position disagree is one a renderer is
//! entitled to resolve either way.
//!
//! **Azimuth is positive to the left.** That is the convention that catches
//! people, and getting it backwards mirrors the whole programme without
//! anything failing.

/// One bed channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speaker {
    /// The master format's name for it, e.g. `Lss`.
    pub master: &'static str,
    /// The ADM common-definition label, e.g. `M+090`.
    pub label: &'static str,
    /// The element id a master set gives this channel.
    pub id: u32,
    /// Nominal azimuth in degrees, positive to the left.
    pub azimuth: f64,
    /// Nominal elevation in degrees, positive up.
    pub elevation: f64,
}

impl Speaker {
    /// Whether this channel is an LFE — which is not panned to, and not
    /// measured.
    pub fn is_lfe(&self) -> bool {
        self.master.starts_with("LFE")
    }
}

/// Every bed channel this engine names.
#[rustfmt::skip]
pub const SPEAKERS: [Speaker; 17] = [
    Speaker { master: "L",    label: "M+030", id: 0,  azimuth:   30.0, elevation:   0.0 },
    Speaker { master: "R",    label: "M-030", id: 1,  azimuth:  -30.0, elevation:   0.0 },
    Speaker { master: "C",    label: "M+000", id: 2,  azimuth:    0.0, elevation:   0.0 },
    Speaker { master: "LFE",  label: "LFE1",  id: 3,  azimuth:    0.0, elevation: -30.0 },
    Speaker { master: "Lss",  label: "M+090", id: 4,  azimuth:   90.0, elevation:   0.0 },
    Speaker { master: "Rss",  label: "M-090", id: 5,  azimuth:  -90.0, elevation:   0.0 },
    Speaker { master: "Lrs",  label: "M+135", id: 6,  azimuth:  135.0, elevation:   0.0 },
    Speaker { master: "Rrs",  label: "M-135", id: 7,  azimuth: -135.0, elevation:   0.0 },
    Speaker { master: "Lfh",  label: "U+030", id: 8,  azimuth:   30.0, elevation:  30.0 },
    Speaker { master: "Rfh",  label: "U-030", id: 9,  azimuth:  -30.0, elevation:  30.0 },
    Speaker { master: "Lts",  label: "U+090", id: 10, azimuth:   90.0, elevation:  30.0 },
    Speaker { master: "Rts",  label: "U-090", id: 11, azimuth:  -90.0, elevation:  30.0 },
    Speaker { master: "Lrh",  label: "U+135", id: 12, azimuth:  135.0, elevation:  30.0 },
    Speaker { master: "Rrh",  label: "U-135", id: 13, azimuth: -135.0, elevation:  30.0 },
    Speaker { master: "Lw",   label: "M+060", id: 14, azimuth:   60.0, elevation:   0.0 },
    Speaker { master: "Rw",   label: "M-060", id: 15, azimuth:  -60.0, elevation:   0.0 },
    Speaker { master: "LFE2", label: "LFE2",  id: 16, azimuth:   45.0, elevation: -30.0 },
];

/// Labels that name the same physical channel as one in the table but sit at a
/// different nominal angle.
///
/// BS.775 puts the 5.1 surrounds at ±110°; the label set the rest of this
/// engine uses has them at ±90°, under the names a 7.1 bed gives its side
/// surrounds. Both spellings occur, and they are the same channel.
#[rustfmt::skip]
const ALIASES: [(&str, &str, f64, f64); 2] = [
    ("M+110", "Lss",  110.0, 0.0),
    ("M-110", "Rss", -110.0, 0.0),
];

/// The speaker a master-format channel name refers to.
pub fn by_master_name(name: &str) -> Option<&'static Speaker> {
    SPEAKERS.iter().find(|s| s.master == name)
}

/// The speaker an ADM label refers to. Exact labels only.
///
/// Deliberately does **not** resolve an alias: a `Speaker` carries an angle,
/// and handing back one whose angle is not the angle the label names is a trap
/// that puts every panned object slightly in the wrong place. Ask
/// [`master_name_for_label`] which channel it is, and [`direction_of`] where it
/// points.
pub fn by_label(label: &str) -> Option<&'static Speaker> {
    SPEAKERS.iter().find(|s| s.label == label)
}

/// Which master-format channel a label names, aliases included.
pub fn master_name_for_label(label: &str) -> Option<&'static str> {
    if let Some(speaker) = by_label(label) {
        return Some(speaker.master);
    }
    ALIASES
        .iter()
        .find(|(alias, _, _, _)| *alias == label)
        .map(|(_, master, _, _)| *master)
}

/// Where a label points, in degrees.
///
/// An alias answers at the angle it names, not the angle of the channel it
/// aliases to: a panner told the surround is at 90° when the room has it at
/// 110° puts everything between them in the wrong place.
pub fn direction_of(label: &str) -> Option<(f64, f64)> {
    if let Some(speaker) = by_label(label) {
        return Some((speaker.azimuth, speaker.elevation));
    }
    ALIASES
        .iter()
        .find(|(alias, _, _, _)| *alias == label)
        .map(|(_, _, azimuth, elevation)| (*azimuth, *elevation))
}

/// Whether a name is the low frequency channel, by either spelling.
///
/// This workspace calls a channel two things: the ADM label `LFE1`, and the
/// master format's own `LFE`. Answering only for the first was a trap rather
/// than a distinction — a master set names its bed channels the second way, so
/// a caller walking one got `false` for the one channel that has no direction
/// at all, and then went looking for a cube position for it. Aliases included,
/// both ways round.
pub fn is_lfe_label(name: &str) -> bool {
    let master = master_name_for_label(name).unwrap_or(name);
    by_master_name(master).is_some_and(|speaker| speaker.master.starts_with("LFE"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The low frequency channel answers to both of its spellings.
    ///
    /// A master set writes `LFE` and an ADM file writes `LFE1`, and a caller
    /// that has one of them should not have to know which.
    #[test]
    fn the_low_frequency_channel_answers_to_either_name() {
        assert!(is_lfe_label("LFE"));
        assert!(is_lfe_label("LFE1"));
        assert!(!is_lfe_label("M+030"));
        assert!(!is_lfe_label("Lss"));
        assert!(!is_lfe_label("nonsense"));
    }

    #[test]
    fn a_channel_is_reachable_by_either_name() {
        assert_eq!(by_master_name("Lss").map(|s| s.label), Some("M+090"));
        assert_eq!(by_label("M+090").map(|s| s.master), Some("Lss"));
        assert_eq!(by_master_name("Nope"), None);
    }

    /// Both spellings of the 5.1 surrounds have to name the same channel, or
    /// a layout written one way stops matching a layout written the other.
    #[test]
    fn the_five_one_surrounds_answer_to_both_spellings() {
        assert_eq!(master_name_for_label("M+110"), Some("Lss"));
        assert_eq!(master_name_for_label("M-110"), Some("Rss"));
        assert_eq!(master_name_for_label("M+090"), Some("Lss"));
        assert_eq!(master_name_for_label("nonsense"), None);
    }

    /// An alias points where it says, not where the channel it aliases to
    /// points. Twenty degrees of error would move every panned object.
    #[test]
    fn an_alias_keeps_the_angle_it_names() {
        assert_eq!(direction_of("M+110"), Some((110.0, 0.0)));
        assert_eq!(direction_of("M+090"), Some((90.0, 0.0)));
        assert_eq!(direction_of("M-110"), Some((-110.0, 0.0)));
    }

    /// And an alias is deliberately not reachable as a `Speaker`, because a
    /// `Speaker` carries an angle and that one would be the wrong angle.
    #[test]
    fn an_alias_is_not_reachable_as_a_speaker() {
        assert_eq!(by_label("M+110"), None);
    }

    #[test]
    fn an_lfe_is_recognised_by_either_name() {
        assert!(is_lfe_label("LFE1"));
        assert!(is_lfe_label("LFE2"));
        assert!(!is_lfe_label("M+110"));
    }

    /// Positive azimuth is to the left. Backwards, this mirrors the whole
    /// programme and nothing fails.
    #[test]
    fn positive_azimuth_is_the_left_side() {
        assert!(by_master_name("L").unwrap().azimuth > 0.0);
        assert!(by_master_name("R").unwrap().azimuth < 0.0);
        assert_eq!(by_master_name("C").unwrap().azimuth, 0.0);
    }

    #[test]
    fn heights_are_above_the_listener_and_lfes_are_marked() {
        assert!(by_master_name("Lfh").unwrap().elevation > 0.0);
        assert!(by_master_name("LFE").unwrap().is_lfe());
        assert!(by_master_name("LFE2").unwrap().is_lfe());
        assert!(!by_master_name("L").unwrap().is_lfe());
    }

    #[test]
    fn element_ids_are_unique_and_so_are_the_names() {
        for (index, speaker) in SPEAKERS.iter().enumerate() {
            for other in &SPEAKERS[index + 1..] {
                assert_ne!(
                    speaker.id, other.id,
                    "{} and {}",
                    speaker.master, other.master
                );
                assert_ne!(speaker.master, other.master);
                assert_ne!(speaker.label, other.label);
            }
        }
    }
}
