// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries values taken from the EBU ADM Renderer —
// Copyright (c) 2018-2019 EBU ADM Renderer Authors — under its BSD 3-Clause
// licence, whose text is reproduced in LICENSES/EBU-ADM-Renderer.txt.
// What was taken and what was changed is recorded in docs/provenance.md.

//! Projecting a master set onto ADM, and back.
//!
//! # The coordinate mapping, derived rather than assumed
//!
//! Getting an axis sign wrong here swaps front and back for every object, and
//! nothing in either file would say so. The mapping is therefore taken from
//! the decoder's own conversion out of object metadata, which is authoritative
//! and permissively licensed:
//!
//! ```text
//! x_master = (x_payload         - 0.5) * 2     // left −1, right +1
//! y_master = (0.5 - y_payload        ) * 2     // back  −1, front +1   (sign flipped)
//! z_master =  z_payload                        // listener plane 0, ceiling +1
//! ```
//!
//! BS.2076 Cartesian coordinates are X left−right, Y back−front, Z
//! bottom−top, all with the listener at the origin. Those are the same three
//! axes with the same senses, so **the projection is the identity** — which is
//! worth stating precisely because it looks like an assumption and is not.
//!
//! # What cannot be carried is said, not swallowed
//!
//! The two formats do not describe the same set of things, and a converter
//! that silently drops the difference is worse than one that refuses. Every
//! function here returns notes naming what it could not carry, and refuses
//! outright when continuing would mean guessing — a channel count that does
//! not match the elements, or a track that two elements share.

use crate::adm::chna::{AudioId, Chna};
use crate::adm::model::*;
use crate::adm::time::Time;
use crate::master::{
    BedInstance, Channel, Config, Event, EventStream, Object, Presentation, PresentationType,
};
use crate::yaml::{Num, Real};
use hz_core::{Error, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// A master set projected onto ADM.
#[derive(Debug, Clone, PartialEq)]
pub struct Projected {
    pub chna: Chna,
    pub adm: AudioFormatExtended,
    /// What could not be carried across, in the order it was found.
    pub notes: Vec<String>,
}

/// An ADM description projected back onto a master set.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovered {
    pub config: Config,
    pub events: EventStream,
    pub notes: Vec<String>,
}

/// One addressable thing in a master set, in channel order.
#[derive(Debug, Clone, PartialEq)]
enum Element {
    /// A bed channel, carrying its master-format name.
    Bed {
        id: u32,
        name: String,
    },
    Object {
        id: u32,
    },
}

impl Element {
    fn type_definition(&self) -> TypeDefinition {
        match self {
            Self::Bed { .. } => TypeDefinition::DirectSpeakers,
            Self::Object { .. } => TypeDefinition::Objects,
        }
    }
}

/// The elements of a presentation, in the order their channels appear: bed
/// channels as listed, then objects as listed.
fn elements(presentation: &Presentation) -> Vec<Element> {
    let mut out = Vec::new();
    for bed in &presentation.bed_instances {
        for channel in &bed.channels {
            out.push(Element::Bed {
                id: channel.id,
                name: channel.channel.clone(),
            });
        }
    }
    for object in &presentation.objects {
        out.push(Element::Object { id: object.id });
    }
    out
}

// ---------------------------------------------------------------------------
// Master set -> ADM
// ---------------------------------------------------------------------------

