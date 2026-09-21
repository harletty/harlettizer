//! The master set's event stream — the `.atmos.metadata` file.
//!
//! One entry per object per update, in sample order. A real programme runs to
//! tens of thousands of entries per master, so this is the one document in the
//! format where reading and writing cost anything: everything here avoids a
//! second pass over the text.

use crate::yaml::{Num, Real, Writer};
use hz_core::{Error, Result};
use serde::Deserialize;
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventStream {
    #[serde(default)]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub events: Vec<Event>,
}

impl EventStream {
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
        // Roughly what an event costs in this format, so a programme-length
        // stream is written into one allocation instead of a few dozen.
        const BYTES_PER_EVENT: usize = 96;

        let mut w = Writer::with_capacity(32 + self.events.len() * BYTES_PER_EVENT);
        if let Some(rate) = self.sample_rate {
            w.pair(0, "sampleRate", rate);
        }
        w.key(0, "events");
        for event in &self.events {
            w.dash(Writer::dash_indent(0));
            event.write_into(&mut w, Writer::item_indent(0));
        }
        w.finish()
    }
}

/// One object's state at one sample position.
///
/// Every field is optional because the format is a delta stream: an entry
/// states what changed, and what it omits stays in force. That is also why
/// this type must not invent defaults on read — an absent `gain` and a gain of
/// zero mean different things.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Event {
    #[serde(rename = "ID", default)]
    pub id: Option<u32>,
    #[serde(default)]
    pub sample_pos: Option<u64>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub pos: Option<Vec<Num>>,
    #[serde(default)]
    pub snap: Option<bool>,
    #[serde(default)]
    pub elevation: Option<bool>,
    #[serde(default)]
    pub zones: Option<Zones>,
    #[serde(default)]
    pub size: Option<Real>,
    #[serde(rename = "size3D", default)]
    pub size_3d: Option<Vec<Num>>,
    #[serde(default)]
    pub decorr: Option<u32>,
    #[serde(default)]
    pub importance: Option<Real>,
    /// Carried as `-inf` for silence, which is why it is a [`Num`] and not a
    /// plain float.
    #[serde(default)]
    pub gain: Option<Num>,
    #[serde(default)]
    pub ramp_length: Option<u32>,
    #[serde(default)]
    pub trim_bypass: Option<bool>,
    #[serde(default)]
    pub dialog: Option<i32>,
    #[serde(default)]
    pub music: Option<i32>,
    #[serde(default)]
    pub distance: Option<Real>,
    #[serde(default)]
    pub screen_factor: Option<Real>,
    #[serde(default)]
    pub depth_factor: Option<Real>,
    #[serde(default)]
    pub head_track_mode: Option<String>,
    #[serde(default)]
    pub binaural_render_mode: Option<String>,
}

impl Event {
    pub fn with_id(id: u32) -> Self {
        Self {
            id: Some(id),
            ..Self::default()
        }
    }

