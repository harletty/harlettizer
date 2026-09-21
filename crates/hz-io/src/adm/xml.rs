//! Reading and writing the `axml` payload.
//!
//! # Strict where there is proof, tolerant where there is not
//!
//! The master-format model in this crate is `deny_unknown_fields`, and that is
//! defensible because real masters prove it complete. Nothing comparable
//! exists for ADM here: no third-party ADM file is available on this machine,
//! and the only local ADM implementation is one whose licence forbids using it
//! as a source. Being equally strict would mean rejecting real files on the
//! strength of a model nothing has tested.
//!
//! So the reader is tolerant — and tolerant is not the same as careless. An
//! element it does not model is **counted and reported**, never silently
//! dropped. A conversion that loses something says what it lost, which is the
//! honest form of "lossless" when the target format genuinely cannot hold the
//! source's every element.
//!
//! Namespace prefixes are ignored: `<adm:audioObject>` and `<audioObject>` are
//! the same element, and real documents disagree about which to write.

use super::model::*;
use super::time::Time;
use hz_core::{Error, Result};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// What a read left behind, so a caller can say so rather than pretend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ignored {
    /// Element name to how many times it was skipped.
    pub elements: BTreeMap<String, usize>,
}

impl Ignored {
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    fn note(&mut self, name: &str) {
        *self.elements.entry(name.to_string()).or_default() += 1;
    }
}

/// The result of reading an `axml` payload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub adm: AudioFormatExtended,
    pub ignored: Ignored,
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