/// Project one presentation of a master set onto ADM.
///
/// `channels`, `frames` and `sample_rate` come from the audio component, not
/// from the config and not from the metadata: those say what they believe, and
/// the audio says what is there.
///
/// The rate is the one that used to be taken from the event stream's
/// declaration, falling back to 48 kHz. Every time in the result is a sample
/// position divided by it, so a metadata stream that disagrees with its own
/// audio — or does not say — moved every block boundary in the projection. It
/// is now the audio's, and a disagreement is a note rather than a silent
/// reinterpretation.
pub fn to_adm(
    path: &Path,
    config: &Config,
    presentation: usize,
    events: &EventStream,
    channels: u32,
    frames: u64,
    sample_rate: u32,
) -> Result<Projected> {
    let presentation = config.presentations.get(presentation).ok_or_else(|| {
        Error::malformed(path, format!("no presentation {presentation} in this set"))
    })?;
    let elements = elements(presentation);

    // Refusing here is the whole point. Mapping sixteen declared elements onto
    // twenty-one channels means choosing which five to ignore, and any choice
    // would be a guess that silently mislabels every track after it.
    if elements.len() as u32 != channels {
        return Err(Error::malformed(
            path,
            format!(
                "the presentation declares {} elements ({} bed channels, {} objects) \
                 but the audio has {channels} channels; the mapping cannot be guessed",
                elements.len(),
                presentation
                    .bed_instances
                    .iter()
                    .map(|b| b.channels.len())
                    .sum::<usize>(),
                presentation.objects.len(),
            ),
        ));
    }

    let mut notes = Vec::new();
    if let Some(declared) = events.sample_rate
        && declared != sample_rate
    {
        notes.push(format!(
            "the metadata declares {declared} Hz and the audio is {sample_rate} Hz; \
             the audio's rate is the one used"
        ));
    }

    // Fields the master format has and the model does not. Naming them costs
    // two lines and turns a silent loss into a stated one — which is the only
    // difference between a converter and a shredder.
    if config.language.is_some() {
        notes.push("the set's language is not carried: ADM has no field for it".into());
    }
    if presentation.fps.is_some() {
        notes.push("the set's frame rate is not carried: ADM times are absolute".into());
    }
    if presentation.trim_mode.is_some() {
        notes.push("the set's trims are not carried: ADM has no equivalent".into());
    }
    if presentation.warp_mode.is_some() {
        notes.push("the set's warp mode is not carried: ADM has no equivalent".into());
    }

    let mut adm = AudioFormatExtended::default();
    let mut chna = Chna {
        track_count: channels as u16,
        ids: Vec::with_capacity(channels as usize),
    };

    // The events, bucketed by the object they belong to.
    //
    // A real master's event stream is a million entries and its programme is
    // a hundred objects; walking the stream once per object is a hundred
    // million comparisons for a hundred thousand events kept. One pass fills
    // the buckets and every object then reads its own.
    let mut by_object: BTreeMap<u32, Vec<&Event>> = BTreeMap::new();
    for event in &events.events {
        if let Some(id) = event.id {
            by_object.entry(id).or_default().push(event);
        }
    }

    let bed_pack = format!("AP_0001{FIRST_FREE_ID:04X}");
    let mut bed_channel_refs = Vec::new();
    let mut bed_track_uids = Vec::new();
    let mut object_ids = Vec::new();

    for (index, element) in elements.iter().enumerate() {
        let track = index as u16 + 1;
        let kind = element.type_definition();
        let ids = Ids::new(kind, track);

        let blocks = match element {
            Element::Bed { name, .. } => {
                vec![bed_block(&ids, name, frames, sample_rate, &mut notes)]
            }
            Element::Object { id } => object_blocks(
                &ids,
                *id,
                by_object.get(id).map(Vec::as_slice).unwrap_or(&[]),
                frames,
                sample_rate,
                &mut notes,
            ),
        };

        let frequency = match element {
            // An LFE says it is one with a band limit; without it a renderer
            // has only the label to go on.
            Element::Bed { name, .. } if name.starts_with("LFE") => Frequency {
                low_pass: Some(120.0),
                high_pass: None,
            },
            _ => Frequency::default(),
        };

        adm.channel_formats.push(AudioChannelFormat {
            id: ids.channel_format.clone(),
            name: element_name(element),
            type_definition: kind,
            frequency,
            blocks,
        });

        adm.stream_formats.push(AudioStreamFormat {
            id: ids.stream_format.clone(),
            name: element_name(element),
            format_definition: "0001".into(),
            channel_format_ref: Some(ids.channel_format.clone()),
            pack_format_ref: None,
            track_format_refs: vec![ids.track_format.clone()],
        });
        adm.track_formats.push(AudioTrackFormat {
            id: ids.track_format.clone(),
            name: element_name(element),
            format_definition: "0001".into(),
            stream_format_ref: Some(ids.stream_format.clone()),
        });

        let pack = match element {
            Element::Bed { .. } => {
                bed_channel_refs.push(ids.channel_format.clone());
                bed_track_uids.push(ids.track_uid.clone());
                bed_pack.clone()
            }
            Element::Object { .. } => {
                adm.pack_formats.push(AudioPackFormat {
                    id: ids.pack_format.clone(),
                    name: element_name(element),
                    type_definition: kind,
                    channel_format_refs: vec![ids.channel_format.clone()],
                });
                object_ids.push((ids.clone(), element_name(element)));
                ids.pack_format.clone()
            }
        };

        adm.track_uids.push(AudioTrackUid {
            uid: ids.track_uid.clone(),
            sample_rate: Some(sample_rate),
            bit_depth: None,
            track_format_ref: Some(ids.track_format.clone()),
            pack_format_ref: Some(pack.clone()),
            channel_format_ref: None,
        });
        chna.ids.push(AudioId {
            track_index: track,
            uid: ids.track_uid.clone(),
            track_ref: ids.track_format.clone(),
            pack_ref: pack,
        });
    }

    let mut object_refs = Vec::new();

    if !bed_channel_refs.is_empty() {
        adm.pack_formats.insert(
            0,
            AudioPackFormat {
                id: bed_pack.clone(),
                name: "Bed".into(),
                type_definition: TypeDefinition::DirectSpeakers,
                channel_format_refs: bed_channel_refs,
            },
        );
        adm.objects.push(AudioObject {
            id: format!("AO_{FIRST_FREE_ID:04X}"),
            name: "Bed".into(),
            start: Some(Time::at_sample(0, sample_rate)),
            duration: Some(Time::at_sample(frames, sample_rate)),
            pack_format_refs: vec![bed_pack],
            track_uid_refs: bed_track_uids,
        });
        object_refs.push(format!("AO_{FIRST_FREE_ID:04X}"));
    }

    for (ids, name) in object_ids {
        adm.objects.push(AudioObject {
            id: ids.object.clone(),
            name,
            start: Some(Time::at_sample(0, sample_rate)),
            duration: Some(Time::at_sample(frames, sample_rate)),
            pack_format_refs: vec![ids.pack_format],
            track_uid_refs: vec![ids.track_uid],
        });
        object_refs.push(ids.object);
    }

    adm.contents.push(AudioContent {
        id: "ACO_1001".into(),
        name: "Main".into(),
        object_refs,
    });
    adm.programmes.push(AudioProgramme {
        id: "APR_1001".into(),
        name: presentation
            .creation_tool
            .clone()
            .unwrap_or_else(|| "Programme".into()),
        start: Some(Time::at_sample(0, sample_rate)),
        end: Some(Time::at_sample(frames, sample_rate)),
        content_refs: vec!["ACO_1001".into()],
    });

    Ok(Projected { chna, adm, notes })
}