    fn write_into(&self, w: &mut Writer, indent: usize) {
        w.pair_opt(indent, "ID", self.id);
        w.pair_opt(indent, "samplePos", self.sample_pos);
        w.pair_opt(indent, "active", self.active);
        if let Some(pos) = &self.pos {
            w.flow(indent, "pos", pos);
        }
        w.pair_opt(indent, "snap", self.snap);
        w.pair_opt(indent, "elevation", self.elevation);
        w.pair_opt(indent, "zones", self.zones);
        w.pair_opt(indent, "size", self.size);
        if let Some(size) = &self.size_3d {
            w.flow(indent, "size3D", size);
        }
        w.pair_opt(indent, "decorr", self.decorr);
        w.pair_opt(indent, "importance", self.importance);
        w.pair_opt(indent, "gain", self.gain);
        w.pair_opt(indent, "rampLength", self.ramp_length);
        w.pair_opt(indent, "trimBypass", self.trim_bypass);
        w.pair_opt(indent, "dialog", self.dialog);
        w.pair_opt(indent, "music", self.music);
        w.pair_opt(indent, "distance", self.distance);
        w.pair_opt(indent, "screenFactor", self.screen_factor);
        w.pair_opt(indent, "depthFactor", self.depth_factor);
        w.text_opt(indent, "headTrackMode", self.head_track_mode.as_deref());
        w.text_opt(
            indent,
            "binauralRenderMode",
            self.binaural_render_mode.as_deref(),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum Zones {
    #[default]
    #[serde(rename = "all")]
    All,
    #[serde(rename = "no back")]
    NoBack,
    #[serde(rename = "no sides")]
    NoSides,
    #[serde(rename = "center back")]
    CenterBack,
    #[serde(rename = "screen only")]
    ScreenOnly,
    #[serde(rename = "surround only")]
    SurroundOnly,
}

impl Zones {
    /// The zone constraint as the object metadata payload codes it: nought
    /// for none, then the five constraints in the order the master names
    /// them. The order is the one the bridge's master writer reads the
    /// payload's code back into a name with — see `docs/provenance.md`.
    pub fn code(self) -> u8 {
        match self {
            Self::All => 0,
            Self::NoBack => 1,
            Self::NoSides => 2,
            Self::CenterBack => 3,
            Self::ScreenOnly => 4,
            Self::SurroundOnly => 5,
        }
    }
}

/// The screen and depth factors as the object metadata payload codes them:
/// three bits for how far an object follows the screen, in eighths above
/// nought, and two for how far into it, in quarters. A screen factor of
/// nought is an object following the room, which the payload states by
/// leaving the pair out. See `docs/provenance.md` for the quantisation.
pub fn screen_codes(screen_factor: f64, depth_factor: f64) -> Option<(u8, u8)> {
    if !screen_factor.is_finite() || screen_factor <= 0.0 {
        return None;
    }
    let screen = ((screen_factor * 8.0).round() as i64 - 1).clamp(0, 7) as u8;
    let depth = ((depth_factor / 0.25).round() as i64 - 1).clamp(0, 3) as u8;
    Some((screen, depth))
}

impl fmt::Display for Zones {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::All => "all",
            Self::NoBack => "no back",
            Self::NoSides => "no sides",
            Self::CenterBack => "center back",
            Self::ScreenOnly => "screen only",
            Self::SurroundOnly => "surround only",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One bed entry and one object entry, in the exact shape real masters use.
    const SAMPLE: &str = "sampleRate: 48000\n\
        events:\n\
        \x20 - ID: 3\n\
        \x20   samplePos: 0\n\
        \x20   active: true\n\
        \x20   importance: 1.0\n\
        \x20   gain: 0\n\
        \x20   rampLength: 1152\n\
        \x20   trimBypass: false\n\
        \x20   headTrackMode: undefined\n\
        \x20   binauralRenderMode: off\n\
        \x20 - ID: 10\n\
        \x20   samplePos: 0\n\
        \x20   active: true\n\
        \x20   pos: [-1, 1, 0]\n\
        \x20   snap: false\n\
        \x20   elevation: true\n\
        \x20   zones: all\n\
        \x20   size: 0.0\n\
        \x20   importance: 1.0\n\
        \x20   gain: -inf\n\
        \x20   rampLength: 1152\n\
        \x20   trimBypass: false\n\
        \x20   dialog: -1\n\
        \x20   music: -1\n\
        \x20   screenFactor: 0.0\n\
        \x20   depthFactor: 0.25\n\
        \x20   headTrackMode: undefined\n\
        \x20   binauralRenderMode: undefined\n";

    #[test]
    fn parses_a_real_event_stream() {
        let stream = EventStream::parse(SAMPLE).unwrap();
        assert_eq!(stream.sample_rate, Some(48000));
        assert_eq!(stream.events.len(), 2);

        let bed = &stream.events[0];
        assert_eq!(bed.id, Some(3));
        assert!(bed.pos.is_none(), "a bed entry carries no position");
        assert_eq!(bed.binaural_render_mode.as_deref(), Some("off"));

        let object = &stream.events[1];
        assert_eq!(object.pos, Some(vec![Num(-1.0), Num(1.0), Num(0.0)]));
        assert_eq!(object.zones, Some(Zones::All));
        assert!(object.gain.unwrap().0.is_infinite());
    }

    #[test]
    fn writes_back_exactly_what_it_read() {
        let stream = EventStream::parse(SAMPLE).unwrap();
        assert_eq!(stream.to_yaml(), SAMPLE);
    }

    /// `off` is a boolean in YAML 1.1 and a string in 1.2. Getting this wrong
    /// silently turns a render mode into `false`.
    #[test]
    fn off_stays_a_render_mode() {
        let stream = EventStream::parse(SAMPLE).unwrap();
        assert!(stream.to_yaml().contains("binauralRenderMode: off"));
        let again = EventStream::parse(&stream.to_yaml()).unwrap();
        assert_eq!(stream, again);
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let text = SAMPLE.replace("    active: true\n", "    active: true\n    newField: 1\n");
        let err = EventStream::parse(&text).unwrap_err();
        assert!(err.contains("newField"), "{err}");
    }

    /// Absent is not zero: a delta stream that fills in omitted fields on read
    /// re-asserts values that were never sent.
    #[test]
    fn an_omitted_field_stays_omitted() {
        let stream = EventStream::parse("events:\n  - ID: 3\n").unwrap();
        assert_eq!(stream.events[0], Event::with_id(3));
        assert_eq!(stream.to_yaml(), "events:\n  - ID: 3\n");
    }
}
