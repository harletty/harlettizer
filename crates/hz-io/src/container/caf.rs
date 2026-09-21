//! The Core Audio Format container, which is where a master set keeps its
//! audio.
//!
//! Only linear PCM is handled. A master's audio component is uncompressed by
//! definition, and a CAF carrying a compressed payload is not something an
//! encoder should quietly accept and half-understand — it is reported as
//! unsupported instead.
//!
//! Reads and writes go through a reusable byte buffer, so a programme-length
//! component costs one allocation rather than one per block. That matters
//! here: the audio component of a two-hour master is measured in gigabytes.

use super::Chunk;
use super::pcm::{self, Endianness, PcmFormat, SampleFormat};
use hz_core::{Error, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const FILE_TYPE: &[u8; 4] = b"caff";
const CHUNK_DESC: &[u8; 4] = b"desc";
const CHUNK_DATA: &[u8; 4] = b"data";
const FORMAT_LPCM: &[u8; 4] = b"lpcm";

/// Largest chunk carried through verbatim.
///
/// Every metadata chunk the format defines — channel layout, strings, markers,
/// overview — is small. Something larger is not metadata, and quietly dropping
/// it on the way out would be the exact data loss this crate exists to avoid,
/// so it is refused instead.
const MAX_PRESERVED_CHUNK: u64 = 1 << 20;

const FLAG_IS_FLOAT: u32 = 1 << 0;
const FLAG_IS_LITTLE_ENDIAN: u32 = 1 << 1;

/// The `desc` chunk's flag word for a given layout.
fn desc_flags(format: &PcmFormat) -> u32 {
    let mut flags = 0;
    if format.sample_format == SampleFormat::Float {
        flags |= FLAG_IS_FLOAT;
    }
    if format.endianness == Endianness::Little {
        flags |= FLAG_IS_LITTLE_ENDIAN;
    }
    flags
}

/// Interpret a `desc` chunk.
fn format_from_desc(path: &Path, desc: &[u8]) -> Result<PcmFormat> {
    if desc.len() < 32 {
        return Err(Error::malformed(path, "short desc chunk"));
    }
    let format_id = &desc[8..12];
    if format_id != FORMAT_LPCM {
        return Err(Error::unsupported(
            path,
            format!(
                "`{}` audio; only linear PCM is handled",
                String::from_utf8_lossy(format_id)
            ),
        ));
    }

    let sample_rate = f64::from_be_bytes(desc[0..8].try_into().unwrap());
    let flags = u32::from_be_bytes(desc[12..16].try_into().unwrap());
    let bytes_per_packet = u32::from_be_bytes(desc[16..20].try_into().unwrap());
    let frames_per_packet = u32::from_be_bytes(desc[20..24].try_into().unwrap());
    let channels = u32::from_be_bytes(desc[24..28].try_into().unwrap());
    let bits_per_channel = u32::from_be_bytes(desc[28..32].try_into().unwrap());

    if frames_per_packet != 1 {
        return Err(Error::unsupported(
            path,
            format!("{frames_per_packet} frames per packet; PCM is one"),
        ));
    }
    if channels == 0 {
        return Err(Error::malformed(path, "no channels"));
    }

    let format = PcmFormat {
        sample_rate,
        channels,
        bits_per_channel,
        sample_format: if flags & FLAG_IS_FLOAT != 0 {
            SampleFormat::Float
        } else {
            SampleFormat::Integer
        },
        endianness: if flags & FLAG_IS_LITTLE_ENDIAN != 0 {
            Endianness::Little
        } else {
            Endianness::Big
        },
    };

    // Packed PCM only. A packet larger than the samples it holds is padding
    // this reader would silently mis-stride over.
    if bytes_per_packet as usize != format.bytes_per_frame() {
        return Err(Error::unsupported(
            path,
            format!(
                "{bytes_per_packet} bytes per packet for {channels} channels of {bits_per_channel} bits"
            ),
        ));
    }

    format.check_supported(path)?;
    Ok(format)
}

/// Reads the sample data out of a CAF file.
pub struct CafReader<R: Read + Seek> {
    inner: R,
    path: PathBuf,
    format: PcmFormat,
    frames: u64,
    frames_read: u64,
    /// Reused across calls so a long read does not allocate per block.
    staging: Vec<u8>,
    extra: Vec<Chunk>,
}

impl CafReader<BufReader<File>> {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        let length = file.metadata().map_err(|e| Error::io(path, e))?.len();
        Self::new(BufReader::new(file), path, length)
    }
}

