//! The programme's loudness and its information: Evolution frame payloads 1
//! and 2 (TS 102 366 H.3.1 and H.3.2), which every reference E-AC-3 stream
//! carries beside its object metadata.
//!
//! The loudness payload states, when an encoder has them, what was measured:
//! the programme's integrated loudness, gated on level or on speech, its
//! short-term loudness, the true peak since the last time one was stated.
//! Its fields up to the byte boundary are modelled; what follows the
//! boundary — the high-resolution and extended-range fields, which an encoder
//! leaves out when it has none — is kept as the payload's bytes, so a payload
//! reads and writes back unchanged. The programme information states what
//! was done to the audio before coding and the profile its compression words
//! were computed with; it is modelled whole.
//!
//! Both are held byte for byte to every payload of the reference streams to
//! hand (the tests, and `cargo xtask eac3`).

use hz_core::bits::BitWriter;
use hz_core::bits_read::BitReader;

/// A payload this does not read: one cut short, or of a version it does not
/// know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable(pub &'static str);

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "programme payload: {}", self.0)
    }
}

impl std::error::Error for Unreadable {}

type Result<T> = std::result::Result<T, Unreadable>;

fn short<T>(_: T) -> Unreadable {
    Unreadable("cut short")
}

/// Payload 1, the programme's loudness (TS 102 366 H.3.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Loudness {
    /// Nought for every payload this Annex describes; three and up carry an
    /// extended version, which is added to it.
    pub version: u8,
    /// `dialchan`: the channels dialogue was measured on, when stated.
    pub dialogue_channels: Option<u8>,
    /// `loudpractyp`: the recommended practice measured to, nought when not
    /// indicated.
    pub practice: u8,
    /// With a practice stated: whether the programme's loudness was corrected
    /// to the dialogue-gated measure, and how (`loudcorrdialgat`,
    /// `loudcorrtyp`).
    pub correction: Option<(bool, bool)>,
    /// `loudrelgat`: integrated loudness, gated on level — see [`lufs`].
    pub relative_gated: Option<u8>,
    /// `loudspchgat`: integrated loudness, gated on speech — see [`lufs`].
    pub speech_gated: Option<u8>,
    /// `loudstrm3s`: short-term (3 s) loudness.
    pub short_term: Option<u8>,
    /// `truepk`: the true peak since the last one stated — see
    /// [`true_peak_code`].
    pub true_peak: Option<u8>,
    /// `dmixloudoffst`.
    pub downmix_offset: Option<u8>,
    pub boundary: Option<Boundary>,
    /// Everything past the byte boundary, as the payload carries it: the
    /// high-resolution and extended fields, absent when an encoder has none.
    pub extension: Vec<u8>,
}

/// A programme boundary (`prgmbndy`): one is this many frames away, a power
/// of two, at its end or its start, and so many samples into its frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    /// The power of two: `prgmbndy` is `1 << frames_log2`, at least one.
    pub frames_log2: u8,
    pub end: bool,
    pub offset: Option<u16>,
}

/// Integrated loudness codes to LUFS: −58 to +5.5 in half steps.
pub fn lufs(code: u8) -> f64 {
    -58.0 + f64::from(code) / 2.0
}

/// A true peak in dBTP to its code: −116 to +11.5 in half steps, silence the
/// lowest.
pub fn true_peak_code(db: f64) -> u8 {
    if db.is_nan() {
        return 0;
    }
    ((db + 116.0) * 2.0).round().clamp(0.0, 255.0) as u8
}

/// A true peak code to dBTP.
pub fn true_peak_db(code: u8) -> f64 {
    -116.0 + f64::from(code) / 2.0
}

fn read_version(r: &mut BitReader<'_>) -> Result<u8> {
    let mut version = r.get(2).map_err(short)? as u8;
    if version == 3 {
        version += r.get(4).map_err(short)? as u8;
    }
    Ok(version)
}

fn write_version(w: &mut BitWriter, version: u8) {
    if version >= 3 {
        w.put(2, 3);
        w.put(4, u32::from(version - 3));
    } else {
        w.put(2, u32::from(version));
    }
}

fn read_option(r: &mut BitReader<'_>, bits: u32) -> Result<Option<u32>> {
    Ok(if r.get_bit().map_err(short)? {
        Some(r.get(bits).map_err(short)?)
    } else {
        None
    })
}

fn write_option(w: &mut BitWriter, bits: u32, value: Option<u32>) {
    w.put_bit(value.is_some());
    if let Some(value) = value {
        w.put(bits, value);
    }
}

