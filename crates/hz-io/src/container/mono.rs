//! A master set's audio as one mono WAV per waveform.
//!
//! A master set carries its waveforms interleaved, in one CAF, and that is
//! what a delivered master looks like. A master somebody is still *making* —
//! decoded from a stream, re-voiced, re-mixed — is naturally one mono file
//! per waveform, and interleaving them only so that they can be read back
//! apart costs a copy of the whole programme: tens of gigabytes for a film,
//! written once and read once.
//!
//! So this reads them where they are. The files are `<prefix>_<n>.wav`, `n`
//! from 0 in the order of the master's own waveforms — its bed channels, then
//! its objects — which is the order an interleaved file would have held them
//! in and the name `harletty decode --mono-prefix` writes them under. Each
//! must be mono integer PCM, and all of them must share one rate, one width
//! and one length: a master has one of each, and a file that disagrees is a
//! programme that was put together wrong, which is better said than read.

use super::FrameSource;
use super::pcm::PcmFormat;
use super::wav::WavReader;
use hz_core::{Error, Result};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// Waveforms read from their own files, handed out as interleaved frames.
pub struct MonoSet {
    readers: Vec<WavReader<BufReader<File>>>,
    format: PcmFormat,
    frames: u64,
    /// One waveform's worth of a read, before it is interleaved.
    column: Vec<i32>,
}

impl MonoSet {
    /// The file waveform `n` is read from.
    pub fn path(prefix: &Path, n: usize) -> PathBuf {
        let mut name = prefix.as_os_str().to_owned();
        name.push(format!("_{n}.wav"));
        PathBuf::from(name)
    }

    /// Open the `channels` files under `prefix`.
    pub fn open(prefix: &Path, channels: usize) -> Result<Self> {
        if channels == 0 {
            return Err(Error::malformed(prefix, "a master set with no waveform"));
        }
        let mut readers = Vec::with_capacity(channels);
        let mut first: Option<(PathBuf, PcmFormat, u64)> = None;
        for n in 0..channels {
            let path = Self::path(prefix, n);
            let reader = WavReader::open(&path)?;
            let pcm = reader.format().pcm;
            if pcm.channels != 1 {
                return Err(Error::malformed(
                    &path,
                    format!("{} channels where a waveform is one", pcm.channels),
                ));
            }
            if pcm.sample_format != super::SampleFormat::Integer {
                return Err(Error::unsupported(
                    &path,
                    "float samples: a master is integer PCM",
                ));
            }
            match &first {
                None => first = Some((path.clone(), pcm, reader.frames())),
                Some((first_path, first_pcm, first_frames)) => {
                    let disagrees = if pcm.sample_rate != first_pcm.sample_rate {
                        Some(format!("{} Hz", pcm.sample_rate))
                    } else if pcm.bits_per_channel != first_pcm.bits_per_channel {
                        Some(format!("{}-bit", pcm.bits_per_channel))
                    } else if reader.frames() != *first_frames {
                        Some(format!("{} frames", reader.frames()))
                    } else {
                        None
                    };
                    if let Some(what) = disagrees {
                        return Err(Error::malformed(
                            &path,
                            format!(
                                "{what}, where {} has {} Hz, {}-bit, {} frames: one master, one \
                                 rate, one width, one length",
                                first_path.display(),
                                first_pcm.sample_rate,
                                first_pcm.bits_per_channel,
                                first_frames
                            ),
                        ));
                    }
                }
            }
            readers.push(reader);
        }
        // One file too many is a master whose header and audio disagree
        // about how many waveforms it has.
        let extra = Self::path(prefix, channels);
        if extra.exists() {
            return Err(Error::malformed(
                &extra,
                format!("a waveform past the {channels} the master declares"),
            ));
        }
        let (_, pcm, frames) = first.expect("at least one waveform was opened");
        Ok(Self {
            readers,
            format: PcmFormat {
                channels: channels as u32,
                ..pcm
            },
            frames,
            column: Vec::new(),
        })
    }
}