impl<R: Read + Seek> CafReader<R> {
    pub fn new(mut inner: R, path: &Path, file_length: u64) -> Result<Self> {
        let mut header = [0u8; 8];
        read_exact(&mut inner, path, &mut header)?;
        if &header[0..4] != FILE_TYPE {
            return Err(Error::malformed(path, "not a CAF file"));
        }

        let mut format = None;
        let mut data = None;
        let mut extra = Vec::new();
        let mut offset = 8u64;

        while offset + 12 <= file_length {
            let mut chunk_header = [0u8; 12];
            inner
                .seek(SeekFrom::Start(offset))
                .map_err(|e| Error::io(path, e))?;
            read_exact(&mut inner, path, &mut chunk_header)?;

            let kind: [u8; 4] = chunk_header[0..4].try_into().unwrap();
            let declared = i64::from_be_bytes(chunk_header[4..12].try_into().unwrap());
            let body = offset + 12;
            // A size of -1 means "to the end of the file", which is how a
            // streaming writer leaves the data chunk it never came back to.
            let size = if declared < 0 {
                file_length.saturating_sub(body)
            } else {
                declared as u64
            };

            match &kind {
                CHUNK_DESC => {
                    let mut desc = [0u8; 32];
                    read_exact(&mut inner, path, &mut desc)?;
                    format = Some(format_from_desc(path, &desc)?);
                }
                CHUNK_DATA => data = Some((body, size)),
                _ => {
                    if size > MAX_PRESERVED_CHUNK {
                        return Err(Error::unsupported(
                            path,
                            format!(
                                "`{}` chunk of {size} bytes, too large to carry through",
                                String::from_utf8_lossy(&kind)
                            ),
                        ));
                    }
                    let mut body_bytes = vec![0u8; size as usize];
                    read_exact(&mut inner, path, &mut body_bytes)?;
                    extra.push(Chunk {
                        kind,
                        body: body_bytes,
                    });
                }
            }

            offset = body + size;
        }

        let format = format.ok_or_else(|| Error::malformed(path, "no desc chunk"))?;
        let (data_start, data_size) =
            data.ok_or_else(|| Error::malformed(path, "no data chunk"))?;

        // The data chunk opens with an edit count before the samples.
        let samples_start = data_start + 4;
        let sample_bytes = data_size.saturating_sub(4);
        let frames = sample_bytes / format.bytes_per_frame() as u64;

        inner
            .seek(SeekFrom::Start(samples_start))
            .map_err(|e| Error::io(path, e))?;

        Ok(Self {
            inner,
            path: path.to_path_buf(),
            format,
            frames,
            frames_read: 0,
            staging: Vec::new(),
            extra,
        })
    }

    pub fn format(&self) -> &PcmFormat {
        &self.format
    }

    /// Total frames in the file.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Chunks this reader did not interpret, in file order. Pass them to a
    /// writer to keep them.
    pub fn extra_chunks(&self) -> &[Chunk] {
        &self.extra
    }

    /// Fill `out` with interleaved samples, returning how many frames landed.
    ///
    /// Samples are sign-extended into `i32` at their native width — a 24-bit
    /// file yields 24-bit values, not values scaled to 32 bits. Scaling here
    /// would lose the exactness the lossless track depends on.
    pub fn read_frames(&mut self, out: &mut [i32]) -> Result<usize> {
        let channels = self.format.channels as usize;
        let wanted = (out.len() / channels).min((self.frames - self.frames_read) as usize);
        if wanted == 0 {
            return Ok(0);
        }

        let bytes = wanted * self.format.bytes_per_frame();
        if self.staging.len() < bytes {
            self.staging.resize(bytes, 0);
        }
        read_exact(&mut self.inner, &self.path, &mut self.staging[..bytes])?;

        pcm::decode(
            &self.staging[..bytes],
            &self.format,
            &mut out[..wanted * channels],
        );
        self.frames_read += wanted as u64;
        Ok(wanted)
    }
}

impl<R: Read + Seek + Send> super::FrameSource for CafReader<R> {
    fn pcm_format(&self) -> &PcmFormat {
        &self.format
    }

    fn frame_count(&self) -> u64 {
        self.frames
    }

    fn read_frames(&mut self, out: &mut [i32]) -> Result<usize> {
        CafReader::read_frames(self, out)
    }
}

