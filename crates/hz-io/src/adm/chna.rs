//! The `chna` chunk: which track carries which ADM element.
//!
//! Fixed-width and unforgiving. Each entry is forty bytes — a one-based track
//! index, then three identifiers of exactly twelve, fourteen and eleven
//! characters, then a pad byte. The identifiers are not length-prefixed and
//! not terminated: an implementation that writes a short one and pads with
//! anything other than spaces produces a file whose track references do not
//! match the `axml` beside it, and the only symptom is a renderer that
//! silently drops a track.
//!
//! Specified in ITU-R BS.2088 / EBU Tech 3306.

use hz_core::{Error, Result};
use std::path::Path;

/// Bytes per entry: 2 + 12 + 14 + 11 + 1.
const ENTRY_BYTES: usize = 40;
const UID_LEN: usize = 12;
const TRACK_REF_LEN: usize = 14;
const PACK_REF_LEN: usize = 11;

/// One track's ADM references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioId {
    /// One-based index of the track in the file.
    pub track_index: u16,
    /// `ATU_00000001`.
    pub uid: String,
    /// `AT_00031001_01`.
    pub track_ref: String,
    /// `AP_00031001`.
    pub pack_ref: String,
}

impl AudioId {
    /// Whether this entry marks a track with no ADM description.
    ///
    /// The convention is an all-zero or all-space identifier, and both occur.
    pub fn is_silent(&self) -> bool {
        self.uid.is_empty()
    }
}

/// The `chna` chunk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chna {
    /// Tracks in the file, which is not always the number of entries: a track
    /// with several non-overlapping objects gets one entry each.
    pub track_count: u16,
    pub ids: Vec<AudioId>,
}

impl Chna {
    pub fn parse(path: &Path, body: &[u8]) -> Result<Self> {
        if body.len() < 4 {
            return Err(Error::malformed(path, "short chna chunk"));
        }
        let track_count = u16::from_le_bytes(body[0..2].try_into().unwrap());
        let uid_count = u16::from_le_bytes(body[2..4].try_into().unwrap()) as usize;

        let available = (body.len() - 4) / ENTRY_BYTES;
        if uid_count > available {
            return Err(Error::malformed(
                path,
                format!("chna declares {uid_count} entries but holds {available}"),
            ));
        }

        let mut ids = Vec::with_capacity(uid_count);
        for i in 0..uid_count {
            let at = 4 + i * ENTRY_BYTES;
            let entry = &body[at..at + ENTRY_BYTES];
            ids.push(AudioId {
                track_index: u16::from_le_bytes(entry[0..2].try_into().unwrap()),
                uid: read_id(&entry[2..2 + UID_LEN]),
                track_ref: read_id(&entry[14..14 + TRACK_REF_LEN]),
                pack_ref: read_id(&entry[28..28 + PACK_REF_LEN]),
            });
        }

        Ok(Self { track_count, ids })
    }

    pub fn to_chunk_body(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(4 + self.ids.len() * ENTRY_BYTES);
        body.extend_from_slice(&self.track_count.to_le_bytes());
        body.extend_from_slice(&(self.ids.len() as u16).to_le_bytes());
        for id in &self.ids {
            body.extend_from_slice(&id.track_index.to_le_bytes());
            write_id(&mut body, &id.uid, UID_LEN);
            write_id(&mut body, &id.track_ref, TRACK_REF_LEN);
            write_id(&mut body, &id.pack_ref, PACK_REF_LEN);
            body.push(0);
        }
        body
    }
}

/// Read a fixed-width identifier, dropping the padding it was written with.
///
/// Both spellings of "no identifier" occur — all spaces and all zeros — and
/// both have to come back as nothing rather than as whitespace or as a string
/// of NULs that would then be written into the XML.
fn read_id(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.trim_matches(|c: char| c == '\0' || c == ' ')
        .to_string()
}

/// Write a fixed-width identifier, padded with spaces and never truncated
/// silently past its field.
fn write_id(out: &mut Vec<u8>, value: &str, width: usize) {
    let bytes = value.as_bytes();
    let take = bytes.len().min(width);
    out.extend_from_slice(&bytes[..take]);
    out.extend(std::iter::repeat_n(b' ', width - take));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Chna {
        Chna {
            track_count: 2,
            ids: vec![
                AudioId {
                    track_index: 1,
                    uid: "ATU_00000001".into(),
                    track_ref: "AT_00031001_01".into(),
                    pack_ref: "AP_00031001".into(),
                },
                AudioId {
                    track_index: 2,
                    uid: "ATU_00000002".into(),
                    track_ref: "AT_00031002_01".into(),
                    pack_ref: "AP_00031002".into(),
                },
            ],
        }
    }

    #[test]
    fn an_entry_is_exactly_forty_bytes() {
        let body = sample().to_chunk_body();
        assert_eq!(body.len(), 4 + 2 * ENTRY_BYTES);
    }

    #[test]
    fn round_trips_through_its_own_bytes() {
        let chna = sample();
        let body = chna.to_chunk_body();
        assert_eq!(Chna::parse(Path::new("t.wav"), &body).unwrap(), chna);
    }

    /// The identifiers are fixed-width and unterminated, so the field
    /// boundaries are the only thing separating them.
    #[test]
    fn identifiers_land_on_their_own_field_boundaries() {
        let body = sample().to_chunk_body();
        assert_eq!(&body[6..18], b"ATU_00000001");
        assert_eq!(&body[18..32], b"AT_00031001_01");
        assert_eq!(&body[32..43], b"AP_00031001");
    }

    /// A track with no ADM is written as an empty field, and both the space
    /// and the NUL spelling have to read back as nothing.
    #[test]
    fn both_spellings_of_an_absent_identifier_read_as_nothing() {
        let mut body = sample().to_chunk_body();
        body[6..18].copy_from_slice(&[0u8; 12]);
        body[46..58].copy_from_slice(&[b' '; 12]);

        let chna = Chna::parse(Path::new("t.wav"), &body).unwrap();
        assert!(chna.ids[0].is_silent());
        assert!(chna.ids[1].is_silent());
    }

    #[test]
    fn a_truncated_chunk_is_refused_rather_than_read_past() {
        let mut body = sample().to_chunk_body();
        body.truncate(4 + ENTRY_BYTES);
        let err = Chna::parse(Path::new("t.wav"), &body)
            .expect_err("a chunk shorter than it claims was accepted");
        assert!(err.to_string().contains("declares 2 entries"), "{err}");
    }

    /// More entries than tracks is legal: one track can carry several
    /// non-overlapping objects, each with its own entry.
    #[test]
    fn more_entries_than_tracks_is_allowed() {
        let mut chna = sample();
        chna.track_count = 1;
        let body = chna.to_chunk_body();
        assert_eq!(Chna::parse(Path::new("t.wav"), &body).unwrap(), chna);
    }
}
