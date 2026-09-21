//! The BS.2076 element types an object-based master needs.
//!
//! Not all of the Audio Definition Model — that standard describes matrices,
//! higher-order ambisonics and binaural streams this encoder has no use for.
//! What is modelled is the chain a renderer walks to place an object: a
//! programme selects content, content selects objects, an object names a pack
//! and the track UIDs that feed it, a pack names channel formats, and a
//! channel format is a list of block formats — one per update, each with a
//! time, a position and a gain.
//!
//! Unlike the master format, this model was written from the specification
//! rather than from real files: no third-party ADM file exists on the machine
//! it was developed on. So it is *tolerant* rather than strict — an element it
//! does not model is reported, not rejected and not silently dropped. See
//! [`crate::adm::xml`].

use super::time::Time;
use std::fmt;

/// What kind of audio an element describes. The label is the wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeDefinition {
    DirectSpeakers,
    Matrix,
    Objects,
    HigherOrderAmbisonics,
    Binaural,
    /// A type this crate does not model, kept so it round-trips.
    Other(u16),
}

impl TypeDefinition {
    pub fn label(self) -> String {
        format!("{:04X}", self.code())
    }

    pub fn code(self) -> u16 {
        match self {
            Self::DirectSpeakers => 1,
            Self::Matrix => 2,
            Self::Objects => 3,
            Self::HigherOrderAmbisonics => 4,
            Self::Binaural => 5,
            Self::Other(code) => code,
        }
    }

    pub fn from_code(code: u16) -> Self {
        match code {
            1 => Self::DirectSpeakers,
            2 => Self::Matrix,
            3 => Self::Objects,
            4 => Self::HigherOrderAmbisonics,
            5 => Self::Binaural,
            other => Self::Other(other),
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        u16::from_str_radix(label.trim(), 16)
            .ok()
            .map(Self::from_code)
    }

    /// The type from its written name, which is the other way a file may say
    /// it — `typeDefinition="Objects"` rather than `typeLabel="0003"`.
    ///
    /// BS.2076 says both attributes are present and agree. Files exist that
    /// carry only one, so both are read; `Undefined` is deliberately not here,
    /// because it names no code and a file that says it has said nothing.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "DirectSpeakers" => Self::DirectSpeakers,
            "Matrix" => Self::Matrix,
            "Objects" => Self::Objects,
            "HOA" => Self::HigherOrderAmbisonics,
            "Binaural" => Self::Binaural,
            _ => return None,
        })
    }
}

impl fmt::Display for TypeDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DirectSpeakers => "DirectSpeakers",
            Self::Matrix => "Matrix",
            Self::Objects => "Objects",
            Self::HigherOrderAmbisonics => "HOA",
            Self::Binaural => "Binaural",
            Self::Other(_) => "Undefined",
        })
    }
}

/// Where an object is. The model allows two coordinate systems and a master
/// set uses the Cartesian one, on a unit cube.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Position {
    Cartesian {
        x: f64,
        y: f64,
        z: f64,
    },
    Polar {
        azimuth: f64,
        elevation: f64,
        distance: f64,
    },
}