/// The identifiers one element gets across the model.
#[derive(Debug, Clone)]
struct Ids {
    channel_format: String,
    pack_format: String,
    stream_format: String,
    track_format: String,
    track_uid: String,
    object: String,
}

/// Identifiers below this are reserved for the model's common definitions.
///
/// Minting `AC_00010001` for the first bed channel is not merely untidy: that
/// identifier already means "front left" to every implementation that loads
/// the common definitions, so a file that redefines it is asking a renderer to
/// choose between two answers. The EBU's renderer says so out loud, which is
/// how this was found.
const FIRST_FREE_ID: u16 = 0x1000;

impl Ids {
    fn new(kind: TypeDefinition, track: u16) -> Self {
        let type_label = kind.label();
        let index = FIRST_FREE_ID + track;
        Self {
            channel_format: format!("AC_{type_label}{index:04X}"),
            pack_format: format!("AP_{type_label}{index:04X}"),
            stream_format: format!("AS_{type_label}{index:04X}"),
            track_format: format!("AT_{type_label}{index:04X}_01"),
            track_uid: format!("ATU_{track:08X}"),
            object: format!("AO_{index:04X}"),
        }
    }
}

fn element_name(element: &Element) -> String {
    match element {
        Element::Bed { name, .. } => name.clone(),
        Element::Object { id } => format!("Object {id}"),
    }
}

/// A bed channel is one block that lasts the whole programme.
fn bed_block(
    ids: &Ids,
    name: &str,
    frames: u64,
    sample_rate: u32,
    notes: &mut Vec<String>,
) -> AudioBlockFormat {
    let (label, position) = match speakers::by_master_name(name) {
        Some(speaker) => (
            speaker.label.to_string(),
            Some(Position::Polar {
                azimuth: speaker.azimuth,
                elevation: speaker.elevation,
                distance: 1.0,
            }),
        ),
        None => {
            // Emitting the master's own name keeps the file readable and
            // honest; a renderer that does not know it will say so. Inventing
            // a position to go with it would not be honest.
            notes.push(format!(
                "bed channel `{name}` has no common ADM speaker label; wrote it unchanged, \
                 with no nominal position"
            ));
            (name.to_string(), None)
        }
    };

    AudioBlockFormat {
        id: format!("{}_00000001", ids.channel_format.replacen("AC_", "AB_", 1)),
        rtime: Some(Time::at_sample(0, sample_rate)),
        duration: Some(Time::at_sample(frames, sample_rate)),
        speaker_label: Some(label),
        position,
        ..Default::default()
    }
}

