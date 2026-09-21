//! What `encode` is given, and how it reads it.
//!
//! It takes a master set or a BW64 file and wants the same three things out
//! of either — an ADM description, a sample format, and interleaved frames —
//! before walking the description's tracks. That half lives here, apart from
//! what is built from each track, so the container is decided once.

use hz_core::{Error, Result};
use hz_io::adm::bw64;
use hz_io::adm::chna::Chna;
use hz_io::adm::model::{AudioChannelFormat, AudioFormatExtended, Position};
use hz_io::container::FrameSource;
use hz_io::container::caf::CafReader;
use hz_io::container::pcm::PcmFormat;
use hz_io::container::wav::WavReader;
use hz_io::master::MasterSet;
use hz_io::project;
use hz_render::{Keyframe, Mode};
use std::path::Path;

/// What both kinds of input boil down to: an ADM description and a reader.
pub(crate) struct Described {
    pub(crate) chna: Chna,
    pub(crate) adm: AudioFormatExtended,
    pub(crate) sample_rate: u32,
}

/// An opened input: its description, its sample layout, and its frames.
///
/// Not an enum over the two containers. Everything below the header is the
/// same for both — see [`FrameSource`] — so the container is decided once, in
/// [`Source::open`], and nothing after it asks again.
pub(crate) struct Source {
    reader: Box<dyn FrameSource>,
    described: Described,
    format: PcmFormat,
}

impl Source {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if path.extension().and_then(|e| e.to_str()) == Some("atmos") {
            let set = MasterSet::open(path)?;
            let events = set.read_events(0)?;
            let audio = set.components[0]
                .audio
                .path()
                .ok_or_else(|| Error::MissingComponent {
                    referenced_by: path.to_path_buf(),
                    reference: set.config.presentations[0].audio.clone(),
                })?;
            let reader = CafReader::open(audio)?;
            let format = *reader.format();

            let projected = project::to_adm(
                path,
                &set.config,
                0,
                &events,
                format.channels,
                reader.frames(),
                format.sample_rate_hz(),
            )?;
            for note in &projected.notes {
                eprintln!("note: {note}");
            }

            Ok(Self {
                reader: Box::new(reader),
                described: Described {
                    chna: projected.chna,
                    adm: projected.adm,
                    sample_rate: events.sample_rate.unwrap_or(48_000),
                },
                format,
            })
        } else {
            let reader = WavReader::open(path)?;
            let description = bw64::read(path, &reader)?
                .ok_or_else(|| Error::malformed(path, "no ADM: this is a plain WAV"))?;
            let format = reader.format().pcm;
            Ok(Self {
                reader: Box::new(reader),
                described: Described {
                    chna: description.chna,
                    adm: description.parsed.adm,
                    sample_rate: format.sample_rate_hz(),
                },
                format,
            })
        }
    }

    pub(crate) fn adm(&self) -> &Described {
        &self.described
    }

    pub(crate) fn channels(&self) -> usize {
        self.format.channels as usize
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.format.sample_rate_hz()
    }

    /// Frames the input holds, which is what a percentage needs a denominator
    /// for. Both containers state it in their header, so it is known before a
    /// sample is read rather than discovered at the end.
    pub(crate) fn frames(&self) -> u64 {
        self.reader.frame_count()
    }

    pub(crate) fn read(&mut self, into: &mut [i32]) -> Result<usize> {
        self.reader.read_frames(into)
    }
}

/// One track of a description, resolved to the channel format that describes
/// it.
pub(crate) struct Track<'a> {
    pub(crate) number: u16,
    /// Where its audio is in an interleaved frame.
    pub(crate) source_channel: usize,
    pub(crate) format: &'a AudioChannelFormat,
}

impl Track<'_> {
    /// The speaker this track names, for a `DirectSpeakers` format. Empty
    /// where the format states none, which is a label to refuse rather than to
    /// guess at.
    pub(crate) fn speaker_label(&self) -> &str {
        self.format
            .blocks
            .first()
            .and_then(|b| b.speaker_label.as_deref())
            .unwrap_or_default()
    }
}

/// The tracks of a description, in track order, each resolved to its channel
/// format.
///
/// A track with no `chna` entry, or a silent one, is not here: it carries no
/// audio and there is nothing to describe. A track whose entry names a uid
/// that resolves to nothing is an error rather than a gap — the description is
/// internally inconsistent and any element built from it would be a guess.
pub(crate) fn tracks<'a>(
    path: &Path,
    described: &'a Described,
    channels: usize,
) -> Result<Vec<Track<'a>>> {
    let mut out = Vec::with_capacity(channels);
    for number in 1..=channels as u16 {
        let Some(entry) = described
            .chna
            .ids
            .iter()
            .find(|id| id.track_index == number && !id.is_silent())
        else {
            continue;
        };
        let Some(format) = described.adm.channel_format_for_track(&entry.uid) else {
            return Err(Error::malformed(
                path,
                format!(
                    "track {number} references `{}`, which resolves to nothing",
                    entry.uid
                ),
            ));
        };
        out.push(Track {
            number,
            source_channel: usize::from(number - 1),
            format,
        });
    }
    Ok(out)
}