impl Loudness {
    pub fn read(bytes: &[u8]) -> Result<Self> {
        let r = &mut BitReader::new(bytes);
        let version = read_version(r)?;
        let dialogue_channels = read_option(r, 3)?.map(|v| v as u8);
        let practice = r.get(4).map_err(short)? as u8;
        let correction = if practice != 0 {
            Some((r.get_bit().map_err(short)?, r.get_bit().map_err(short)?))
        } else {
            None
        };
        let relative_gated = read_option(r, 7)?.map(|v| v as u8);
        let speech_gated = read_option(r, 7)?.map(|v| v as u8);
        let short_term = read_option(r, 8)?.map(|v| v as u8);
        let true_peak = read_option(r, 8)?.map(|v| v as u8);
        let downmix_offset = read_option(r, 5)?.map(|v| v as u8);
        let boundary = if r.get_bit().map_err(short)? {
            let mut frames_log2 = 1u8;
            while !r.get_bit().map_err(short)? {
                frames_log2 += 1;
                if frames_log2 > 31 {
                    return Err(Unreadable("a programme boundary past 2^31 frames"));
                }
            }
            let end = r.get_bit().map_err(short)?;
            let offset = read_option(r, 11)?.map(|v| v as u16);
            Some(Boundary {
                frames_log2,
                end,
                offset,
            })
        } else {
            None
        };
        // The fill to the byte boundary is nought; the rest is kept.
        let fill = (8 - r.bit_position() % 8) % 8;
        if fill > 0 && r.get(fill as u32).map_err(short)? != 0 {
            return Err(Unreadable("fill bits that are not nought"));
        }
        let extension = bytes[r.byte_position()..].to_vec();
        Ok(Self {
            version,
            dialogue_channels,
            practice,
            correction,
            relative_gated,
            speech_gated,
            short_term,
            true_peak,
            downmix_offset,
            boundary,
            extension,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let w = &mut BitWriter::new();
        write_version(w, self.version);
        write_option(w, 3, self.dialogue_channels.map(u32::from));
        w.put(4, u32::from(self.practice));
        if self.practice != 0 {
            let (dialogue_gated, kind) = self.correction.unwrap_or_default();
            w.put_bit(dialogue_gated);
            w.put_bit(kind);
        }
        write_option(w, 7, self.relative_gated.map(u32::from));
        write_option(w, 7, self.speech_gated.map(u32::from));
        write_option(w, 8, self.short_term.map(u32::from));
        write_option(w, 8, self.true_peak.map(u32::from));
        write_option(w, 5, self.downmix_offset.map(u32::from));
        w.put_bit(self.boundary.is_some());
        if let Some(boundary) = self.boundary {
            for _ in 1..boundary.frames_log2 {
                w.put_bit(false);
            }
            w.put_bit(true);
            w.put_bit(boundary.end);
            write_option(w, 11, boundary.offset.map(u32::from));
        }
        w.align_to(8);
        w.put_bytes(&self.extension);
        w.bytes().to_vec()
    }
}

/// Payload 2, the programme's information (TS 102 366 H.3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Information {
    pub version: u8,
    /// `activechan`: which channel locations carry audio.
    pub active_channels: Option<u16>,
    /// `dmixtyp`: how the programme was downmixed before coding, when it was.
    pub downmix: Option<u8>,
    /// `upmixtyp`: how it was upmixed, when it was and was not downmixed.
    pub upmix: Option<u8>,
    pub preprocessing: Option<Preprocessing>,
    /// `dynrngprof`, `comprprof`: the profiles the compression words were
    /// computed with, nought for none.
    pub compression_profiles: Option<(u8, u8)>,
    /// `specprocstartf`, `specprocendf`.
    pub spectral_processing: Option<(u8, u8)>,
    /// `chancplstartf`, `chancplendf`.
    pub channel_coupling: Option<(u8, u8)>,
    /// `enhncrng`.
    pub enhanced_range: Option<u8>,
}

/// What was done to the audio before coding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Preprocessing {
    /// The surrounds attenuated by 3 dB.
    pub surround_attenuation: bool,
    /// A 90° phase shift on Ls and Rs.
    pub phase_shift: bool,
    /// A low-pass filter on the LFE.
    pub lfe_filter: bool,
    /// The level the LFE was monitored at: nought, not indicated.
    pub lfe_monitor_level: u8,
}