/// An object's blocks, one per event, each lasting until the next.
fn object_blocks(
    ids: &Ids,
    id: u32,
    mine: &[&Event],
    frames: u64,
    sample_rate: u32,
    notes: &mut Vec<String>,
) -> Vec<AudioBlockFormat> {
    if mine.is_empty() {
        notes.push(format!("object {id} has no events; wrote a silent block"));
    }

    // The event stream is a delta stream: an entry states what changed and the
    // rest stays in force. Blocks are absolute, so the state has to be carried
    // forward or every unstated field would reset to nothing.
    let mut position = None;
    // What the stream last *said* the gain was, kept apart from what the block
    // carries: an object the master switched off is silent whatever it last
    // said, and is not to have forgotten its level when it comes back.
    let mut stated = None;
    let mut active = true;
    let mut width = None;
    let mut importance = None;
    let mut hints = RenderHints::default();
    let mut blocks = Vec::with_capacity(mine.len().max(1));

    for (index, event) in mine.iter().enumerate() {
        let start = event.sample_pos.unwrap_or(0);
        let end = mine
            .get(index + 1)
            .and_then(|next| next.sample_pos)
            .unwrap_or(frames)
            .max(start);

        if let Some(pos) = &event.pos {
            position = position_of(pos);
        }
        if let Some(value) = event.gain {
            stated = Some(decibels_to_linear(value.0));
        }
        if let Some(value) = event.active {
            active = value;
        }
        // **An object the master switched off is silent.** The description
        // has no way to say "off" — ADM states a gain and a position and
        // nothing else — so the one honest projection of it is a gain of
        // nought, which is what every reader of this already treats as an
        // element carrying nothing: the fold weighs it at no energy, an
        // overlay will not route a source through it, and the metadata written
        // back out states silence rather than a level nobody asked for.
        //
        // Read and then dropped, this field was: the parser has always
        // understood it and the projection always wrote `true`, so a
        // programme that switched an object off had it encoded at whatever
        // level it last held.
        let gain = if active { stated } else { Some(0.0) };
        if let Some(value) = event.size {
            width = Some(value.0);
        }
        if let Some(value) = event.importance {
            // The master carries importance as a fraction; ADM as 0..10.
            importance = Some((value.0 * 10.0).round() as i32);
        }
        // How the object is to be rendered, in the master's own words,
        // carried forward like everything else it states.
        if let Some(value) = event.snap {
            hints.snap = value;
        }
        if let Some(value) = event.elevation {
            hints.elevation = value;
        }
        if let Some(value) = event.zones {
            hints.zones = value;
        }
        if let Some(value) = event.screen_factor {
            hints.screen_factor = value.0;
        }
        if let Some(value) = event.depth_factor {
            hints.depth_factor = value.0;
        }

        blocks.push(AudioBlockFormat {
            id: format!(
                "{}_{:08X}",
                ids.channel_format.replacen("AC_", "AB_", 1),
                index + 1
            ),
            rtime: Some(Time::at_sample(start, sample_rate)),
            duration: Some(Time::at_sample(end - start, sample_rate)),
            cartesian: Some(true),
            position,
            gain,
            width,
            importance,
            jump_position: Some(JumpPosition {
                flag: true,
                interpolation_length: event
                    .ramp_length
                    .filter(|&length| length > 0)
                    .map(|length| Time::at_sample(length as u64, sample_rate)),
            }),
            hints: Some(hints),
            ..Default::default()
        });
    }

    if blocks.is_empty() {
        blocks.push(AudioBlockFormat {
            id: format!("{}_00000001", ids.channel_format.replacen("AC_", "AB_", 1)),
            rtime: Some(Time::at_sample(0, sample_rate)),
            duration: Some(Time::at_sample(frames, sample_rate)),
            cartesian: Some(true),
            gain: Some(0.0),
            ..Default::default()
        });
    }
    blocks
}