impl FrameSource for MonoSet {
    fn pcm_format(&self) -> &PcmFormat {
        &self.format
    }

    fn frame_count(&self) -> u64 {
        self.frames
    }

    fn read_frames(&mut self, out: &mut [i32]) -> Result<usize> {
        let channels = self.readers.len();
        let wanted = out.len() / channels;
        if wanted == 0 {
            return Ok(0);
        }
        self.column.resize(wanted, 0);
        let mut landed = usize::MAX;
        for (c, reader) in self.readers.iter_mut().enumerate() {
            let got = reader.read_frames(&mut self.column[..wanted])?;
            // Every file is the same length, so every read lands the same
            // count; the smallest is kept all the same, so a file cut short
            // under us ends the programme rather than tearing a frame.
            landed = landed.min(got);
            for (f, sample) in self.column[..got].iter().enumerate() {
                out[f * channels + c] = *sample;
            }
        }
        Ok(landed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::wav::{RiffKind, WavFormat, WavWriter};

    fn write(path: &Path, samples: &[i32], bits: u32) {
        let format = WavFormat::plain(PcmFormat::wav(48_000.0, 1, bits));
        let mut writer = WavWriter::create(path, RiffKind::Riff, format).unwrap();
        writer.write_frames(samples).unwrap();
        writer.finish().unwrap();
    }

    /// Fixtures go under `target/`, never the system temp directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-fixtures/hz-io/mono")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn waveforms_come_back_interleaved_in_their_order() {
        let dir = scratch("order");
        let prefix = dir.join("canal");
        write(&MonoSet::path(&prefix, 0), &[1, 2, 3], 24);
        write(&MonoSet::path(&prefix, 1), &[-4, -5, -6], 24);
        write(&MonoSet::path(&prefix, 2), &[7, 8_388_607, -8_388_608], 24);
        let mut set = MonoSet::open(&prefix, 3).unwrap();
        assert_eq!(set.frame_count(), 3);
        assert_eq!(set.pcm_format().channels, 3);
        assert_eq!(set.pcm_format().bits_per_channel, 24);
        // Two frames at a time, so a read ends mid-programme.
        let mut out = [0i32; 6];
        assert_eq!(set.read_frames(&mut out).unwrap(), 2);
        assert_eq!(out, [1, -4, 7, 2, -5, 8_388_607]);
        assert_eq!(set.read_frames(&mut out).unwrap(), 1);
        assert_eq!(&out[..3], &[3, -6, -8_388_608]);
        assert_eq!(set.read_frames(&mut out).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_waveform_of_another_length_is_refused() {
        let dir = scratch("length");
        let prefix = dir.join("canal");
        write(&MonoSet::path(&prefix, 0), &[1, 2, 3], 24);
        write(&MonoSet::path(&prefix, 1), &[1, 2], 24);
        let Err(e) = MonoSet::open(&prefix, 2) else {
            panic!("two lengths opened as one master");
        };
        assert!(e.to_string().contains("2 frames"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_waveform_of_another_width_is_refused() {
        let dir = scratch("width");
        let prefix = dir.join("canal");
        write(&MonoSet::path(&prefix, 0), &[1, 2], 24);
        write(&MonoSet::path(&prefix, 1), &[1, 2], 16);
        assert!(MonoSet::open(&prefix, 2).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn more_files_than_the_master_declares_is_a_mistake_said_out_loud() {
        let dir = scratch("extra");
        let prefix = dir.join("canal");
        for n in 0..3 {
            write(&MonoSet::path(&prefix, n), &[1, 2], 24);
        }
        assert!(MonoSet::open(&prefix, 2).is_err());
        assert!(MonoSet::open(&prefix, 3).is_ok());
        // And fewer is a missing file.
        assert!(MonoSet::open(&prefix, 4).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