pub fn parse(path: &Path, text: &str) -> Result<Parsed> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut parsed = Parsed::default();
    let mut found = false;

    loop {
        match reader
            .read_event()
            .map_err(|e| Error::malformed(path, format!("axml: {e}")))?
        {
            // `audioFormatExtended` may sit at any depth: bare, or wrapped in
            // an ebuCore document. Finding it by name rather than by path
            // accepts both without caring which.
            Event::Start(e) if local(e.local_name().as_ref()) == "audioFormatExtended" => {
                found = true;
                read_format_extended(path, &mut reader, &mut parsed)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !found {
        return Err(Error::malformed(path, "axml has no audioFormatExtended"));
    }
    Ok(parsed)
}

fn read_format_extended(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    parsed: &mut Parsed,
) -> Result<()> {
    loop {
        let event = reader
            .read_event()
            .map_err(|e| Error::malformed(path, format!("axml: {e}")))?;
        match event {
            Event::Start(e) => {
                let name = local(e.local_name().as_ref()).to_string();
                let attrs = attributes(path, &e)?;
                match name.as_str() {
                    "audioProgramme" => {
                        let v = read_programme(path, reader, &attrs, parsed)?;
                        parsed.adm.programmes.push(v);
                    }
                    "audioContent" => {
                        let v = read_content(path, reader, &attrs, parsed)?;
                        parsed.adm.contents.push(v);
                    }
                    "audioObject" => {
                        let v = read_object(path, reader, &attrs, parsed)?;
                        parsed.adm.objects.push(v);
                    }
                    "audioPackFormat" => {
                        let v = read_pack_format(path, reader, &attrs, parsed)?;
                        parsed.adm.pack_formats.push(v);
                    }
                    "audioChannelFormat" => {
                        let v = read_channel_format(path, reader, &attrs, parsed)?;
                        parsed.adm.channel_formats.push(v);
                    }
                    "audioStreamFormat" => {
                        let v = read_stream_format(path, reader, &attrs, parsed)?;
                        parsed.adm.stream_formats.push(v);
                    }
                    "audioTrackFormat" => {
                        let v = read_track_format(path, reader, &attrs, parsed)?;
                        parsed.adm.track_formats.push(v);
                    }
                    "audioTrackUID" => {
                        let v = read_track_uid(path, reader, &attrs, parsed)?;
                        parsed.adm.track_uids.push(v);
                    }
                    other => {
                        parsed.ignored.note(other);
                        skip(path, reader, other)?;
                    }
                }
            }
            Event::Empty(e) => parsed.ignored.note(local(e.local_name().as_ref())),
            Event::End(_) | Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

/// Read the children of the element in progress, handing each to `child`.
///
/// `child` returns whether it consumed the element; anything it declines is
/// noted as ignored and skipped, so no branch can drop an element by omission.
fn read_children<F>(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    parsed: &mut Parsed,
    mut child: F,
) -> Result<()>
where
    F: FnMut(&str, &Attrs, &str) -> Result<bool>,
{
    loop {
        let event = reader
            .read_event()
            .map_err(|e| Error::malformed(path, format!("axml: {e}")))?;
        match event {
            Event::Start(e) => {
                let name = local(e.local_name().as_ref()).to_string();
                let attrs = attributes(path, &e)?;
                let text = read_text(path, reader, &name)?;
                if !child(&name, &attrs, &text)? {
                    parsed.ignored.note(&name);
                }
            }
            Event::Empty(e) => {
                let name = local(e.local_name().as_ref()).to_string();
                let attrs = attributes(path, &e)?;
                if !child(&name, &attrs, "")? {
                    parsed.ignored.note(&name);
                }
            }
            Event::End(_) | Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn read_programme(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioProgramme> {
    let mut value = AudioProgramme {
        id: attrs.get("audioProgrammeID"),
        name: attrs.get("audioProgrammeName"),
        start: attrs.time(path, "start")?,
        end: attrs.time(path, "end")?,
        content_refs: Vec::new(),
    };
    let mut refs = Vec::new();
    read_children(path, reader, parsed, |name, _, text| match name {
        "audioContentIDRef" => {
            refs.push(text.to_string());
            Ok(true)
        }
        _ => Ok(false),
    })?;
    value.content_refs = refs;
    Ok(value)
}

fn read_content(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioContent> {
    let mut refs = Vec::new();
    read_children(path, reader, parsed, |name, _, text| match name {
        "audioObjectIDRef" => {
            refs.push(text.to_string());
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(AudioContent {
        id: attrs.get("audioContentID"),
        name: attrs.get("audioContentName"),
        object_refs: refs,
    })
}

fn read_object(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioObject> {
    let mut packs = Vec::new();
    let mut uids = Vec::new();
    read_children(path, reader, parsed, |name, _, text| match name {
        "audioPackFormatIDRef" => {
            packs.push(text.to_string());
            Ok(true)
        }
        "audioTrackUIDRef" => {
            uids.push(text.to_string());
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(AudioObject {
        id: attrs.get("audioObjectID"),
        name: attrs.get("audioObjectName"),
        start: attrs.time(path, "start")?,
        duration: attrs.time(path, "duration")?,
        pack_format_refs: packs,
        track_uid_refs: uids,
    })
}

fn read_pack_format(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioPackFormat> {
    let mut refs = Vec::new();
    read_children(path, reader, parsed, |name, _, text| match name {
        "audioChannelFormatIDRef" => {
            refs.push(text.to_string());
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(AudioPackFormat {
        id: attrs.get("audioPackFormatID"),
        name: attrs.get("audioPackFormatName"),
        type_definition: attrs.type_definition(),
        channel_format_refs: refs,
    })
}

fn read_channel_format(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioChannelFormat> {
    let id = attrs.get("audioChannelFormatID");
    let name = attrs.get("audioChannelFormatName");
    let type_definition = attrs.type_definition();

    let mut frequency = Frequency::default();
    let mut blocks = Vec::new();
    loop {
        let event = reader
            .read_event()
            .map_err(|e| Error::malformed(path, format!("axml: {e}")))?;
        match event {
            Event::Start(e) => {
                let child = local(e.local_name().as_ref()).to_string();
                let child_attrs = attributes(path, &e)?;
                if child == "audioBlockFormat" {
                    blocks.push(read_block_format(path, reader, &child_attrs, parsed)?);
                } else if child == "frequency" {
                    // `<frequency typeDefinition="lowPass">120</frequency>`
                    let text = read_text(path, reader, &child)?;
                    let value = text.trim().parse::<f64>().ok();
                    match child_attrs.get("typeDefinition").as_str() {
                        "lowPass" => frequency.low_pass = value,
                        "highPass" => frequency.high_pass = value,
                        other => {
                            parsed.ignored.note(&format!("frequency[{other}]"));
                        }
                    }
                } else {
                    parsed.ignored.note(&child);
                    skip(path, reader, &child)?;
                }
            }
            Event::Empty(e) => parsed.ignored.note(local(e.local_name().as_ref())),
            Event::End(_) | Event::Eof => break,
            _ => {}
        }
    }

    Ok(AudioChannelFormat {
        id,
        name,
        type_definition,
        frequency,
        blocks,
    })
}

fn read_block_format(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioBlockFormat> {
    let mut block = AudioBlockFormat {
        id: attrs.get("audioBlockFormatID"),
        rtime: attrs.time(path, "rtime")?,
        duration: attrs.time(path, "duration")?,
        ..Default::default()
    };

    // Coordinates arrive one element at a time, each tagged by axis, and the
    // set that arrives says which system is in use.
    let mut cartesian = [None; 3];
    let mut polar = [None; 3];
    let mut jump_flag = None;
    let mut jump_length: Option<Time> = None;
    let mut jump_length_error = None;

    read_children(path, reader, parsed, |name, attrs, text| {
        match name {
            "cartesian" => block.cartesian = Some(text.trim() == "1" || text.trim() == "true"),
            "position" => {
                let value = text.trim().parse::<f64>().ok();
                match attrs.get("coordinate").as_str() {
                    "X" => cartesian[0] = value,
                    "Y" => cartesian[1] = value,
                    "Z" => cartesian[2] = value,
                    "azimuth" => polar[0] = value,
                    "elevation" => polar[1] = value,
                    "distance" => polar[2] = value,
                    _ => return Ok(false),
                }
            }
            "gain" => block.gain = text.trim().parse().ok(),
            "width" => block.width = text.trim().parse().ok(),
            "height" => block.height = text.trim().parse().ok(),
            "depth" => block.depth = text.trim().parse().ok(),
            "diffuse" => block.diffuse = text.trim().parse().ok(),
            "importance" => block.importance = text.trim().parse().ok(),
            "speakerLabel" => block.speaker_label = Some(text.trim().to_string()),
            "jumpPosition" => {
                jump_flag = Some(text.trim() == "1" || text.trim() == "true");
                let length = attrs.get("interpolationLength");
                if !length.is_empty() {
                    // An interpolation length is a duration, and the elapsed
                    // seconds form is what the attribute carries.
                    match parse_interpolation_length(&length) {
                        Some(time) => jump_length = Some(time),
                        None => jump_length_error = Some(length),
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;

    if let Some(bad) = jump_length_error {
        return Err(Error::malformed(
            path,
            format!("interpolationLength `{bad}` is neither a time nor a number of seconds"),
        ));
    }

    block.position = match (cartesian, polar) {
        ([Some(x), Some(y), Some(z)], _) => Some(Position::Cartesian { x, y, z }),
        (_, [Some(azimuth), Some(elevation), distance]) => Some(Position::Polar {
            azimuth,
            elevation,
            // Distance is optional and defaults to the unit sphere.
            distance: distance.unwrap_or(1.0),
        }),
        _ => None,
    };
    if let Some(flag) = jump_flag {
        block.jump_position = Some(JumpPosition {
            flag,
            interpolation_length: jump_length,
        });
    }

    Ok(block)
}

fn read_stream_format(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioStreamFormat> {
    let mut channel = None;
    let mut pack = None;
    let mut tracks = Vec::new();
    read_children(path, reader, parsed, |name, _, text| match name {
        "audioChannelFormatIDRef" => {
            channel = Some(text.to_string());
            Ok(true)
        }
        "audioPackFormatIDRef" => {
            pack = Some(text.to_string());
            Ok(true)
        }
        "audioTrackFormatIDRef" => {
            tracks.push(text.to_string());
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(AudioStreamFormat {
        id: attrs.get("audioStreamFormatID"),
        name: attrs.get("audioStreamFormatName"),
        format_definition: attrs.get("formatLabel"),
        channel_format_ref: channel,
        pack_format_ref: pack,
        track_format_refs: tracks,
    })
}

fn read_track_format(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioTrackFormat> {
    let mut stream = None;
    read_children(path, reader, parsed, |name, _, text| match name {
        "audioStreamFormatIDRef" => {
            stream = Some(text.to_string());
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(AudioTrackFormat {
        id: attrs.get("audioTrackFormatID"),
        name: attrs.get("audioTrackFormatName"),
        format_definition: attrs.get("formatLabel"),
        stream_format_ref: stream,
    })
}

fn read_track_uid(
    path: &Path,
    reader: &mut Reader<&[u8]>,
    attrs: &Attrs,
    parsed: &mut Parsed,
) -> Result<AudioTrackUid> {
    let mut track = None;
    let mut pack = None;
    let mut channel = None;
    read_children(path, reader, parsed, |name, _, text| match name {
        "audioTrackFormatIDRef" => {
            track = Some(text.to_string());
            Ok(true)
        }
        "audioPackFormatIDRef" => {
            pack = Some(text.to_string());
            Ok(true)
        }
        "audioChannelFormatIDRef" => {
            channel = Some(text.to_string());
            Ok(true)
        }
        _ => Ok(false),
    })?;
    Ok(AudioTrackUid {
        uid: attrs.get("UID"),
        sample_rate: attrs.get("sampleRate").parse().ok(),
        bit_depth: attrs.get("bitDepth").parse().ok(),
        track_format_ref: track,
        pack_format_ref: pack,
        channel_format_ref: channel,
    })
}

// ---------------------------------------------------------------------------
// Reader plumbing
// ---------------------------------------------------------------------------

/// An element's attributes, by local name.
#[derive(Debug, Default)]
struct Attrs(BTreeMap<String, String>);

impl Attrs {
    fn get(&self, name: &str) -> String {
        self.0.get(name).cloned().unwrap_or_default()
    }

    fn time(&self, path: &Path, name: &str) -> Result<Option<Time>> {
        match self.0.get(name) {
            Some(text) if !text.trim().is_empty() => Ok(Some(Time::parse(path, text)?)),
            _ => Ok(None),
        }
    }

    /// The element's type, from either of the two attributes that state it.
    ///
    /// `typeLabel` is the coded one and is preferred; `typeDefinition` is the
    /// written name and is read when the label is missing or is not a number,
    /// which is a file this used to read as an object. Objects remains the
    /// default because a master set's axml is objects and files exist that
    /// state neither — but it is now the answer only when nothing was said,
    /// not whenever the label alone was absent.
    fn type_definition(&self) -> TypeDefinition {
        TypeDefinition::from_label(&self.get("typeLabel"))
            .or_else(|| TypeDefinition::from_name(&self.get("typeDefinition")))
            .unwrap_or(TypeDefinition::Objects)
    }
}

fn attributes(path: &Path, e: &quick_xml::events::BytesStart<'_>) -> Result<Attrs> {
    let mut map = BTreeMap::new();
    for attribute in e.attributes() {
        let attribute =
            attribute.map_err(|e| Error::malformed(path, format!("axml attribute: {e}")))?;
        let name = local(attribute.key.local_name().as_ref()).to_string();
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| Error::malformed(path, format!("axml attribute value: {e}")))?
            .into_owned();
        map.insert(name, value);
    }
    Ok(Attrs(map))
}

/// The part of a name after any namespace prefix.
fn local(name: &[u8]) -> &str {
    let text = std::str::from_utf8(name).unwrap_or_default();
    match text.rsplit_once(':') {
        Some((_, local)) => local,
        None => text,
    }
}

/// Read the text of the element in progress, up to its end tag.
fn read_text(path: &Path, reader: &mut Reader<&[u8]>, name: &str) -> Result<String> {
    let mut text = String::new();
    let mut depth = 0usize;
    loop {
        match reader
            .read_event()
            .map_err(|e| Error::malformed(path, format!("axml in `{name}`: {e}")))?
        {
            Event::Text(t) => text.push_str(
                &t.xml_content(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|e| Error::malformed(path, format!("axml text: {e}")))?,
            ),
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(text)
}

/// Consume the element in progress and everything inside it.
fn skip(path: &Path, reader: &mut Reader<&[u8]>, name: &str) -> Result<()> {
    read_text(path, reader, name).map(|_| ())
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Serialise an `audioFormatExtended` document, wrapped for a BW64 `axml`
/// chunk.
pub fn to_xml(adm: &AudioFormatExtended) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<ebuCoreMain xmlns=\"urn:ebu:metadata-schema:ebuCore_2015\">\n");
    out.push_str("  <coreMetadata>\n    <format>\n      <audioFormatExtended>\n");

    let indent = 8;
    for programme in &adm.programmes {
        open(
            &mut out,
            indent,
            "audioProgramme",
            &[
                ("audioProgrammeID", &programme.id),
                ("audioProgrammeName", &programme.name),
                ("start", &opt_time(programme.start)),
                ("end", &opt_time(programme.end)),
            ],
        );
        for reference in &programme.content_refs {
            leaf(&mut out, indent + 2, "audioContentIDRef", &[], reference);
        }
        close(&mut out, indent, "audioProgramme");
    }

    for content in &adm.contents {
        open(
            &mut out,
            indent,
            "audioContent",
            &[
                ("audioContentID", &content.id),
                ("audioContentName", &content.name),
            ],
        );
        for reference in &content.object_refs {
            leaf(&mut out, indent + 2, "audioObjectIDRef", &[], reference);
        }
        close(&mut out, indent, "audioContent");
    }

    for object in &adm.objects {
        open(
            &mut out,
            indent,
            "audioObject",
            &[
                ("audioObjectID", &object.id),
                ("audioObjectName", &object.name),
                ("start", &opt_time(object.start)),
                ("duration", &opt_time(object.duration)),
            ],
        );
        for reference in &object.pack_format_refs {
            leaf(&mut out, indent + 2, "audioPackFormatIDRef", &[], reference);
        }
        for reference in &object.track_uid_refs {
            leaf(&mut out, indent + 2, "audioTrackUIDRef", &[], reference);
        }
        close(&mut out, indent, "audioObject");
    }

    for pack in &adm.pack_formats {
        open(
            &mut out,
            indent,
            "audioPackFormat",
            &[
                ("audioPackFormatID", &pack.id),
                ("audioPackFormatName", &pack.name),
                ("typeLabel", &pack.type_definition.label()),
                ("typeDefinition", &pack.type_definition.to_string()),
            ],
        );
        for reference in &pack.channel_format_refs {
            leaf(
                &mut out,
                indent + 2,
                "audioChannelFormatIDRef",
                &[],
                reference,
            );
        }
        close(&mut out, indent, "audioPackFormat");
    }

    for channel in &adm.channel_formats {
        open(
            &mut out,
            indent,
            "audioChannelFormat",
            &[
                ("audioChannelFormatID", &channel.id),
                ("audioChannelFormatName", &channel.name),
                ("typeLabel", &channel.type_definition.label()),
                ("typeDefinition", &channel.type_definition.to_string()),
            ],
        );
        if let Some(value) = channel.frequency.low_pass {
            leaf(
                &mut out,
                indent + 2,
                "frequency",
                &[("typeDefinition", "lowPass")],
                &number(value),
            );
        }
        if let Some(value) = channel.frequency.high_pass {
            leaf(
                &mut out,
                indent + 2,
                "frequency",
                &[("typeDefinition", "highPass")],
                &number(value),
            );
        }
        for block in &channel.blocks {
            write_block(&mut out, indent + 2, block);
        }
        close(&mut out, indent, "audioChannelFormat");
    }

    for stream in &adm.stream_formats {
        open(
            &mut out,
            indent,
            "audioStreamFormat",
            &[
                ("audioStreamFormatID", &stream.id),
                ("audioStreamFormatName", &stream.name),
                ("formatLabel", &stream.format_definition),
                (
                    "formatDefinition",
                    pcm_definition(&stream.format_definition),
                ),
            ],
        );
        if let Some(reference) = &stream.channel_format_ref {
            leaf(
                &mut out,
                indent + 2,
                "audioChannelFormatIDRef",
                &[],
                reference,
            );
        }
        if let Some(reference) = &stream.pack_format_ref {
            leaf(&mut out, indent + 2, "audioPackFormatIDRef", &[], reference);
        }
        for reference in &stream.track_format_refs {
            leaf(
                &mut out,
                indent + 2,
                "audioTrackFormatIDRef",
                &[],
                reference,
            );
        }
        close(&mut out, indent, "audioStreamFormat");
    }

    for track in &adm.track_formats {
        open(
            &mut out,
            indent,
            "audioTrackFormat",
            &[
                ("audioTrackFormatID", &track.id),
                ("audioTrackFormatName", &track.name),
                ("formatLabel", &track.format_definition),
                ("formatDefinition", pcm_definition(&track.format_definition)),
            ],
        );
        if let Some(reference) = &track.stream_format_ref {
            leaf(
                &mut out,
                indent + 2,
                "audioStreamFormatIDRef",
                &[],
                reference,
            );
        }
        close(&mut out, indent, "audioTrackFormat");
    }

    for uid in &adm.track_uids {
        open(
            &mut out,
            indent,
            "audioTrackUID",
            &[
                ("UID", &uid.uid),
                ("sampleRate", &opt_number(uid.sample_rate)),
                ("bitDepth", &opt_number(uid.bit_depth)),
            ],
        );
        if let Some(reference) = &uid.track_format_ref {
            leaf(
                &mut out,
                indent + 2,
                "audioTrackFormatIDRef",
                &[],
                reference,
            );
        }
        if let Some(reference) = &uid.channel_format_ref {
            leaf(
                &mut out,
                indent + 2,
                "audioChannelFormatIDRef",
                &[],
                reference,
            );
        }
        if let Some(reference) = &uid.pack_format_ref {
            leaf(&mut out, indent + 2, "audioPackFormatIDRef", &[], reference);
        }
        close(&mut out, indent, "audioTrackUID");
    }

    out.push_str("      </audioFormatExtended>\n    </format>\n  </coreMetadata>\n");
    out.push_str("</ebuCoreMain>\n");
    out
}

fn write_block(out: &mut String, indent: usize, block: &AudioBlockFormat) {
    open(
        out,
        indent,
        "audioBlockFormat",
        &[
            ("audioBlockFormatID", &block.id),
            ("rtime", &opt_time(block.rtime)),
            ("duration", &opt_time(block.duration)),
        ],
    );

    let inner = indent + 2;
    if let Some(cartesian) = block.cartesian {
        leaf(
            out,
            inner,
            "cartesian",
            &[],
            if cartesian { "1" } else { "0" },
        );
    }
    match block.position {
        Some(Position::Cartesian { x, y, z }) => {
            for (axis, value) in [("X", x), ("Y", y), ("Z", z)] {
                leaf(
                    out,
                    inner,
                    "position",
                    &[("coordinate", axis)],
                    &number(value),
                );
            }
        }
        Some(Position::Polar {
            azimuth,
            elevation,
            distance,
        }) => {
            for (axis, value) in [
                ("azimuth", azimuth),
                ("elevation", elevation),
                ("distance", distance),
            ] {
                leaf(
                    out,
                    inner,
                    "position",
                    &[("coordinate", axis)],
                    &number(value),
                );
            }
        }
        None => {}
    }
    if let Some(label) = &block.speaker_label {
        leaf(out, inner, "speakerLabel", &[], label);
    }
    for (name, value) in [
        ("width", block.width),
        ("height", block.height),
        ("depth", block.depth),
        ("diffuse", block.diffuse),
        ("gain", block.gain),
    ] {
        if let Some(value) = value {
            leaf(out, inner, name, &[], &number(value));
        }
    }
    if let Some(importance) = block.importance {
        leaf(out, inner, "importance", &[], &importance.to_string());
    }
    if let Some(jump) = &block.jump_position {
        let flag = if jump.flag { "1" } else { "0" };
        match jump.interpolation_length {
            // A number of seconds, which is what this attribute carries and
            // what implementations expect. Exactness is not lost by the form:
            // 1151/48000 s printed at full precision multiplies back to 1151
            // samples. A timestamp here is read but never written.
            Some(length) => leaf(
                out,
                inner,
                "jumpPosition",
                &[("interpolationLength", &number(length.to_seconds()))],
                flag,
            ),
            None => leaf(out, inner, "jumpPosition", &[], flag),
        }
    }

    close(out, indent, "audioBlockFormat");
}

fn open(out: &mut String, indent: usize, name: &str, attrs: &[(&str, &str)]) {
    pad(out, indent);
    let _ = write!(out, "<{name}");
    push_attrs(out, attrs);
    out.push_str(">\n");
}

fn close(out: &mut String, indent: usize, name: &str) {
    pad(out, indent);
    let _ = writeln!(out, "</{name}>");
}

fn leaf(out: &mut String, indent: usize, name: &str, attrs: &[(&str, &str)], text: &str) {
    pad(out, indent);
    let _ = write!(out, "<{name}");
    push_attrs(out, attrs);
    let _ = write!(out, ">{}</{name}>", escape(text));
    out.push('\n');
}

fn push_attrs(out: &mut String, attrs: &[(&str, &str)]) {
    for (name, value) in attrs {
        // An empty optional attribute is absent, not empty.
        if value.is_empty() {
            continue;
        }
        let _ = write!(out, " {name}=\"{}\"", escape(value));
    }
}

fn pad(out: &mut String, indent: usize) {
    out.extend(std::iter::repeat_n(' ', indent));
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn number(value: f64) -> String {
    // `{}` gives the shortest form that reads back as the same double, which
    // is what a coordinate wants: no invented precision, no lost precision.
    format!("{value}")
}

fn opt_number<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// Times go out in the decimal form. See [`super::time`] for why: it is exact
/// at nanoseconds, and the fractional form is a second-edition feature the
/// most widely deployed ADM implementation rejects outright.
fn opt_time(time: Option<Time>) -> String {
    time.map(|t| t.to_decimal().to_string()).unwrap_or_default()
}

/// Read an interpolation length in either form the model has used.
///
/// The second edition writes a time; the first wrote a bare number of
/// seconds. A reader that only accepts one silently loses every ramp written
/// by the other.
fn parse_interpolation_length(text: &str) -> Option<Time> {
    if let Ok(time) = Time::parse(Path::new("axml"), text) {
        return Some(time);
    }
    text.trim()
        .parse::<f64>()
        .ok()
        .map(|seconds| Time::Decimal {
            nanos: (seconds * 1e9).round() as u64,
        })
}

fn pcm_definition(label: &str) -> &'static str {
    if label.trim() == "0001" { "PCM" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AudioFormatExtended {
        AudioFormatExtended {
            programmes: vec![AudioProgramme {
                id: "APR_1001".into(),
                name: "Programme".into(),
                start: Some(Time::at_sample(0, 48_000)),
                end: Some(Time::at_sample(96_000, 48_000)),
                content_refs: vec!["ACO_1001".into()],
            }],
            contents: vec![AudioContent {
                id: "ACO_1001".into(),
                name: "Content".into(),
                object_refs: vec!["AO_1001".into()],
            }],
            objects: vec![AudioObject {
                id: "AO_1001".into(),
                name: "Object 1".into(),
                start: Some(Time::at_sample(0, 48_000)),
                duration: Some(Time::at_sample(96_000, 48_000)),
                pack_format_refs: vec!["AP_00031001".into()],
                track_uid_refs: vec!["ATU_00000001".into()],
            }],
            pack_formats: vec![AudioPackFormat {
                id: "AP_00031001".into(),
                name: "Object 1".into(),
                type_definition: TypeDefinition::Objects,
                channel_format_refs: vec!["AC_00031001".into()],
            }],
            channel_formats: vec![AudioChannelFormat {
                id: "AC_00031001".into(),
                name: "Object 1".into(),
                type_definition: TypeDefinition::Objects,
                frequency: Frequency::default(),
                blocks: vec![AudioBlockFormat {
                    id: "AB_00031001_00000001".into(),
                    rtime: Some(Time::at_sample(0, 48_000)),
                    duration: Some(Time::at_sample(1152, 48_000)),
                    cartesian: Some(true),
                    position: Some(Position::Cartesian {
                        x: -1.0,
                        y: 1.0,
                        z: 0.0,
                    }),
                    gain: Some(1.0),
                    width: Some(0.0),
                    importance: Some(10),
                    jump_position: Some(JumpPosition {
                        flag: true,
                        interpolation_length: Some(Time::at_sample(1152, 48_000)),
                    }),
                    ..Default::default()
                }],
            }],
            stream_formats: vec![AudioStreamFormat {
                id: "AS_00031001".into(),
                name: "Object 1".into(),
                format_definition: "0001".into(),
                channel_format_ref: Some("AC_00031001".into()),
                pack_format_ref: None,
                track_format_refs: vec!["AT_00031001_01".into()],
            }],
            track_formats: vec![AudioTrackFormat {
                id: "AT_00031001_01".into(),
                name: "Object 1".into(),
                format_definition: "0001".into(),
                stream_format_ref: Some("AS_00031001".into()),
            }],
            track_uids: vec![AudioTrackUid {
                uid: "ATU_00000001".into(),
                sample_rate: Some(48_000),
                bit_depth: Some(24),
                track_format_ref: Some("AT_00031001_01".into()),
                pack_format_ref: Some("AP_00031001".into()),
                channel_format_ref: None,
            }],
        }
    }

    fn reparse(adm: &AudioFormatExtended) -> Parsed {
        parse(Path::new("t.wav"), &to_xml(adm)).unwrap()
    }

    /// The same document with every time in the form the writer emits, so a
    /// comparison tests the content rather than the spelling.
    fn decimal(mut adm: AudioFormatExtended) -> AudioFormatExtended {
        let convert = |time: &mut Option<Time>| {
            if let Some(t) = time {
                *t = t.to_decimal();
            }
        };
        for programme in &mut adm.programmes {
            convert(&mut programme.start);
            convert(&mut programme.end);
        }
        for object in &mut adm.objects {
            convert(&mut object.start);
            convert(&mut object.duration);
        }
        for channel in &mut adm.channel_formats {
            for block in &mut channel.blocks {
                convert(&mut block.rtime);
                convert(&mut block.duration);
                if let Some(jump) = &mut block.jump_position {
                    if let Some(length) = jump.interpolation_length {
                        jump.interpolation_length = Some(Time::Decimal {
                            nanos: length.to_nanos(),
                        });
                    }
                }
            }
        }
        adm
    }

    /// Everything survives; times come back in the decimal form they were
    /// written in, naming the same samples.
    #[test]
    fn a_document_survives_being_written_and_read() {
        let adm = decimal(sample());
        let parsed = reparse(&adm);
        assert_eq!(parsed.adm, adm);
        assert!(parsed.ignored.is_empty(), "{:?}", parsed.ignored);
    }

    /// Sample-exact times are the whole reason the fractional form is written.
    #[test]
    fn block_times_come_back_at_the_same_sample() {
        let parsed = reparse(&sample());
        let block = &parsed.adm.channel_formats[0].blocks[0];
        assert_eq!(block.rtime.unwrap().to_samples(48_000), 0);
        assert_eq!(block.duration.unwrap().to_samples(48_000), 1152);
    }

    /// A ramp length is a number of samples, written as seconds. Full
    /// precision is what keeps 1151/48000 s worth 1151 samples on the way
    /// back.
    #[test]
    fn a_ramp_length_keeps_its_exact_sample_count() {
        let mut adm = sample();
        let length = Time::at_sample(1151, 48_000);
        adm.channel_formats[0].blocks[0].jump_position = Some(JumpPosition {
            flag: true,
            interpolation_length: Some(length),
        });

        let parsed = reparse(&adm);
        let jump = parsed.adm.channel_formats[0].blocks[0]
            .jump_position
            .unwrap();
        assert_eq!(jump.interpolation_length.unwrap().to_samples(48_000), 1151);
    }

    /// The first edition wrote a bare number of seconds there. Still read.
    #[test]
    fn a_first_edition_ramp_length_is_still_read() {
        let xml = "<audioFormatExtended><audioChannelFormat \
             audioChannelFormatID=\"AC_00031001\" audioChannelFormatName=\"o\" \
             typeLabel=\"0003\"><audioBlockFormat audioBlockFormatID=\"AB_1\">\
             <jumpPosition interpolationLength=\"0.024\">1</jumpPosition>\
             </audioBlockFormat></audioChannelFormat></audioFormatExtended>";
        let parsed = parse(Path::new("t.wav"), xml).unwrap();
        let jump = parsed.adm.channel_formats[0].blocks[0]
            .jump_position
            .unwrap();
        assert_eq!(jump.interpolation_length.unwrap().to_samples(48_000), 1152);
    }

    #[test]
    fn polar_positions_survive_too() {
        let mut adm = decimal(sample());
        adm.channel_formats[0].blocks[0].position = Some(Position::Polar {
            azimuth: -30.0,
            elevation: 0.0,
            distance: 1.0,
        });
        adm.channel_formats[0].blocks[0].cartesian = Some(false);
        assert_eq!(reparse(&adm).adm, adm);
    }

    /// Namespace prefixes vary between producers and mean nothing here.
    #[test]
    fn a_prefixed_document_reads_the_same() {
        let xml = "<adm:audioFormatExtended xmlns:adm=\"urn:x\">\
             <adm:audioObject audioObjectID=\"AO_1001\" audioObjectName=\"o\">\
             <adm:audioPackFormatIDRef>AP_00031001</adm:audioPackFormatIDRef>\
             </adm:audioObject></adm:audioFormatExtended>";
        let parsed = parse(Path::new("t.wav"), xml).unwrap();
        assert_eq!(parsed.adm.objects[0].id, "AO_1001");
        assert_eq!(parsed.adm.objects[0].pack_format_refs, ["AP_00031001"]);
    }

    /// The point of a tolerant reader: what it does not model, it counts.
    #[test]
    fn an_unmodelled_element_is_reported_not_dropped_in_silence() {
        let xml = "<audioFormatExtended>\
             <audioObject audioObjectID=\"AO_1001\" audioObjectName=\"o\">\
             <gain>0.5</gain></audioObject>\
             <audioMatrixThing/></audioFormatExtended>";
        let parsed = parse(Path::new("t.wav"), xml).unwrap();
        assert_eq!(parsed.adm.objects.len(), 1);
        assert_eq!(parsed.ignored.elements.get("gain"), Some(&1));
        assert_eq!(parsed.ignored.elements.get("audioMatrixThing"), Some(&1));
    }

    /// An LFE says it is an LFE with a low-pass. Losing it sends a bed
    /// channel full-range somewhere it should not go.
    #[test]
    fn a_band_limit_survives() {
        let mut adm = decimal(sample());
        adm.channel_formats[0].frequency = Frequency {
            low_pass: Some(120.0),
            high_pass: None,
        };
        let parsed = reparse(&adm);
        assert_eq!(parsed.adm, adm);
        assert!(parsed.ignored.is_empty(), "{:?}", parsed.ignored);
    }

    #[test]
    fn a_document_without_the_root_element_is_refused() {
        let err = parse(Path::new("t.wav"), "<somethingElse/>")
            .expect_err("a document with no audioFormatExtended was accepted");
        assert!(err.to_string().contains("audioFormatExtended"), "{err}");
    }

    #[test]
    fn names_with_markup_are_escaped_and_come_back_intact() {
        let mut adm = decimal(sample());
        adm.objects[0].name = "Bed & <Objects>".into();
        assert_eq!(reparse(&adm).adm.objects[0].name, "Bed & <Objects>");
    }

    #[test]
    fn an_empty_optional_attribute_is_absent_rather_than_empty() {
        let mut adm = decimal(sample());
        adm.objects[0].start = None;
        adm.objects[0].duration = None;
        let xml = to_xml(&adm);
        assert!(!xml.contains("start=\"\""), "{xml}");
        assert_eq!(reparse(&adm).adm.objects[0].start, None);
    }
}