fn position_of(pos: &[Num]) -> Option<Position> {
    match pos {
        [x, y, z] => Some(Position::Cartesian {
            x: x.0,
            y: y.0,
            z: z.0,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ADM -> master set
// ---------------------------------------------------------------------------

/// Project an ADM description back onto a master set.
pub fn to_master(
    path: &Path,
    chna: &Chna,
    adm: &AudioFormatExtended,
    sample_rate: u32,
    channels: u32,
) -> Result<Recovered> {
    let mut notes = Vec::new();
    let mut bed = Vec::new();
    let mut objects = Vec::new();
    let mut events = Vec::new();
    let mut next_object_id = 10u32;

    for track in 1..=channels as u16 {
        let entries: Vec<&AudioId> = chna
            .ids
            .iter()
            .filter(|id| id.track_index == track && !id.is_silent())
            .collect();

        // One channel per element is what a master set can express. A track
        // shared by two non-overlapping elements is legal ADM and has no
        // representation here, so it is refused rather than half-converted.
        match entries.len() {
            0 => {
                notes.push(format!("track {track} has no ADM entry; left out"));
                continue;
            }
            1 => {}
            n => {
                return Err(Error::malformed(
                    path,
                    format!(
                        "track {track} carries {n} elements; a master set has one channel \
                         per element and cannot represent that"
                    ),
                ));
            }
        }

        let uid = &entries[0].uid;
        let Some(channel_format) = adm.channel_format_for_track(uid) else {
            return Err(Error::malformed(
                path,
                format!("track {track} references `{uid}`, which resolves to no channel format"),
            ));
        };

        match channel_format.type_definition {
            TypeDefinition::DirectSpeakers => {
                let label = channel_format
                    .blocks
                    .first()
                    .and_then(|b| b.speaker_label.as_deref())
                    .unwrap_or_default();
                let name = match speakers::master_name_for_label(label) {
                    Some(name) => name.to_string(),
                    None => {
                        notes.push(format!(
                            "speaker label `{label}` on track {track} is not a bed channel this \
                             format names; kept it unchanged"
                        ));
                        label.to_string()
                    }
                };
                let id = speakers::by_master_name(&name)
                    .map(|s| s.id)
                    .unwrap_or(bed.len() as u32);
                events.push(Event {
                    id: Some(id),
                    sample_pos: Some(0),
                    active: Some(true),
                    gain: Some(Num(0.0)),
                    ..Event::default()
                });
                bed.push(Channel { channel: name, id });
            }
            TypeDefinition::Objects => {
                let id = next_object_id;
                next_object_id += 1;
                objects.push(Object {
                    description: None,
                    group_name: None,
                    id,
                });
                events.extend(object_events(id, channel_format, sample_rate, &mut notes));
            }
            other => {
                return Err(Error::unsupported(
                    path,
                    format!(
                        "track {track} is `{other}` audio, which a master set has no place for"
                    ),
                ));
            }
        }
    }

    // Events are addressed by sample position across all elements, so a stream
    // built element by element has to be put back in time order.
    events.sort_by_key(|e| (e.sample_pos.unwrap_or(0), e.id.unwrap_or(0)));

    let config = Config {
        language: None,
        version: crate::master::DAMF_VERSION.to_string(),
        presentations: vec![Presentation {
            presentation_type: PresentationType::Home,
            simplified: false,
            metadata: String::new(),
            audio: String::new(),
            offset: Real(0.0),
            ffoa: None,
            fps: None,
            sc_number_of_elements: None,
            sc_bed_configuration: (!bed.is_empty())
                .then(|| bed.iter().map(|c| c.id).collect::<Vec<_>>()),
            creation_tool: Some("harlettizer".into()),
            creation_tool_version: Some(env!("CARGO_PKG_VERSION").into()),
            source_codec: None,
            downmix_type_5to2: None,
            ls_rs_90_deg_phase_shift: None,
            warp_mode: None,
            trim_mode: None,
            bed_instances: if bed.is_empty() {
                Vec::new()
            } else {
                vec![BedInstance {
                    description: None,
                    group_name: None,
                    channels: bed,
                }]
            },
            objects,
        }],
    };

    Ok(Recovered {
        config,
        events: EventStream {
            sample_rate: Some(sample_rate),
            events,
        },
        notes,
    })
}

fn object_events(
    id: u32,
    channel_format: &AudioChannelFormat,
    sample_rate: u32,
    notes: &mut Vec<String>,
) -> Vec<Event> {
    let mut out = Vec::with_capacity(channel_format.blocks.len());
    let mut polar_reported = false;

    for block in &channel_format.blocks {
        let position = match block.position {
            Some(Position::Cartesian { x, y, z }) => Some(vec![Num(x), Num(y), Num(z)]),
            Some(Position::Polar {
                azimuth,
                elevation,
                distance,
            }) => {
                // Sphere onto cube, BS.2127-0 §10. Not the spherical
                // conversion it looks like — the two coordinate systems
                // describe rooms of different shapes — which is why it is a
                // module of its own and not a formula here.
                if !polar_reported {
                    notes.push(format!(
                        "object on `{}` was positioned in polar coordinates and \
                         converted onto the cube (BS.2127-0 §10)",
                        channel_format.id
                    ));
                    polar_reported = true;
                }
                let [x, y, z] = hz_core::coords::polar_to_cartesian(azimuth, elevation, distance);
                Some(vec![Num(x), Num(y), Num(z)])
            }
            None => None,
        };

        out.push(Event {
            id: Some(id),
            sample_pos: Some(block.rtime.map(|t| t.to_samples(sample_rate)).unwrap_or(0)),
            active: Some(true),
            pos: position,
            size: block.width.map(Real),
            importance: block.importance.map(|i| Real(i as f64 / 10.0)),
            gain: block.gain.map(|g| Num(linear_to_decibels(g))),
            ramp_length: block
                .jump_position
                .and_then(|jump| jump.interpolation_length)
                .map(|length| length.to_samples(sample_rate) as u32),
            ..Event::default()
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Tables and units
// ---------------------------------------------------------------------------

// The speaker table lives in `hz-core`, because the panner and the meter need
// the same answers and two copies is two chances for a channel to end up
// somewhere different depending on which path reached it.
use hz_core::speakers;

/// The master format carries gain in decibels, with silence as `-inf`; ADM
/// carries a linear factor by default.
fn decibels_to_linear(db: f64) -> f64 {
    if db.is_infinite() && db.is_sign_negative() {
        0.0
    } else {
        10f64.powf(db / 20.0)
    }
}

fn linear_to_decibels(linear: f64) -> f64 {
    if linear <= 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * linear.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master::{Config, PresentationType};

    fn path() -> &'static Path {
        Path::new("t.atmos")
    }

    fn master() -> (Config, EventStream) {
        let config = Config::parse(
            "version: 0.5.1\n\
             presentations:\n\
             \x20 - type: home\n\
             \x20   simplified: false\n\
             \x20   metadata: t.atmos.metadata\n\
             \x20   audio: t.atmos.audio\n\
             \x20   offset: 0.0\n\
             \x20   bedInstances:\n\
             \x20     - channels:\n\
             \x20         - channel: L\n\
             \x20           ID: 0\n\
             \x20         - channel: LFE\n\
             \x20           ID: 3\n\
             \x20   objects:\n\
             \x20     - ID: 10\n",
        )
        .unwrap();

        let events = EventStream::parse(
            "sampleRate: 48000\n\
             events:\n\
             \x20 - ID: 10\n\
             \x20   samplePos: 0\n\
             \x20   active: true\n\
             \x20   pos: [-1, 1, 0]\n\
             \x20   size: 0.0\n\
             \x20   importance: 1.0\n\
             \x20   gain: 0\n\
             \x20   rampLength: 1152\n\
             \x20 - ID: 10\n\
             \x20   samplePos: 4800\n\
             \x20   pos: [1, -1, 0.5]\n\
             \x20   gain: -inf\n\
             \x20   rampLength: 0\n",
        )
        .unwrap();

        (config, events)
    }

    #[test]
    fn a_presentation_becomes_a_complete_adm_chain() {
        let (config, events) = master();
        let projected = to_adm(path(), &config, 0, &events, 3, 48_000, 48_000).unwrap();

        assert_eq!(projected.chna.track_count, 3);
        assert_eq!(projected.chna.ids.len(), 3);
        assert_eq!(projected.adm.channel_formats.len(), 3);
        // One shared pack for the bed, one per object.
        assert_eq!(projected.adm.pack_formats.len(), 2);
        // One bed object plus one per object element.
        assert_eq!(projected.adm.objects.len(), 2);

        // Every track resolves, which is the property a renderer depends on.
        for id in &projected.chna.ids {
            assert!(
                projected.adm.channel_format_for_track(&id.uid).is_some(),
                "{} does not resolve",
                id.uid
            );
        }
        assert!(projected.notes.is_empty(), "{:?}", projected.notes);
    }

    /// Sixteen elements onto twenty-one channels means choosing five to
    /// ignore. There is no right choice, so there is no choice.
    /// A conversion that loses something says so. These fields have nowhere
    /// to go in the model, and silence about that is the failure mode this
    /// module is arranged against.
    #[test]
    fn fields_the_model_cannot_hold_are_named() {
        let (mut config, events) = master();
        config.language = Some("eng".into());
        config.presentations[0].fps = Some(crate::master::Fps::R24);

        let projected = to_adm(path(), &config, 0, &events, 3, 48_000, 48_000).unwrap();
        assert!(projected.notes.iter().any(|n| n.contains("language")));
        assert!(projected.notes.iter().any(|n| n.contains("frame rate")));
    }

    #[test]
    fn a_channel_count_that_does_not_match_is_refused() {
        let (config, events) = master();
        let err = to_adm(path(), &config, 0, &events, 21, 48_000, 48_000)
            .expect_err("a mismatched channel count was accepted");
        assert!(err.to_string().contains("cannot be guessed"), "{err}");
    }

    /// A DirectSpeakers block needs a position as well as a label, and the
    /// two have to agree — a renderer may resolve either.
    #[test]
    fn an_lfe_keeps_its_band_limit_its_label_and_its_position() {
        let (config, events) = master();
        let projected = to_adm(path(), &config, 0, &events, 3, 48_000, 48_000).unwrap();
        let lfe = &projected.adm.channel_formats[1];
        assert_eq!(lfe.frequency.low_pass, Some(120.0));
        assert_eq!(lfe.blocks[0].speaker_label.as_deref(), Some("LFE1"));
        assert_eq!(
            lfe.blocks[0].position,
            Some(Position::Polar {
                azimuth: 0.0,
                elevation: -30.0,
                distance: 1.0
            })
        );

        let left = &projected.adm.channel_formats[0];
        assert_eq!(left.blocks[0].speaker_label.as_deref(), Some("M+030"));
        assert_eq!(
            left.blocks[0].position,
            Some(Position::Polar {
                azimuth: 30.0,
                elevation: 0.0,
                distance: 1.0
            })
        );
    }

    /// A delta stream states what changed; blocks are absolute. A projection
    /// that forgets that resets every unstated field to nothing.
    #[test]
    fn state_carries_forward_across_events() {
        let (config, events) = master();
        let projected = to_adm(path(), &config, 0, &events, 3, 48_000, 48_000).unwrap();
        let blocks = &projected.adm.channel_formats[2].blocks;
        assert_eq!(blocks.len(), 2);

        // The second event states no size or importance; the block still has
        // the values that were in force.
        assert_eq!(blocks[1].width, Some(0.0));
        assert_eq!(blocks[1].importance, Some(10));
        assert_eq!(
            blocks[1].position,
            Some(Position::Cartesian {
                x: 1.0,
                y: -1.0,
                z: 0.5
            })
        );
    }

    #[test]
    fn blocks_last_until_the_next_event() {
        let (config, events) = master();
        let projected = to_adm(path(), &config, 0, &events, 3, 96_000, 48_000).unwrap();
        let blocks = &projected.adm.channel_formats[2].blocks;
        assert_eq!(blocks[0].rtime.unwrap().to_samples(48_000), 0);
        assert_eq!(blocks[0].duration.unwrap().to_samples(48_000), 4800);
        assert_eq!(blocks[1].rtime.unwrap().to_samples(48_000), 4800);
        assert_eq!(
            blocks[1].duration.unwrap().to_samples(48_000),
            96_000 - 4800
        );
    }

    /// The times in a projection are sample positions divided by a rate, and
    /// the rate is the **audio's**. It used to be the metadata stream's, with
    /// 48 kHz assumed when it did not say — so a metadata stream that
    /// disagreed with its own audio moved every block boundary in the result,
    /// and said nothing about it.
    #[test]
    fn the_audios_rate_wins_and_a_disagreement_is_said_out_loud() {
        let (config, mut events) = master();
        events.sample_rate = Some(96_000);
        let projected = to_adm(path(), &config, 0, &events, 3, 96_000, 48_000).unwrap();

        assert!(
            projected
                .notes
                .iter()
                .any(|note| note.contains("96000") && note.contains("48000")),
            "the disagreement should be a note: {:?}",
            projected.notes
        );
        // And the boundaries are where the audio's rate puts them, not the
        // metadata's.
        let blocks = &projected.adm.channel_formats[2].blocks;
        assert_eq!(blocks[0].duration.unwrap().to_samples(48_000), 4800);
    }

    /// **An object the master switched off is silent.** `active` was read by
    /// the parser and dropped here, and the projection wrote `true`
    /// unconditionally — so a programme that switched an object off had it
    /// encoded at whatever level it last held, for as long as it was off.
    ///
    /// And it comes back at the level it had, not at nothing: what `active`
    /// suppresses is the gain the block carries, not the gain the stream
    /// stated.
    #[test]
    fn an_object_switched_off_is_projected_silent_and_comes_back() {
        let set = concat!(
            "version: 0.5.1\n",
            "presentations:\n",
            "  - type: home\n",
            "    simplified: false\n",
            "    metadata: x.atmos.metadata\n",
            "    audio: x.atmos.audio\n",
            "    offset: 0.0\n",
            "    objects:\n",
            "      - ID: 10\n",
        );
        let events = concat!(
            "events:\n",
            "  - ID: 10\n",
            "    samplePos: 0\n",
            "    active: true\n",
            "    pos: [0, 1, 0]\n",
            "    gain: -6\n",
            "  - ID: 10\n",
            "    samplePos: 480\n",
            "    active: false\n",
            "  - ID: 10\n",
            "    samplePos: 960\n",
            "    active: true\n",
        );
        let config = Config::parse(set).expect("a config");
        let events = EventStream::parse(events).expect("an event stream");
        let projected = to_adm(Path::new("x.atmos"), &config, 0, &events, 1, 1440, 48_000)
            .expect("a projection");

        let blocks: Vec<f64> = projected
            .adm
            .channel_formats
            .iter()
            .flat_map(|format| format.blocks.iter())
            .map(|block| block.gain.unwrap_or(1.0))
            .collect();
        assert_eq!(blocks.len(), 3, "three events, three blocks");
        assert!(
            (blocks[0] - 0.501_187).abs() < 1e-5,
            "the stated −6 dB did not survive: {}",
            blocks[0]
        );
        assert_eq!(
            blocks[1], 0.0,
            "the object was switched off and is not silent"
        );
        assert!(
            (blocks[2] - 0.501_187).abs() < 1e-5,
            "it came back at {} rather than at the level it had",
            blocks[2]
        );
    }

    #[test]
    fn silence_survives_the_trip_through_a_linear_gain() {
        assert_eq!(decibels_to_linear(f64::NEG_INFINITY), 0.0);
        assert!(linear_to_decibels(0.0).is_infinite());
        assert!((decibels_to_linear(0.0) - 1.0).abs() < 1e-12);
        assert!((linear_to_decibels(decibels_to_linear(-6.0)) + 6.0).abs() < 1e-9);
    }

    #[test]
    fn a_projection_comes_back_with_the_same_positions_and_times() {
        let (config, events) = master();
        let projected = to_adm(path(), &config, 0, &events, 3, 96_000, 48_000).unwrap();
        let recovered = to_master(path(), &projected.chna, &projected.adm, 48_000, 3).unwrap();

        let object: Vec<&Event> = recovered
            .events
            .events
            .iter()
            .filter(|e| e.id == Some(10))
            .collect();
        assert_eq!(object.len(), 2);
        assert_eq!(object[0].sample_pos, Some(0));
        assert_eq!(object[0].pos, Some(vec![Num(-1.0), Num(1.0), Num(0.0)]));
        assert_eq!(object[0].ramp_length, Some(1152));
        assert_eq!(object[1].sample_pos, Some(4800));
        assert_eq!(object[1].pos, Some(vec![Num(1.0), Num(-1.0), Num(0.5)]));
        assert!(object[1].gain.unwrap().0.is_infinite());

        // And the bed came back as bed channels, with their own names.
        let bed = &recovered.config.presentations[0].bed_instances[0].channels;
        assert_eq!(bed[0].channel, "L");
        assert_eq!(bed[1].channel, "LFE");
        assert_eq!(bed[1].id, 3);
        assert_eq!(recovered.config.presentations[0].objects.len(), 1);
    }

    /// A track two elements share is legal ADM with no master-set equivalent.
    #[test]
    fn a_shared_track_is_refused_rather_than_half_converted() {
        let (config, events) = master();
        let mut projected = to_adm(path(), &config, 0, &events, 3, 48_000, 48_000).unwrap();
        projected.chna.ids[1].track_index = 1;

        let err = to_master(path(), &projected.chna, &projected.adm, 48_000, 3)
            .expect_err("a shared track was accepted");
        assert!(err.to_string().contains("carries 2 elements"), "{err}");
    }

    /// A polar position is converted onto the cube rather than dropped, and
    /// the conversion is the standard's, not a spherical shortcut: the front
    /// corners of the cube are at ∓30°, so azimuth 30 lands exactly on one.
    #[test]
    fn polar_positions_are_converted_onto_the_cube() {
        let (config, events) = master();
        let mut projected = to_adm(path(), &config, 0, &events, 3, 48_000, 48_000).unwrap();
        projected.adm.channel_formats[2].blocks[0].position = Some(Position::Polar {
            azimuth: 30.0,
            elevation: 0.0,
            distance: 1.0,
        });

        let recovered = to_master(path(), &projected.chna, &projected.adm, 48_000, 3).unwrap();
        assert!(
            recovered.notes.iter().any(|n| n.contains("BS.2127")),
            "{:?}",
            recovered.notes
        );

        let object = recovered
            .events
            .events
            .iter()
            .find(|e| e.id == Some(10))
            .unwrap();
        let position = object.pos.as_ref().expect("position was dropped");
        assert!((position[0].0 + 1.0).abs() < 1e-9, "x {}", position[0].0);
        assert!((position[1].0 - 1.0).abs() < 1e-9, "y {}", position[1].0);
        assert!(position[2].0.abs() < 1e-9, "z {}", position[2].0);
    }

    #[test]
    fn an_unknown_bed_name_is_kept_and_reported() {
        let config = Config::parse(
            "version: 0.5.1\n\
             presentations:\n\
             \x20 - type: home\n\
             \x20   simplified: false\n\
             \x20   metadata: t\n\
             \x20   audio: t\n\
             \x20   offset: 0.0\n\
             \x20   bedInstances:\n\
             \x20     - channels:\n\
             \x20         - channel: Xyz\n\
             \x20           ID: 0\n\
             \x20   objects: []\n",
        )
        .unwrap();
        let events = EventStream::parse("sampleRate: 48000\nevents: []\n").unwrap();

        let projected = to_adm(path(), &config, 0, &events, 1, 48_000, 48_000).unwrap();
        assert_eq!(
            projected.adm.channel_formats[0].blocks[0]
                .speaker_label
                .as_deref(),
            Some("Xyz")
        );
        assert!(projected.notes.iter().any(|n| n.contains("Xyz")));
        assert_eq!(
            config.presentations[0].presentation_type,
            PresentationType::Home
        );
    }
}
