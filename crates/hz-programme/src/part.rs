//! A master's tracks, sorted by what they are.
//!
//! Every writer asks the same question of a master before it decides
//! anything about its own format: which track is the low frequency channel,
//! which are bed channels and where their speakers are, which are objects and
//! where their updates put them. The answer is the same whatever is written,
//! so it is worked out once, here; what a writer then makes of a bed channel
//! — an element pinned at its speaker, a channel of a bed element, an object
//! that never moves — is the writer's.

use crate::source::{Source, keyframes_of, tracks};
use hz_core::{Error, Result, speakers};
use hz_io::adm::model::TypeDefinition;
use hz_render::Keyframe;
use std::path::Path;

/// One track of the master, sorted by what it becomes.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    /// The low frequency channel, which has no position of its own.
    Lfe { channel: usize },
    /// A bed channel other than the LFE: a speaker feed, `name` the master's
    /// own name for its channel and `position` its speaker's place in the
    /// cube.
    Bed {
        channel: usize,
        name: &'static str,
        position: [f64; 3],
    },
    /// An object and its updates.
    Object {
        channel: usize,
        keyframes: Vec<Keyframe>,
    },
}

impl Part {
    /// Where its audio is in an interleaved frame of the master.
    pub fn channel(&self) -> usize {
        match self {
            Self::Lfe { channel } | Self::Bed { channel, .. } | Self::Object { channel, .. } => {
                *channel
            }
        }
    }

    /// The state a writer starts it in: an object where and as loud as its
    /// first update says, a bed channel at its speaker in the bed class —
    /// see `hz_cluster::class::BED` — and the LFE at nothing in particular.
    pub fn initial(&self) -> Keyframe {
        match self {
            Self::Object { keyframes, .. } => keyframes.first().copied().unwrap_or_default(),
            Self::Bed { position, .. } => Keyframe {
                position: *position,
                mode: hz_cluster::class::BED,
                ..Keyframe::default()
            },
            Self::Lfe { .. } => Keyframe::default(),
        }
    }

    /// Where it starts in the cube; `None` for the LFE, which has no place.
    pub fn origin(&self) -> Option<[f64; 3]> {
        match self {
            Self::Lfe { .. } => None,
            other => Some(other.initial().position),
        }
    }
}

/// The master's tracks, the LFE first and the rest in track order — the
/// order every writer numbers a programme's elements in, which an overlay's
/// arithmetic depends on.
///
/// A bed channel with no place in the cube is refused rather than guessed
/// at, and so is a second LFE: no writer here carries two, and dropping one
/// is silence nobody asked for. A track of any other kind is left out, and
/// said so.
pub fn parts_of(path: &Path, source: &Source) -> Result<Vec<Part>> {
    let described = source.adm();
    let mut lfe = None;
    let mut rest = Vec::new();
    for track in tracks(path, described, source.channels())? {
        match track.format.type_definition {
            TypeDefinition::DirectSpeakers => {
                let label = track.speaker_label();
                if speakers::is_lfe_label(label) {
                    if lfe.replace(track.source_channel).is_some() {
                        return Err(Error::unsupported(
                            path,
                            format!(
                                "track {} is a second LFE channel; a programme carries one",
                                track.number
                            ),
                        ));
                    }
                    continue;
                }
                // Either spelling — an ADM label, an alias of one, or a
                // master's channel name — to the master's name for it.
                let name = speakers::master_name_for_label(label)
                    .or_else(|| speakers::by_master_name(label).map(|s| s.master));
                let place = name.and_then(|name| {
                    hz_render::fold::bed_position(name).map(|position| (name, position))
                });
                let Some((name, position)) = place else {
                    return Err(Error::unsupported(
                        path,
                        format!(
                            "track {} is the bed channel `{label}`, which has no place in the \
                             room to put it at",
                            track.number
                        ),
                    ));
                };
                rest.push(Part::Bed {
                    channel: track.source_channel,
                    name,
                    position,
                });
            }
            TypeDefinition::Objects => rest.push(Part::Object {
                channel: track.source_channel,
                keyframes: keyframes_of(track.format, described.sample_rate),
            }),
            other => eprintln!("note: track {} is `{other}` audio; left out", track.number),
        }
    }
    let mut parts = Vec::with_capacity(rest.len() + 1);
    parts.extend(lfe.map(|channel| Part::Lfe { channel }));
    parts.extend(rest);
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bed channel starts where its speaker is, in the bed class; an object
    /// at its first update; the LFE nowhere.
    #[test]
    fn each_part_starts_where_a_writer_states_it() {
        let bed = Part::Bed {
            channel: 1,
            name: "L",
            position: [0.0, 1.0, 0.0],
        };
        assert_eq!(bed.initial().mode, hz_cluster::class::BED);
        assert_eq!(bed.origin(), Some([0.0, 1.0, 0.0]));
        let first = Keyframe {
            position: [0.5, 0.5, 0.0],
            gain: 0.5,
            ..Keyframe::default()
        };
        let object = Part::Object {
            channel: 2,
            keyframes: vec![first],
        };
        assert_eq!(object.initial(), first);
        assert_eq!(object.channel(), 2);
        assert_eq!(Part::Lfe { channel: 0 }.origin(), None);
    }
}