/// Writes a CAF file, patching the data size once the length is known.
pub struct CafWriter<W: Write + Seek> {
    inner: W,
    path: PathBuf,
    format: PcmFormat,
    frames: u64,
    staging: Vec<u8>,
    /// Where the data chunk's size field sits, which moves with any preserved
    /// chunks written ahead of it.
    data_size_offset: u64,
}

impl CafWriter<BufWriter<File>> {
    pub fn create(path: &Path, format: PcmFormat) -> Result<Self> {
        Self::create_with_chunks(path, format, &[])
    }

    /// Create a file that also carries `extra` — typically what a
    /// [`CafReader::extra_chunks`] handed back.
    pub fn create_with_chunks(path: &Path, format: PcmFormat, extra: &[Chunk]) -> Result<Self> {
        let file = File::create(path).map_err(|e| Error::io(path, e))?;
        Self::with_chunks(BufWriter::new(file), path, format, extra)
    }
}

impl<W: Write + Seek> CafWriter<W> {
    pub fn new(inner: W, path: &Path, format: PcmFormat) -> Result<Self> {
        Self::with_chunks(inner, path, format, &[])
    }

    pub fn with_chunks(
        mut inner: W,
        path: &Path,
        format: PcmFormat,
        extra: &[Chunk],
    ) -> Result<Self> {
        format.check_supported(path)?;

        let mut header = Vec::with_capacity(68);
        header.extend_from_slice(FILE_TYPE);
        header.extend_from_slice(&1u16.to_be_bytes()); // version
        header.extend_from_slice(&0u16.to_be_bytes()); // flags

        header.extend_from_slice(CHUNK_DESC);
        header.extend_from_slice(&32i64.to_be_bytes());
        header.extend_from_slice(&format.sample_rate.to_be_bytes());
        header.extend_from_slice(FORMAT_LPCM);
        header.extend_from_slice(&desc_flags(&format).to_be_bytes());
        header.extend_from_slice(&(format.bytes_per_frame() as u32).to_be_bytes());
        header.extend_from_slice(&1u32.to_be_bytes()); // frames per packet
        header.extend_from_slice(&format.channels.to_be_bytes());
        header.extend_from_slice(&format.bits_per_channel.to_be_bytes());

        for chunk in extra {
            header.extend_from_slice(&chunk.kind);
            header.extend_from_slice(&(chunk.body.len() as i64).to_be_bytes());
            header.extend_from_slice(&chunk.body);
        }

        header.extend_from_slice(CHUNK_DATA);
        header.extend_from_slice(&0i64.to_be_bytes()); // patched by `finish`
        header.extend_from_slice(&0u32.to_be_bytes()); // edit count

        inner.write_all(&header).map_err(|e| Error::io(path, e))?;

        Ok(Self {
            inner,
            path: path.to_path_buf(),
            format,
            frames: 0,
            staging: Vec::new(),
            data_size_offset: (header.len() - 12) as u64,
        })
    }

    /// Append interleaved samples. The slice must hold whole frames.
    pub fn write_frames(&mut self, samples: &[i32]) -> Result<()> {
        let channels = self.format.channels as usize;
        if !samples.len().is_multiple_of(channels) {
            return Err(Error::malformed(
                &self.path,
                format!(
                    "{} samples is not whole frames of {channels}",
                    samples.len()
                ),
            ));
        }

        let bytes = samples.len() * self.format.bytes_per_sample();
        if self.staging.len() < bytes {
            self.staging.resize(bytes, 0);
        }
        pcm::encode(samples, &self.format, &mut self.staging[..bytes]);
        self.inner
            .write_all(&self.staging[..bytes])
            .map_err(|e| Error::io(&self.path, e))?;

        self.frames += (samples.len() / channels) as u64;
        Ok(())
    }

    /// Patch the data chunk size and flush.
    pub fn finish(mut self) -> Result<()> {
        let data_size = 4 + self.frames * self.format.bytes_per_frame() as u64;
        self.inner
            .seek(SeekFrom::Start(self.data_size_offset))
            .map_err(|e| Error::io(&self.path, e))?;
        self.inner
            .write_all(&(data_size as i64).to_be_bytes())
            .map_err(|e| Error::io(&self.path, e))?;
        self.inner.flush().map_err(|e| Error::io(&self.path, e))?;
        Ok(())
    }
}

