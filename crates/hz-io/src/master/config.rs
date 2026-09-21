//! The master set's config document — the `.atmos` file.
//!
//! It names the other two components, describes the presentation, and lists
//! the bed channels and objects the event stream refers to by ID.
//!
//! Every struct here is `deny_unknown_fields`. That is the point: a key this
//! model does not know about is a key that would be silently dropped on the
//! way back out, and losing a field from a master is not an error anyone would
//! notice until much later.

use crate::yaml::{Num, Real, Writer, scalar_text};
use hz_core::{Error, Result};
use serde::Deserialize;
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Absent from a fifth of the masters seen, and absent from the decoder's
    /// own model of this format — which is how it got dropped there.
    #[serde(default)]
    pub language: Option<String>,
    pub version: String,
    #[serde(default)]
    pub presentations: Vec<Presentation>,
}

impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::parse(&text).map_err(|e| Error::malformed(path, e))
    }

    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        serde_yaml_ng::from_str(text).map_err(|e| e.to_string())
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_yaml()).map_err(|e| Error::io(path, e))
    }

    pub fn to_yaml(&self) -> String {
        let mut w = Writer::with_capacity(512 + self.presentations.len() * 512);
        if let Some(language) = &self.language {
            w.text(0, "language", language);
        }
        w.text(0, "version", &self.version);
        w.key(0, "presentations");
        for p in &self.presentations {
            w.dash(Writer::dash_indent(0));
            p.write_into(&mut w, Writer::item_indent(0));
        }
        w.finish()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Presentation {
    #[serde(rename = "type")]
    pub presentation_type: PresentationType,
    pub simplified: bool,
    /// Name of the event-stream component. Not always the name of the file
    /// that is actually there — see [`crate::master::Components`].
    pub metadata: String,
    /// Name of the audio component, with the same caveat.
    pub audio: String,
    pub offset: Real,
    /// First frame of action.
    #[serde(default)]
    pub ffoa: Option<Real>,
    #[serde(default)]
    pub fps: Option<Fps>,
    #[serde(default)]
    pub sc_number_of_elements: Option<u32>,
    #[serde(default)]
    pub sc_bed_configuration: Option<Vec<u32>>,
    #[serde(default)]
    pub creation_tool: Option<String>,
    #[serde(default)]
    pub creation_tool_version: Option<String>,
    #[serde(default)]
    pub source_codec: Option<SourceCodec>,
    #[serde(rename = "downmixType_5to2", default)]
    pub downmix_type_5to2: Option<DownmixMode>,
    #[serde(rename = "51-to-20_LsRs90degPhaseShift", default)]
    pub ls_rs_90_deg_phase_shift: Option<bool>,
    #[serde(default)]
    pub warp_mode: Option<WarpMode>,
    #[serde(default)]
    pub trim_mode: Option<TrimMode>,
    #[serde(default)]
    pub bed_instances: Vec<BedInstance>,
    #[serde(default)]
    pub objects: Vec<Object>,
}

impl Presentation {
    fn write_into(&self, w: &mut Writer, indent: usize) {
        w.pair(indent, "type", self.presentation_type);
        w.pair(indent, "simplified", self.simplified);
        w.text(indent, "metadata", &self.metadata);
        w.text(indent, "audio", &self.audio);
        w.pair(indent, "offset", self.offset);
        w.pair_opt(indent, "ffoa", self.ffoa);
        w.pair_opt(indent, "fps", self.fps);
        w.pair_opt(indent, "scNumberOfElements", self.sc_number_of_elements);
        if let Some(bed) = &self.sc_bed_configuration {
            w.flow(indent, "scBedConfiguration", bed);
        }
        w.text_opt(indent, "creationTool", self.creation_tool.as_deref());
        w.text_opt(
            indent,
            "creationToolVersion",
            self.creation_tool_version.as_deref(),
        );
        w.pair_opt(indent, "sourceCodec", self.source_codec);
        w.pair_opt(indent, "downmixType_5to2", self.downmix_type_5to2);
        w.pair_opt(
            indent,
            "51-to-20_LsRs90degPhaseShift",
            self.ls_rs_90_deg_phase_shift,
        );
        w.pair_opt(indent, "warpMode", self.warp_mode);
        if let Some(trim) = &self.trim_mode {
            trim.write_into(w, indent);
        }

        w.key(indent, "bedInstances");
        for bed in &self.bed_instances {
            w.dash(Writer::dash_indent(indent));
            bed.write_into(w, Writer::item_indent(indent));
        }

        w.key(indent, "objects");
        for object in &self.objects {
            w.dash(Writer::dash_indent(indent));
            object.write_into(w, Writer::item_indent(indent));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BedInstance {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub group_name: Option<String>,
    pub channels: Vec<Channel>,
}

impl BedInstance {
    fn write_into(&self, w: &mut Writer, indent: usize) {
        w.text_opt(indent, "description", self.description.as_deref());
        w.text_opt(indent, "groupName", self.group_name.as_deref());
        w.key(indent, "channels");
        for channel in &self.channels {
            w.dash(Writer::dash_indent(indent));
            let inner = Writer::item_indent(indent);
            w.text(inner, "channel", &channel.channel);
            w.pair(inner, "ID", channel.id);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    pub channel: String,
    #[serde(rename = "ID")]
    pub id: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Object {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub group_name: Option<String>,
    #[serde(rename = "ID")]
    pub id: u32,
}

impl Object {
    fn write_into(&self, w: &mut Writer, indent: usize) {
        w.text_opt(indent, "description", self.description.as_deref());
        w.text_opt(indent, "groupName", self.group_name.as_deref());
        w.pair(indent, "ID", self.id);
    }
}

/// The per-downmix trim set.
///
/// No master seen carries a populated one — the single example met is the
/// empty map — so the scalar spelling of the values below is modelled on the
/// rest of the format rather than observed. Revisit when a populated example
/// turns up.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct TrimMode {
    #[serde(default)]
    pub no_surrounds_no_heights: Option<TrimOptions>,
    #[serde(default)]
    pub some_surrounds_no_heights: Option<TrimOptions>,
    #[serde(default)]
    pub many_surrounds_no_heights: Option<TrimOptions>,
    #[serde(default)]
    pub no_surrounds_some_heights: Option<TrimOptions>,
    #[serde(default)]
    pub some_surrounds_some_heights: Option<TrimOptions>,
    #[serde(default)]
    pub many_surrounds_some_heights: Option<TrimOptions>,
    #[serde(default)]
    pub no_surrounds_many_heights: Option<TrimOptions>,
    #[serde(default)]
    pub some_surrounds_many_heights: Option<TrimOptions>,
    #[serde(default)]
    pub many_surrounds_many_heights: Option<TrimOptions>,
}

impl TrimMode {
    fn write_into(&self, w: &mut Writer, indent: usize) {
        if self == &Self::default() {
            w.empty_map(indent, "trimMode");
            return;
        }
        w.key(indent, "trimMode");
        let inner = indent + 2;
        for (key, options) in self.entries() {
            if let Some(options) = options {
                w.key(inner, key);
                options.write_into(w, inner + 2);
            }
        }
    }

    fn entries(&self) -> [(&'static str, &Option<TrimOptions>); 9] {
        [
            ("NoSurroundsNoHeights", &self.no_surrounds_no_heights),
            ("SomeSurroundsNoHeights", &self.some_surrounds_no_heights),
            ("ManySurroundsNoHeights", &self.many_surrounds_no_heights),
            ("NoSurroundsSomeHeights", &self.no_surrounds_some_heights),
            (
                "SomeSurroundsSomeHeights",
                &self.some_surrounds_some_heights,
            ),
            (
                "ManySurroundsSomeHeights",
                &self.many_surrounds_some_heights,
            ),
            ("NoSurroundsManyHeights", &self.no_surrounds_many_heights),
            (
                "SomeSurroundsManyHeights",
                &self.some_surrounds_many_heights,
            ),
            (
                "ManySurroundsManyHeights",
                &self.many_surrounds_many_heights,
            ),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrimOptions {
    #[serde(default)]
    pub center_trim: Option<Num>,
    #[serde(default)]
    pub surround_trim: Option<Num>,
    #[serde(default)]
    pub height_trim: Option<Num>,
    #[serde(default)]
    pub front_back_balance_overhead_floor: Option<Num>,
    #[serde(default)]
    pub front_back_balance_listener: Option<Num>,
}

impl TrimOptions {
    fn write_into(&self, w: &mut Writer, indent: usize) {
        w.pair_opt(indent, "centerTrim", self.center_trim);
        w.pair_opt(indent, "surroundTrim", self.surround_trim);
        w.pair_opt(indent, "heightTrim", self.height_trim);
        w.pair_opt(
            indent,
            "frontBackBalanceOverheadFloor",
            self.front_back_balance_overhead_floor,
        );
        w.pair_opt(
            indent,
            "frontBackBalanceListener",
            self.front_back_balance_listener,
        );
    }
}

// ---------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),* $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),*
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self { $(Self::$variant => $text),* })
            }
        }
    };
}

text_enum!(PresentationType {
    Home => "home",
    Cinema => "cinema",
});

text_enum!(SourceCodec {
    TrueHd => "TrueHD",
    Eac3Joc => "EAC3-JOC",
    DtsX714 => "DTS:X-7.1.4",
    DtsX715 => "DTS:X-7.1.5",
    DtsX914 => "DTS:X-9.1.4",
    DtsX71Plus8 => "DTS:X-7.1+8",
});

text_enum!(WarpMode {
    Normal => "normal",
    Warping => "warping",
    ProLogicIIx => "ProLogicIIx",
    LoRo => "LoRo",
});

text_enum!(DownmixMode {
    LoRoStereo => "LoRo_Stereo",
    LtRtProLogic => "LtRt_ProLogic",
    LtRtPlii => "LtRt_PLII",
});

/// Frame rate.
///
/// Deserialized through the scalar's text because `fps: 24` is a YAML number
/// and `fps: 23.976` a float, while the canonical spellings are strings. A
/// derived string-only enum rejects every real master.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fps {
    R23_976,
    R24,
    R25,
    R29_97,
    R29_97Df,
    R30,
}

impl Fps {
    const SPELLINGS: [(Self, &'static str); 6] = [
        (Self::R23_976, "23.976"),
        (Self::R24, "24"),
        (Self::R25, "25"),
        (Self::R29_97, "29.97"),
        (Self::R29_97Df, "29.97df"),
        (Self::R30, "30"),
    ];
}

impl fmt::Display for Fps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = Self::SPELLINGS
            .iter()
            .find(|(v, _)| v == self)
            .map(|(_, t)| *t)
            .unwrap_or("24");
        f.write_str(text)
    }
}

impl<'de> Deserialize<'de> for Fps {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let text = scalar_text(d)?;
        Self::SPELLINGS
            .iter()
            .find(|(_, t)| *t == text)
            .map(|(v, _)| *v)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown frame rate `{text}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "language: eng\n\
        version: 0.5.1\n\
        presentations:\n\
        \x20 - type: home\n\
        \x20   simplified: false\n\
        \x20   metadata: t.1.atmos.metadata\n\
        \x20   audio: t.1.atmos.audio\n\
        \x20   offset: 0.0\n\
        \x20   fps: 24\n\
        \x20   scBedConfiguration: [3]\n\
        \x20   creationTool: truehdd\n\
        \x20   creationToolVersion: 0.4.0\n\
        \x20   sourceCodec: TrueHD\n\
        \x20   warpMode: LoRo\n\
        \x20   trimMode: {}\n\
        \x20   bedInstances:\n\
        \x20     - channels:\n\
        \x20         - channel: LFE\n\
        \x20           ID: 3\n\
        \x20   objects:\n\
        \x20     - ID: 10\n\
        \x20     - ID: 11\n";

    #[test]
    fn parses_every_field_a_real_master_carries() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.language.as_deref(), Some("eng"));
        let p = &config.presentations[0];
        assert_eq!(p.presentation_type, PresentationType::Home);
        assert_eq!(p.fps, Some(Fps::R24));
        assert_eq!(p.source_codec, Some(SourceCodec::TrueHd));
        assert_eq!(p.warp_mode, Some(WarpMode::LoRo));
        assert_eq!(p.trim_mode, Some(TrimMode::default()));
        assert_eq!(p.sc_bed_configuration.as_deref(), Some(&[3u32][..]));
        assert_eq!(p.bed_instances[0].channels[0].channel, "LFE");
        assert_eq!(p.objects.len(), 2);
    }

    #[test]
    fn writes_back_exactly_what_it_read() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.to_yaml(), SAMPLE);
    }

    #[test]
    fn round_trips_through_its_own_output() {
        let config = Config::parse(SAMPLE).unwrap();
        let again = Config::parse(&config.to_yaml()).unwrap();
        assert_eq!(config, again);
    }

    /// The whole reason the model is strict: an unmodelled key must fail
    /// loudly rather than vanish on the way back out.
    #[test]
    fn an_unknown_key_is_refused() {
        let text = SAMPLE.replace("language: eng", "language: eng\nnewField: 1");
        let err = Config::parse(&text).unwrap_err();
        assert!(err.contains("newField"), "{err}");
    }
}