/// Whether an object jumps to its next position or slides there.
///
/// The flag alone means "jump instantly"; with an interpolation length it
/// means "get there over this long", which is how a master set's ramp is
/// expressed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JumpPosition {
    pub flag: bool,
    pub interpolation_length: Option<Time>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioProgramme {
    pub id: String,
    pub name: String,
    pub start: Option<Time>,
    pub end: Option<Time>,
    pub content_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioContent {
    pub id: String,
    pub name: String,
    pub object_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioObject {
    pub id: String,
    pub name: String,
    pub start: Option<Time>,
    pub duration: Option<Time>,
    pub pack_format_refs: Vec<String>,
    pub track_uid_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioPackFormat {
    pub id: String,
    pub name: String,
    pub type_definition: TypeDefinition,
    pub channel_format_refs: Vec<String>,
}

/// A channel's band limits.
///
/// This is how an LFE says it is an LFE. Without it a renderer has only the
/// speaker label to go on, and a bed channel that loses its low-pass is a bed
/// channel that will be sent full-range somewhere.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Frequency {
    pub low_pass: Option<f64>,
    pub high_pass: Option<f64>,
}

impl Frequency {
    pub fn is_empty(&self) -> bool {
        self.low_pass.is_none() && self.high_pass.is_none()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioChannelFormat {
    pub id: String,
    pub name: String,
    pub type_definition: TypeDefinition,
    pub frequency: Frequency,
    pub blocks: Vec<AudioBlockFormat>,
}

/// One object update: where it is, how loud, and for how long.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioBlockFormat {
    pub id: String,
    pub rtime: Option<Time>,
    pub duration: Option<Time>,
    /// Whether positions are Cartesian. Stated explicitly because a renderer
    /// reading polar coordinates as Cartesian puts everything in the wrong
    /// place without failing.
    pub cartesian: Option<bool>,
    pub position: Option<Position>,
    pub gain: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub depth: Option<f64>,
    pub diffuse: Option<f64>,
    pub importance: Option<i32>,
    pub jump_position: Option<JumpPosition>,
    /// Present on a `DirectSpeakers` block: which speaker this feeds.
    pub speaker_label: Option<String>,
    /// What a master set says about how the object is to be rendered, beyond
    /// where it is — carried for the fold, not written to the description.
    pub hints: Option<RenderHints>,
}

/// A master set's own rendering statements for an object, carried through
/// the description in the master's words.
///
/// BS.2076 has fields for some of these — `channelLock` for the snap,
/// `screenRef` for the screen — and a box geometry for the zones that the
/// master's named zones do not map onto without a rendering decision. So
/// they are carried as the master states them and are neither written to
/// nor read from the description; what reads a description from elsewhere
/// gets none of them, and a fold treats every object of such a file as
/// rendered alike.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderHints {
    pub snap: bool,
    pub elevation: bool,
    pub zones: crate::master::Zones,
    pub screen_factor: f64,
    pub depth_factor: f64,
}

impl Default for RenderHints {
    /// What a master states when it states nothing.
    fn default() -> Self {
        Self {
            snap: false,
            elevation: true,
            zones: crate::master::Zones::All,
            screen_factor: 0.0,
            depth_factor: 0.25,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioStreamFormat {
    pub id: String,
    pub name: String,
    /// `PCM` is `0001`; nothing else is written here.
    pub format_definition: String,
    pub channel_format_ref: Option<String>,
    pub pack_format_ref: Option<String>,
    pub track_format_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioTrackFormat {
    pub id: String,
    pub name: String,
    pub format_definition: String,
    pub stream_format_ref: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioTrackUid {
    /// The `UID` attribute, which is what `chna` points at.
    pub uid: String,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub track_format_ref: Option<String>,
    pub pack_format_ref: Option<String>,
    /// BS.2076-2 lets a track UID name a channel format directly, skipping
    /// the stream and track format pair the first edition required.
    pub channel_format_ref: Option<String>,
}

/// A whole `audioFormatExtended` document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioFormatExtended {
    pub programmes: Vec<AudioProgramme>,
    pub contents: Vec<AudioContent>,
    pub objects: Vec<AudioObject>,
    pub pack_formats: Vec<AudioPackFormat>,
    pub channel_formats: Vec<AudioChannelFormat>,
    pub stream_formats: Vec<AudioStreamFormat>,
    pub track_formats: Vec<AudioTrackFormat>,
    pub track_uids: Vec<AudioTrackUid>,
}

impl AudioFormatExtended {
    pub fn channel_format(&self, id: &str) -> Option<&AudioChannelFormat> {
        self.channel_formats.iter().find(|c| c.id == id)
    }

    pub fn pack_format(&self, id: &str) -> Option<&AudioPackFormat> {
        self.pack_formats.iter().find(|p| p.id == id)
    }

    pub fn track_uid(&self, uid: &str) -> Option<&AudioTrackUid> {
        self.track_uids.iter().find(|t| t.uid == uid)
    }

    /// Resolve a track UID to the channel format that describes it, following
    /// either the first edition's stream/track chain or the second edition's
    /// direct reference.
    pub fn channel_format_for_track(&self, uid: &str) -> Option<&AudioChannelFormat> {
        let track = self.track_uid(uid)?;
        if let Some(direct) = &track.channel_format_ref {
            return self.channel_format(direct);
        }

        let track_format_id = track.track_format_ref.as_ref()?;
        let track_format = self
            .track_formats
            .iter()
            .find(|t| &t.id == track_format_id)?;
        let stream_id = track_format.stream_format_ref.as_ref()?;
        let stream = self.stream_formats.iter().find(|s| &s.id == stream_id)?;
        self.channel_format(stream.channel_format_ref.as_ref()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_labels_are_four_hex_digits() {
        assert_eq!(TypeDefinition::Objects.label(), "0003");
        assert_eq!(TypeDefinition::DirectSpeakers.label(), "0001");
        assert_eq!(
            TypeDefinition::from_label("0003"),
            Some(TypeDefinition::Objects)
        );
        assert_eq!(
            TypeDefinition::from_label("00ff"),
            Some(TypeDefinition::Other(255))
        );
        assert_eq!(TypeDefinition::from_label("zz"), None);
    }

    /// A type this crate does not model still comes back out as it went in.
    #[test]
    fn an_unmodelled_type_keeps_its_label() {
        let kind = TypeDefinition::from_label("0042").unwrap();
        assert_eq!(kind.label(), "0042");
    }

    fn chained() -> AudioFormatExtended {
        AudioFormatExtended {
            channel_formats: vec![AudioChannelFormat {
                id: "AC_00031001".into(),
                name: "obj".into(),
                type_definition: TypeDefinition::Objects,
                frequency: Frequency::default(),
                blocks: Vec::new(),
            }],
            stream_formats: vec![AudioStreamFormat {
                id: "AS_00031001".into(),
                channel_format_ref: Some("AC_00031001".into()),
                track_format_refs: vec!["AT_00031001_01".into()],
                ..Default::default()
            }],
            track_formats: vec![AudioTrackFormat {
                id: "AT_00031001_01".into(),
                stream_format_ref: Some("AS_00031001".into()),
                ..Default::default()
            }],
            track_uids: vec![AudioTrackUid {
                uid: "ATU_00000001".into(),
                track_format_ref: Some("AT_00031001_01".into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_track_resolves_through_the_stream_and_track_chain() {
        let adm = chained();
        let channel = adm.channel_format_for_track("ATU_00000001").unwrap();
        assert_eq!(channel.id, "AC_00031001");
    }

    /// The second edition lets a track UID name its channel format directly.
    /// A resolver that only walks the old chain silently finds nothing.
    #[test]
    fn a_track_resolves_through_a_direct_reference() {
        let mut adm = chained();
        adm.stream_formats.clear();
        adm.track_formats.clear();
        adm.track_uids[0].track_format_ref = None;
        adm.track_uids[0].channel_format_ref = Some("AC_00031001".into());

        let channel = adm.channel_format_for_track("ATU_00000001").unwrap();
        assert_eq!(channel.id, "AC_00031001");
    }

    #[test]
    fn a_broken_chain_resolves_to_nothing_rather_than_panicking() {
        let mut adm = chained();
        adm.stream_formats.clear();
        assert!(adm.channel_format_for_track("ATU_00000001").is_none());
        assert!(adm.channel_format_for_track("ATU_99999999").is_none());
    }
}