fn read_exact<R: Read>(reader: &mut R, path: &Path, buf: &mut [u8]) -> Result<()> {
    reader.read_exact(buf).map_err(|e| Error::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn round_trip(format: PcmFormat, samples: &[i32]) -> Vec<i32> {
        let mut buffer = Cursor::new(Vec::new());
        let mut writer = CafWriter::new(&mut buffer, Path::new("t.caf"), format).unwrap();
        writer.write_frames(samples).unwrap();
        writer.finish().unwrap();

        let bytes = buffer.into_inner();
        let length = bytes.len() as u64;
        let mut reader = CafReader::new(Cursor::new(bytes), Path::new("t.caf"), length).unwrap();
        assert_eq!(reader.format(), &format);

        let mut out = vec![0i32; samples.len()];
        let frames = reader.read_frames(&mut out).unwrap();
        assert_eq!(frames, samples.len() / format.channels as usize);
        out
    }

    #[test]
    fn a_master_component_round_trips_exactly() {
        let format = PcmFormat::master(48000.0, 3);
        // Full-scale 24-bit extremes, which is where a sign-extension bug shows.
        let samples = [0, 1, -1, 8_388_607, -8_388_608, 1234, -1234, 0, 42];
        assert_eq!(round_trip(format, &samples), samples);
    }

    #[test]
    fn sixteen_and_thirty_two_bit_integers_round_trip() {
        for bits in [16, 32] {
            let format = PcmFormat {
                bits_per_channel: bits,
                ..PcmFormat::master(48000.0, 2)
            };
            let samples = [0, 1, -1, 32767, -32768];
            let padded = [
                samples[0], samples[1], samples[2], samples[3], samples[4], 0,
            ];
            assert_eq!(round_trip(format, &padded), padded, "{bits}-bit");
        }
    }

    #[test]
    fn little_endian_is_not_big_endian() {
        let format = PcmFormat {
            endianness: Endianness::Little,
            ..PcmFormat::master(48000.0, 1)
        };
        assert_eq!(round_trip(format, &[0x123456, -1]), [0x123456, -1]);
    }

    #[test]
    fn a_compressed_payload_is_refused_rather_than_guessed_at() {
        let mut desc = [0u8; 32];
        desc[8..12].copy_from_slice(b"aac ");
        let err = format_from_desc(Path::new("t.caf"), &desc).unwrap_err();
        assert!(err.to_string().contains("linear PCM"), "{err}");
    }

    /// A channel layout must survive a rewrite. Losing it is silent corruption
    /// that only surfaces in some other decoder.
    #[test]
    fn an_uninterpreted_chunk_is_carried_through() {
        let format = PcmFormat::master(48000.0, 2);
        let chan = Chunk {
            kind: *b"chan",
            body: vec![0, 0, 0, 100, 0, 0, 0, 0, 0, 0, 0, 0],
        };

        let mut buffer = Cursor::new(Vec::new());
        let mut writer = CafWriter::with_chunks(
            &mut buffer,
            Path::new("t.caf"),
            format,
            std::slice::from_ref(&chan),
        )
        .unwrap();
        writer.write_frames(&[1, 2, 3, 4]).unwrap();
        writer.finish().unwrap();

        let bytes = buffer.into_inner();
        let length = bytes.len() as u64;
        let mut reader = CafReader::new(Cursor::new(bytes), Path::new("t.caf"), length).unwrap();
        assert_eq!(reader.extra_chunks(), &[chan]);

        // And the data chunk's size was patched at its shifted offset.
        let mut out = [0i32; 4];
        assert_eq!(reader.read_frames(&mut out).unwrap(), 2);
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn a_chunk_too_large_to_be_metadata_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"caff");
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(b"junk");
        bytes.extend_from_slice(&(MAX_PRESERVED_CHUNK as i64 + 1).to_be_bytes());
        let length = bytes.len() as u64 + MAX_PRESERVED_CHUNK + 1;

        let err = match CafReader::new(Cursor::new(bytes), Path::new("t.caf"), length) {
            Err(e) => e,
            Ok(_) => panic!("an oversized chunk was accepted"),
        };
        assert!(
            err.to_string().contains("too large to carry through"),
            "{err}"
        );
    }

    #[test]
    fn the_frame_count_comes_from_the_data_chunk() {
        let format = PcmFormat::master(48000.0, 3);
        let mut buffer = Cursor::new(Vec::new());
        let mut writer = CafWriter::new(&mut buffer, Path::new("t.caf"), format).unwrap();
        writer.write_frames(&[0; 30]).unwrap();
        writer.finish().unwrap();

        let bytes = buffer.into_inner();
        let length = bytes.len() as u64;
        let reader = CafReader::new(Cursor::new(bytes), Path::new("t.caf"), length).unwrap();
        assert_eq!(reader.frames(), 10);
    }
}