/// The blocks of one channel format, as the panner's keyframes.
///
/// # `width` is two different quantities
///
/// BS.2076 carries an object's extent as `width`, and what that number *is*
/// depends on the position beside it: for a Cartesian block it is a fraction
/// of the room, 0 to 1, and for a polar block it is an angle in degrees. The
/// panner takes the first of those. A master set's own `size` is also the
/// first of those, and [`hz_io::project`] carries one as the other, which is
/// right for every file this project writes.
///
/// It is not right for a polar ADM file from somewhere else, and there is no
/// conversion here that would be: turning an angular extent into a fraction of
/// a room is a rendering decision BS.2127 makes with three angles and a
/// distance, not with one number. So the width is **clamped** to the panner's
/// domain rather than converted — which bounds a 30° object to the widest
/// spread there is instead of handing the panner a 30 it has no meaning for —
/// and the case is named here rather than silently rendered.
pub(crate) fn keyframes_of(
    channel_format: &hz_io::adm::model::AudioChannelFormat,
    sample_rate: u32,
) -> Vec<Keyframe> {
    let mut out = Vec::with_capacity(channel_format.blocks.len());
    for block in &channel_format.blocks {
        let [x, y, z] = match block.position {
            Some(Position::Cartesian { x, y, z }) => [x, y, z],
            // Sphere onto cube, BS.2127-0 §10.
            Some(Position::Polar {
                azimuth,
                elevation,
                distance,
            }) => hz_core::coords::polar_to_cartesian(azimuth, elevation, distance),
            None => continue,
        };
        out.push(Keyframe {
            sample_pos: block.rtime.map(|t| t.to_samples(sample_rate)).unwrap_or(0),
            position: [x, y, z],
            gain: block.gain.unwrap_or(1.0),
            ramp_samples: block
                .jump_position
                .and_then(|jump| jump.interpolation_length)
                .map(|length| length.to_samples(sample_rate) as u32)
                .unwrap_or(0),
            spread: block.width.unwrap_or(0.0).clamp(0.0, 1.0),
            // The description carries importance as 0 to 10; the fold weighs
            // objects by it as a fraction. Absent is one, not zero: an object
            // nobody ranked is an object that matters.
            importance: block
                .importance
                .map_or(1.0, |rank| f64::from(rank) / 10.0)
                .clamp(0.0, 1.0),
            // How it is to be rendered, in the codes the payload writes. A
            // description from elsewhere carries none of this and every
            // object of it renders alike.
            mode: block.hints.map_or(Mode::default(), |hints| Mode {
                snap: hints.snap,
                zones: hints.zones.code(),
                elevation: hints.elevation,
                screen: hz_io::master::screen_codes(hints.screen_factor, hints.depth_factor),
            }),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hz_io::adm::model::{
        AudioBlockFormat, AudioChannelFormat, Frequency, Position, TypeDefinition,
    };

    fn channel_format(blocks: Vec<AudioBlockFormat>) -> AudioChannelFormat {
        AudioChannelFormat {
            id: "AC_00031001".to_string(),
            name: "one object".to_string(),
            type_definition: TypeDefinition::Objects,
            frequency: Frequency {
                low_pass: None,
                high_pass: None,
            },
            blocks,
        }
    }

    fn block(importance: Option<i32>) -> AudioBlockFormat {
        AudioBlockFormat {
            position: Some(Position::Cartesian {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            }),
            importance,
            ..AudioBlockFormat::default()
        }
    }

    /// A description says which of its objects matter, and the fold weighs
    /// them by it. Dropping it on the floor here was one half of the two paths
    /// folding different scenes — see `hz_cluster::scene`.
    ///
    /// No master seen states anything but the top rank, so this changes no
    /// measured figure. It changes what the encoder would do with content
    /// that does.
    #[test]
    fn a_ranked_object_carries_its_rank() {
        let keyframes = keyframes_of(&channel_format(vec![block(Some(3))]), 48_000);
        assert_eq!(keyframes.len(), 1);
        assert!((keyframes[0].importance - 0.3).abs() < 1e-12);
    }

    /// And an object nobody ranked is an object that matters, not one that
    /// does not: absent has to read as one, or every object seen — none of
    /// which state it — would fold as though it were silent.
    #[test]
    fn an_unranked_object_matters() {
        let keyframes = keyframes_of(&channel_format(vec![block(None)]), 48_000);
        assert_eq!(keyframes[0].importance, 1.0);
    }
}
