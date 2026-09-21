//! Getting the ADM in and out of a BW64 file.
//!
//! The container carries `chna` and `axml` as ordinary chunks, so this is
//! mostly plumbing — but plumbing with one rule worth stating: a file that has
//! one and not the other is broken, and saying so here is better than letting
//! a renderer discover it as tracks that go silent.

use super::chna::Chna;
use super::model::AudioFormatExtended;
use super::xml::{self, Parsed};
use crate::container::Chunk;
use crate::container::wav::WavReader;
use hz_core::{Error, Result};
use std::io::{Read, Seek};
use std::path::Path;

const CHUNK_CHNA: [u8; 4] = *b"chna";
const CHUNK_AXML: [u8; 4] = *b"axml";

/// The ADM description carried by a BW64 file.
#[derive(Debug, Clone, PartialEq)]
pub struct Description {
    pub chna: Chna,
    pub parsed: Parsed,
}

impl Description {
    pub fn adm(&self) -> &AudioFormatExtended {
        &self.parsed.adm
    }
}

/// Read the ADM out of an open container, if it has one.
///
/// Returns `None` for a file with neither chunk — a plain WAV is not an
/// error — and fails for a file with only one of them, because that is a
/// description that cannot be acted on.
pub fn read<R: Read + Seek>(path: &Path, reader: &WavReader<R>) -> Result<Option<Description>> {
    let chna = reader.chunk(&CHUNK_CHNA);
    let axml = reader.chunk(&CHUNK_AXML);

    match (chna, axml) {
        (None, None) => Ok(None),
        (Some(chna), Some(axml)) => {
            let chna = Chna::parse(path, &chna.body)?;
            let text = std::str::from_utf8(&axml.body)
                .map_err(|e| Error::malformed(path, format!("axml is not UTF-8: {e}")))?;
            let parsed = xml::parse(path, text)?;
            Ok(Some(Description { chna, parsed }))
        }
        (Some(_), None) => Err(Error::malformed(
            path,
            "chna maps tracks to identifiers, but there is no axml to define them",
        )),
        (None, Some(_)) => Err(Error::malformed(
            path,
            "axml describes the audio, but there is no chna to say which track is which",
        )),
    }
}

/// Build the two chunks a BW64 writer needs, in the order they are written.
pub fn chunks(chna: &Chna, adm: &AudioFormatExtended) -> [Chunk; 2] {
    [
        Chunk {
            kind: CHUNK_CHNA,
            body: chna.to_chunk_body(),
        },
        Chunk {
            kind: CHUNK_AXML,
            body: xml::to_xml(adm).into_bytes(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adm::AudioPackFormat;
    use crate::adm::chna::AudioId;
    use crate::adm::model::{AudioObject, TypeDefinition};
    use crate::container::pcm::PcmFormat;
    use crate::container::wav::{RiffKind, Slot, WavFormat, WavWriter};
    use std::io::Cursor;

    fn description() -> (Chna, AudioFormatExtended) {
        let chna = Chna {
            track_count: 1,
            ids: vec![AudioId {
                track_index: 1,
                uid: "ATU_00000001".into(),
                track_ref: "AT_00031001_01".into(),
                pack_ref: "AP_00031001".into(),
            }],
        };
        let adm = AudioFormatExtended {
            objects: vec![AudioObject {
                id: "AO_1001".into(),
                name: "Object 1".into(),
                pack_format_refs: vec!["AP_00031001".into()],
                track_uid_refs: vec!["ATU_00000001".into()],
                ..Default::default()
            }],
            pack_formats: vec![AudioPackFormat {
                id: "AP_00031001".into(),
                name: "Object 1".into(),
                type_definition: TypeDefinition::Objects,
                channel_format_refs: vec!["AC_00031001".into()],
            }],
            ..Default::default()
        };
        (chna, adm)
    }

    fn write(chunks: &[Chunk]) -> Vec<u8> {
        let format = WavFormat::plain(PcmFormat::wav(48000.0, 1, 24));
        let mut layout = vec![Slot::Ds64, Slot::Fmt];
        layout.extend(chunks.iter().cloned().map(Slot::Other));
        layout.push(Slot::Data);

        let mut buffer = Cursor::new(Vec::new());
        let mut writer = WavWriter::with_layout(
            &mut buffer,
            Path::new("t.wav"),
            RiffKind::Bw64,
            format,
            &layout,
        )
        .unwrap();
        writer.write_frames(&[1, 2, 3, 4]).unwrap();
        writer.finish().unwrap();
        buffer.into_inner()
    }

    fn read_back(bytes: Vec<u8>) -> Result<Option<Description>> {
        let length = bytes.len() as u64;
        let reader = WavReader::new(Cursor::new(bytes), Path::new("t.wav"), length)?;
        read(Path::new("t.wav"), &reader)
    }

    #[test]
    fn an_adm_description_survives_a_bw64_file() {
        let (chna, adm) = description();
        let bytes = write(&chunks(&chna, &adm));
        let read = read_back(bytes).unwrap().unwrap();
        assert_eq!(read.chna, chna);
        assert_eq!(read.adm(), &adm);
    }

    #[test]
    fn a_plain_wav_is_not_an_error() {
        assert_eq!(read_back(write(&[])).unwrap(), None);
    }

    /// Half a description is worse than none: the renderer's symptom would be
    /// tracks that go quiet, which points nowhere near the cause.
    #[test]
    fn half_a_description_is_refused() {
        let (chna, adm) = description();
        let both = chunks(&chna, &adm);

        let err = read_back(write(&both[..1])).expect_err("chna without axml was accepted");
        assert!(err.to_string().contains("no axml"), "{err}");

        let err = read_back(write(&both[1..])).expect_err("axml without chna was accepted");
        assert!(err.to_string().contains("no chna"), "{err}");
    }
}