impl Information {
    pub fn read(bytes: &[u8]) -> Result<Self> {
        let r = &mut BitReader::new(bytes);
        let version = read_version(r)?;
        let active_channels = read_option(r, 16)?.map(|v| v as u16);
        let (downmix, upmix) = if r.get_bit().map_err(short)? {
            (Some(r.get(4).map_err(short)? as u8), None)
        } else {
            (None, read_option(r, 4)?.map(|v| v as u8))
        };
        let preprocessing = if r.get_bit().map_err(short)? {
            Some(Preprocessing {
                surround_attenuation: r.get_bit().map_err(short)?,
                phase_shift: r.get_bit().map_err(short)?,
                lfe_filter: r.get_bit().map_err(short)?,
                lfe_monitor_level: r.get(2).map_err(short)? as u8,
            })
        } else {
            None
        };
        let pair = |r: &mut BitReader<'_>, bits| -> Result<Option<(u8, u8)>> {
            Ok(if r.get_bit().map_err(short)? {
                Some((
                    r.get(bits).map_err(short)? as u8,
                    r.get(bits).map_err(short)? as u8,
                ))
            } else {
                None
            })
        };
        let compression_profiles = pair(r, 3)?;
        let spectral_processing = pair(r, 3)?;
        let channel_coupling = pair(r, 5)?;
        let enhanced_range = read_option(r, 2)?.map(|v| v as u8);
        let fill = (8 - r.bit_position() % 8) % 8;
        if fill > 0 && r.get(fill as u32).map_err(short)? != 0 {
            return Err(Unreadable("fill bits that are not nought"));
        }
        if r.byte_position() != bytes.len() {
            return Err(Unreadable("bytes past the programme information"));
        }
        Ok(Self {
            version,
            active_channels,
            downmix,
            upmix,
            preprocessing,
            compression_profiles,
            spectral_processing,
            channel_coupling,
            enhanced_range,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let w = &mut BitWriter::new();
        write_version(w, self.version);
        write_option(w, 16, self.active_channels.map(u32::from));
        w.put_bit(self.downmix.is_some());
        match self.downmix {
            Some(kind) => w.put(4, u32::from(kind)),
            None => write_option(w, 4, self.upmix.map(u32::from)),
        }
        w.put_bit(self.preprocessing.is_some());
        if let Some(p) = self.preprocessing {
            w.put_bit(p.surround_attenuation);
            w.put_bit(p.phase_shift);
            w.put_bit(p.lfe_filter);
            w.put(2, u32::from(p.lfe_monitor_level));
        }
        let pair = |w: &mut BitWriter, bits, value: Option<(u8, u8)>| {
            w.put_bit(value.is_some());
            if let Some((a, b)) = value {
                w.put(bits, u32::from(a));
                w.put(bits, u32::from(b));
            }
        };
        pair(w, 3, self.compression_profiles);
        pair(w, 3, self.spectral_processing);
        pair(w, 5, self.channel_coupling);
        write_option(w, 2, self.enhanced_range.map(u32::from));
        w.align_to(8);
        w.bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Every distinct loudness payload of the four reference streams reads
    /// and writes back byte for byte.
    #[test]
    fn every_reference_loudness_payload_writes_back() {
        for hex in [
            "0200", "020ed8", "023080", "026100", "020f58", "0230c0", "026900", "0300", "030f40",
            "032f80", "035f00", "1c80", "239000",
        ] {
            let payload = bytes(hex);
            let loudness = Loudness::read(&payload).unwrap_or_else(|e| panic!("{hex}: {e}"));
            assert_eq!(loudness.to_bytes(), payload, "{hex}: {loudness:?}");
        }
    }

    /// What the references state, read as the Annex has it: A/85, then now
    /// and again an integrated loudness, gated on level and on speech, of
    /// −25 LUFS, and a true peak.
    #[test]
    fn the_references_loudness_reads_as_stated() {
        let plain = Loudness::read(&bytes("0200")).unwrap();
        assert_eq!(
            (plain.practice, plain.correction),
            (1, Some((false, false)))
        );
        assert_eq!(plain.relative_gated, None);
        let level = Loudness::read(&bytes("026100")).unwrap();
        assert_eq!(level.relative_gated.map(lufs), Some(-25.0));
        let speech = Loudness::read(&bytes("023080")).unwrap();
        assert_eq!(speech.speech_gated.map(lufs), Some(-25.0));
        let peak = Loudness::read(&bytes("020f58")).unwrap();
        assert_eq!(peak.true_peak, Some(235));
        assert_eq!(true_peak_db(235), 1.5);
        assert_eq!(true_peak_code(1.5), 235);
        assert_eq!(true_peak_code(f64::NEG_INFINITY), 0);
    }

    /// Every distinct programme information payload of the reference streams
    /// reads and writes back byte for byte, and reads as stated: the
    /// preprocessing and the two compression profiles, Film Light.
    #[test]
    fn every_reference_information_payload_writes_back() {
        for hex in ["049240", "049480", "059480"] {
            let payload = bytes(hex);
            let information = Information::read(&payload).unwrap_or_else(|e| panic!("{hex}: {e}"));
            assert_eq!(information.to_bytes(), payload, "{hex}: {information:?}");
        }
        let film = Information::read(&bytes("059480")).unwrap();
        assert_eq!(film.compression_profiles, Some((2, 2)));
        let p = film.preprocessing.unwrap();
        assert!(p.phase_shift && p.lfe_filter && !p.surround_attenuation);
    }

    /// A programme boundary reads and writes back, its power of two unary.
    #[test]
    fn a_boundary_writes_back() {
        let loudness = Loudness {
            practice: 2,
            correction: Some((true, false)),
            relative_gated: Some(70),
            boundary: Some(Boundary {
                frames_log2: 4,
                end: true,
                offset: Some(1234),
            }),
            extension: vec![0x80],
            ..Loudness::default()
        };
        assert_eq!(Loudness::read(&loudness.to_bytes()).unwrap(), loudness);
    }
}
