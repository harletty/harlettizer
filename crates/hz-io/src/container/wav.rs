//! RIFF/WAVE, and the two 64-bit forms that carry an ADM master: RF64 and
//! BW64.
//!
//! The three differ in four bytes of header and one extra chunk. RIFF cannot
//! describe a file past four gigabytes, so both successors set every 32-bit
//! size field to `0xFFFFFFFF` and put the real numbers in a `ds64` chunk. BW64
//! (EBU Tech 3306) is RF64 with a different signature and the expectation that
//! `chna` and `axml` are present — which is what makes it the container for an
//! ADM master.
//!
//! The `fmt ` chunk is modelled rather than copied through. It would be easier
//! to keep its bytes and re-emit them, and it would make the round-trip test
//! pass for the wrong reason: writing an ADM master means *constructing* a
//! format chunk, channel mask included, not echoing someone else's.

use super::Chunk;
use super::pcm::{self, Endianness, PcmFormat, SampleFormat};
use hz_core::{Error, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const RIFF: &[u8; 4] = b"RIFF";
const RF64: &[u8; 4] = b"RF64";
const BW64: &[u8; 4] = b"BW64";
const WAVE: &[u8; 4] = b"WAVE";
const CHUNK_FMT: &[u8; 4] = b"fmt ";
const CHUNK_DATA: &[u8; 4] = b"data";
const CHUNK_DS64: &[u8; 4] = b"ds64";
/// A 32-bit size field that means "look in `ds64`".
const SIZE_ESCAPE: u32 = 0xFFFF_FFFF;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Largest chunk carried through verbatim. An `axml` payload is real XML and
/// can be large, so this is far more generous than the container's other
/// metadata needs.
const MAX_PRESERVED_CHUNK: u64 = 64 << 20;

/// And the two chunks that are *modelled* rather than carried through, whose
/// shapes are known.
///
/// A size field is four bytes out of a file, so it reaches four gigabytes, and
/// `vec![0; size]` on the strength of one is a malformed header turned into an
/// allocation. `fmt ` is sixteen bytes, or eighteen, or forty when it is
/// extensible; `ds64` is twenty-eight plus a table of chunk sizes. Both bounds
/// are generous and both are finite.
const MAX_FMT: u64 = 256;
const MAX_DS64: u64 = 4 << 10;

/// Read one chunk body, refusing a size the chunk cannot have.
///
/// The refusal is the point: the alternative is to trust a four-byte field far
/// enough to allocate on it, and a file that says its `fmt ` chunk is four
/// gigabytes is a file that is wrong, not a file that is large.
fn read_body(
    path: &Path,
    inner: &mut impl Read,
    size: u64,
    limit: u64,
    what: &str,
) -> Result<Vec<u8>> {
    if size > limit {
        return Err(Error::malformed(
            path,
            format!("a `{what}` chunk of {size} bytes; it holds at most {limit}"),
        ));
    }
    let mut body = vec![0u8; size as usize];
    inner
        .read_exact(&mut body)
        .map_err(|e| Error::io(path, e))?;
    Ok(body)
}

/// Which of the three signatures a file carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiffKind {
    /// Plain RIFF/WAVE. Cannot exceed four gigabytes.
    Riff,
    /// RF64: 64-bit sizes in a `ds64` chunk.
    Rf64,
    /// BW64: RF64's structure, and where an ADM master lives.
    Bw64,
}

impl RiffKind {
    fn signature(self) -> &'static [u8; 4] {
        match self {
            Self::Riff => RIFF,
            Self::Rf64 => RF64,
            Self::Bw64 => BW64,
        }
    }

    /// Whether sizes live in a `ds64` chunk rather than in the fields.
    pub fn is_64_bit(self) -> bool {
        self != Self::Riff
    }
}

/// One entry in a file's chunk order.
///
/// Kept because position is not decoration here. A `JUNK` chunk sitting ahead
/// of `fmt ` is the space a writer reserved so the file can become RF64 in
/// place later; moving it behind `fmt ` leaves a valid file that has quietly
/// lost that option. Recording the order costs one enum and means a rewrite
/// puts everything back where it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// The 64-bit size chunk, or the `JUNK` placeholder standing in for it.
    Ds64,
    Fmt,
    Data,
    Other(Chunk),
}

/// The extra tail of a `WAVE_FORMAT_EXTENSIBLE` format chunk.
///
/// Anything above two channels is supposed to use it, and the channel mask is
/// the only place a plain WAV says which speaker each track feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extensible {
    pub valid_bits: u16,
    pub channel_mask: u32,
    pub subformat: [u8; 16],
}

impl Extensible {
    /// The GUID tail every PCM subformat shares.
    const GUID_TAIL: [u8; 14] = [
        0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
    ];

    fn guid_for(tag: u16) -> [u8; 16] {
        let mut guid = [0u8; 16];
        guid[0..2].copy_from_slice(&tag.to_le_bytes());
        guid[2..16].copy_from_slice(&Self::GUID_TAIL);
        guid
    }

    /// The format tag hiding in the subformat GUID.
    fn tag(&self) -> u16 {
        u16::from_le_bytes([self.subformat[0], self.subformat[1]])
    }
}

/// A `fmt ` chunk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WavFormat {
    pub pcm: PcmFormat,
    /// Present when the file uses `WAVE_FORMAT_EXTENSIBLE`.
    pub extensible: Option<Extensible>,
}

impl WavFormat {
    /// A plain, non-extensible format chunk.
    pub fn plain(pcm: PcmFormat) -> Self {
        Self {
            pcm,
            extensible: None,
        }
    }

    /// An extensible format chunk with the given speaker mask.
    pub fn extensible(pcm: PcmFormat, channel_mask: u32) -> Self {
        let tag = match pcm.sample_format {
            SampleFormat::Float => WAVE_FORMAT_IEEE_FLOAT,
            SampleFormat::Integer => WAVE_FORMAT_PCM,
        };
        Self {
            pcm,
            extensible: Some(Extensible {
                valid_bits: pcm.bits_per_channel as u16,
                channel_mask,
                subformat: Extensible::guid_for(tag),
            }),
        }
    }

    fn parse(path: &Path, body: &[u8]) -> Result<Self> {
        if body.len() < 16 {
            return Err(Error::malformed(path, "short fmt chunk"));
        }
        let tag = u16::from_le_bytes(body[0..2].try_into().unwrap());
        let channels = u16::from_le_bytes(body[2..4].try_into().unwrap()) as u32;
        let sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let block_align = u16::from_le_bytes(body[12..14].try_into().unwrap()) as usize;
        let bits_per_channel = u16::from_le_bytes(body[14..16].try_into().unwrap()) as u32;

        let extensible = if tag == WAVE_FORMAT_EXTENSIBLE {
            if body.len() < 40 {
                return Err(Error::malformed(path, "short extensible fmt chunk"));
            }
            Some(Extensible {
                valid_bits: u16::from_le_bytes(body[18..20].try_into().unwrap()),
                channel_mask: u32::from_le_bytes(body[20..24].try_into().unwrap()),
                subformat: body[24..40].try_into().unwrap(),
            })
        } else {
            None
        };

        let effective_tag = match &extensible {
            Some(e) => e.tag(),
            None => tag,
        };
        let sample_format = match effective_tag {
            WAVE_FORMAT_PCM => SampleFormat::Integer,
            WAVE_FORMAT_IEEE_FLOAT => SampleFormat::Float,
            other => {
                return Err(Error::unsupported(
                    path,
                    format!("format tag {other:#06x}; only linear PCM is handled"),
                ));
            }
        };

        let pcm = PcmFormat {
            sample_rate: sample_rate as f64,
            channels,
            bits_per_channel,
            sample_format,
            endianness: Endianness::Little,
        };

        // Packed frames only: a stride wider than the samples is padding this
        // reader would silently walk over.
        if block_align != pcm.bytes_per_frame() {
            return Err(Error::unsupported(
                path,
                format!(
                    "block align {block_align} for {channels} channels of {bits_per_channel} bits"
                ),
            ));
        }
        pcm.check_supported(path)?;

        Ok(Self { pcm, extensible })
    }

    fn to_chunk_body(self) -> Vec<u8> {
        let pcm = self.pcm;
        let mut body = Vec::with_capacity(40);
        let tag = match (&self.extensible, pcm.sample_format) {
            (Some(_), _) => WAVE_FORMAT_EXTENSIBLE,
            (None, SampleFormat::Float) => WAVE_FORMAT_IEEE_FLOAT,
            (None, SampleFormat::Integer) => WAVE_FORMAT_PCM,
        };
        let byte_rate = pcm.sample_rate_hz() as usize * pcm.bytes_per_frame();

        body.extend_from_slice(&tag.to_le_bytes());
        body.extend_from_slice(&(pcm.channels as u16).to_le_bytes());
        body.extend_from_slice(&pcm.sample_rate_hz().to_le_bytes());
        body.extend_from_slice(&(byte_rate as u32).to_le_bytes());
        body.extend_from_slice(&(pcm.bytes_per_frame() as u16).to_le_bytes());
        body.extend_from_slice(&(pcm.bits_per_channel as u16).to_le_bytes());

        if let Some(e) = self.extensible {
            body.extend_from_slice(&22u16.to_le_bytes());
            body.extend_from_slice(&e.valid_bits.to_le_bytes());
            body.extend_from_slice(&e.channel_mask.to_le_bytes());
            body.extend_from_slice(&e.subformat);
        }
        body
    }
}

/// Reads the sample data out of a RIFF, RF64 or BW64 file.
pub struct WavReader<R: Read + Seek> {
    inner: R,
    path: PathBuf,
    kind: RiffKind,
    format: WavFormat,
    frames: u64,
    frames_read: u64,
    staging: Vec<u8>,
    layout: Vec<Slot>,
}

impl WavReader<BufReader<File>> {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        let length = file.metadata().map_err(|e| Error::io(path, e))?.len();
        Self::new(BufReader::new(file), path, length)
    }
}

impl<R: Read + Seek> WavReader<R> {
    pub fn new(mut inner: R, path: &Path, file_length: u64) -> Result<Self> {
        let mut header = [0u8; 12];
        inner
            .read_exact(&mut header)
            .map_err(|e| Error::io(path, e))?;

        let signature: [u8; 4] = header[0..4].try_into().unwrap();
        let kind = match &signature {
            RIFF => RiffKind::Riff,
            RF64 => RiffKind::Rf64,
            BW64 => RiffKind::Bw64,
            other => {
                return Err(Error::malformed(
                    path,
                    format!(
                        "`{}` is not a RIFF signature",
                        String::from_utf8_lossy(other)
                    ),
                ));
            }
        };
        if &header[8..12] != WAVE {
            return Err(Error::malformed(path, "not a WAVE file"));
        }

        let mut format = None;
        let mut data = None;
        let mut data_size_64 = None;
        let mut layout = Vec::new();
        let mut offset = 12u64;

        while offset + 8 <= file_length {
            inner
                .seek(SeekFrom::Start(offset))
                .map_err(|e| Error::io(path, e))?;
            let mut chunk_header = [0u8; 8];
            inner
                .read_exact(&mut chunk_header)
                .map_err(|e| Error::io(path, e))?;

            let id: [u8; 4] = chunk_header[0..4].try_into().unwrap();
            let declared = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap());
            let body = offset + 8;

            let size = match (&id, declared) {
                // The escape means the real size is in `ds64`.
                (CHUNK_DATA, SIZE_ESCAPE) => data_size_64
                    .ok_or_else(|| Error::malformed(path, "data size escapes to a missing ds64"))?,
                _ => declared as u64,
            };

            match &id {
                CHUNK_FMT => {
                    let body_bytes = read_body(path, &mut inner, size, MAX_FMT, "fmt ")?;
                    format = Some(WavFormat::parse(path, &body_bytes)?);
                    layout.push(Slot::Fmt);
                }
                CHUNK_DATA => {
                    data = Some((body, size));
                    layout.push(Slot::Data);
                }
                CHUNK_DS64 => {
                    let body_bytes = read_body(path, &mut inner, size, MAX_DS64, "ds64")?;
                    if body_bytes.len() < 24 {
                        return Err(Error::malformed(path, "short ds64 chunk"));
                    }
                    data_size_64 = Some(u64::from_le_bytes(body_bytes[8..16].try_into().unwrap()));
                    layout.push(Slot::Ds64);
                }
                _ => {
                    if size > MAX_PRESERVED_CHUNK {
                        return Err(Error::unsupported(
                            path,
                            format!(
                                "`{}` chunk of {size} bytes, too large to carry through",
                                String::from_utf8_lossy(&id)
                            ),
                        ));
                    }
                    let body_bytes = read_body(
                        path,
                        &mut inner,
                        size,
                        MAX_PRESERVED_CHUNK,
                        &String::from_utf8_lossy(&id),
                    )?;
                    layout.push(Slot::Other(Chunk {
                        kind: id,
                        body: body_bytes,
                    }));
                }
            }

            // Chunks are word-aligned, and the pad byte is not in the size.
            offset = body + size + (size & 1);
        }

        let format = format.ok_or_else(|| Error::malformed(path, "no fmt chunk"))?;
        let (data_start, data_size) =
            data.ok_or_else(|| Error::malformed(path, "no data chunk"))?;
        let frames = data_size / format.pcm.bytes_per_frame() as u64;

        inner
            .seek(SeekFrom::Start(data_start))
            .map_err(|e| Error::io(path, e))?;

        Ok(Self {
            inner,
            path: path.to_path_buf(),
            kind,
            format,
            frames,
            frames_read: 0,
            staging: Vec::new(),
            layout,
        })
    }

    pub fn kind(&self) -> RiffKind {
        self.kind
    }

    pub fn format(&self) -> &WavFormat {
        &self.format
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The file's chunk order, so a rewrite can reproduce it.
    pub fn layout(&self) -> &[Slot] {
        &self.layout
    }

    /// Find a carried-through chunk by id — `chna` and `axml` arrive this way.
    pub fn chunk(&self, id: &[u8; 4]) -> Option<&Chunk> {
        self.layout.iter().find_map(|slot| match slot {
            Slot::Other(chunk) if &chunk.kind == id => Some(chunk),
            _ => None,
        })
    }

    /// Fill `out` with interleaved samples, returning how many frames landed.
    pub fn read_frames(&mut self, out: &mut [i32]) -> Result<usize> {
        let channels = self.format.pcm.channels as usize;
        let wanted = (out.len() / channels).min((self.frames - self.frames_read) as usize);
        if wanted == 0 {
            return Ok(0);
        }

        let bytes = wanted * self.format.pcm.bytes_per_frame();
        if self.staging.len() < bytes {
            self.staging.resize(bytes, 0);
        }
        self.inner
            .read_exact(&mut self.staging[..bytes])
            .map_err(|e| Error::io(&self.path, e))?;

        pcm::decode(
            &self.staging[..bytes],
            &self.format.pcm,
            &mut out[..wanted * channels],
        );
        self.frames_read += wanted as u64;
        Ok(wanted)
    }
}

impl<R: Read + Seek + Send> super::FrameSource for WavReader<R> {
    fn pcm_format(&self) -> &PcmFormat {
        &self.format.pcm
    }

    fn frame_count(&self) -> u64 {
        self.frames
    }

    fn read_frames(&mut self, out: &mut [i32]) -> Result<usize> {
        WavReader::read_frames(self, out)
    }
}

/// Writes a RIFF, RF64 or BW64 file, patching sizes once they are known.
pub struct WavWriter<W: Write + Seek> {
    inner: W,
    path: PathBuf,
    kind: RiffKind,
    format: WavFormat,
    frames: u64,
    staging: Vec<u8>,
    /// Chunks still to be written once the data chunk is closed.
    tail: Vec<Chunk>,
    ds64_offset: Option<u64>,
    data_size_offset: u64,
    data_start: u64,
}

impl WavWriter<BufWriter<File>> {
    pub fn create(path: &Path, kind: RiffKind, format: WavFormat) -> Result<Self> {
        Self::create_with_layout(path, kind, format, &default_layout(kind))
    }

    /// Create a file whose chunks land in `layout`'s order — typically what a
    /// [`WavReader::layout`] handed back.
    pub fn create_with_layout(
        path: &Path,
        kind: RiffKind,
        format: WavFormat,
        layout: &[Slot],
    ) -> Result<Self> {
        let file = File::create(path).map_err(|e| Error::io(path, e))?;
        Self::with_layout(BufWriter::new(file), path, kind, format, layout)
    }
}

/// What a file gets when the caller has no layout to preserve.
pub fn default_layout(kind: RiffKind) -> Vec<Slot> {
    let mut layout = Vec::with_capacity(3);
    if kind.is_64_bit() {
        layout.push(Slot::Ds64);
    }
    layout.push(Slot::Fmt);
    layout.push(Slot::Data);
    layout
}

/// Refuse a layout that cannot become a readable file, before a byte is
/// written.
///
/// Everything here was previously either dropped in silence — the header loop
/// ignores a `fmt `, `data` or `ds64` slot that follows the data chunk — or
/// raised by `finish`, after every sample of a programme had already gone to
/// disk. A layout is a handful of enum values and checking it costs nothing,
/// so it is checked at the top rather than discovered at the bottom.
fn check_layout(path: &Path, kind: RiffKind, layout: &[Slot]) -> Result<()> {
    let count = |wanted: &Slot| layout.iter().filter(|slot| *slot == wanted).count();
    let at = |wanted: &Slot| layout.iter().position(|slot| slot == wanted);

    if count(&Slot::Data) != 1 {
        return Err(Error::malformed(
            path,
            format!("a layout with {} data chunks", count(&Slot::Data)),
        ));
    }
    if count(&Slot::Fmt) != 1 {
        return Err(Error::malformed(
            path,
            format!("a layout with {} format chunks", count(&Slot::Fmt)),
        ));
    }
    if at(&Slot::Fmt) > at(&Slot::Data) {
        return Err(Error::malformed(
            path,
            "a layout whose format chunk follows its data chunk",
        ));
    }

    // A 64-bit file keeps the escape in its 32-bit size fields and states the
    // real ones in `ds64`, which the reader looks for immediately after
    // `WAVE`. Without the slot `finish` has nowhere to write them — and said
    // so only once the file was full.
    match (kind.is_64_bit(), count(&Slot::Ds64)) {
        (true, 1) if at(&Slot::Ds64) == Some(0) => {}
        (true, 1) => {
            return Err(Error::malformed(
                path,
                "a 64-bit layout whose ds64 chunk is not first",
            ));
        }
        (true, n) => {
            return Err(Error::malformed(
                path,
                format!("a 64-bit layout with {n} ds64 chunks"),
            ));
        }
        (false, 0) => {}
        (false, n) => {
            return Err(Error::malformed(
                path,
                format!("a 32-bit layout with {n} ds64 chunks"),
            ));
        }
    }

    // A carried-through chunk with a reserved id would be written verbatim
    // beside the one this writes, and a file with two format chunks is a file
    // whose second one a reader may believe.
    for slot in layout {
        if let Slot::Other(chunk) = slot {
            if [CHUNK_FMT, CHUNK_DATA, CHUNK_DS64].contains(&&chunk.kind) {
                let kind = String::from_utf8_lossy(&chunk.kind).to_string();
                return Err(Error::malformed(
                    path,
                    format!("a carried-through `{kind}` chunk, which this writes itself"),
                ));
            }
        }
    }
    Ok(())
}

impl<W: Write + Seek> WavWriter<W> {
    pub fn new(inner: W, path: &Path, kind: RiffKind, format: WavFormat) -> Result<Self> {
        Self::with_layout(inner, path, kind, format, &default_layout(kind))
    }

    pub fn with_layout(
        mut inner: W,
        path: &Path,
        kind: RiffKind,
        format: WavFormat,
        layout: &[Slot],
    ) -> Result<Self> {
        format.pcm.check_supported(path)?;
        check_layout(path, kind, layout)?;

        let mut header = Vec::with_capacity(128);
        header.extend_from_slice(kind.signature());
        header.extend_from_slice(&SIZE_ESCAPE.to_le_bytes()); // patched, or left as the escape
        header.extend_from_slice(WAVE);

        let mut ds64_offset = None;
        let mut data_size_offset = None;
        let mut tail = Vec::new();

        for slot in layout {
            // Everything after the data chunk waits until it is closed.
            if data_size_offset.is_some() {
                if let Slot::Other(chunk) = slot {
                    tail.push(chunk.clone());
                }
                continue;
            }
            match slot {
                Slot::Ds64 => {
                    ds64_offset = Some(header.len() as u64 + 8);
                    header.extend_from_slice(CHUNK_DS64);
                    header.extend_from_slice(&28u32.to_le_bytes());
                    header.extend_from_slice(&[0u8; 28]); // patched by `finish`
                }
                Slot::Fmt => {
                    let body = format.to_chunk_body();
                    header.extend_from_slice(CHUNK_FMT);
                    header.extend_from_slice(&(body.len() as u32).to_le_bytes());
                    header.extend_from_slice(&body);
                }
                Slot::Data => {
                    header.extend_from_slice(CHUNK_DATA);
                    data_size_offset = Some(header.len() as u64);
                    header.extend_from_slice(&SIZE_ESCAPE.to_le_bytes());
                }
                Slot::Other(chunk) => push_chunk(&mut header, chunk),
            }
        }

        let data_size_offset = data_size_offset.expect("the layout was checked for a data chunk");
        let data_start = header.len() as u64;

        inner.write_all(&header).map_err(|e| Error::io(path, e))?;

        Ok(Self {
            inner,
            path: path.to_path_buf(),
            kind,
            format,
            frames: 0,
            staging: Vec::new(),
            tail,
            ds64_offset,
            data_size_offset,
            data_start,
        })
    }

    pub fn write_frames(&mut self, samples: &[i32]) -> Result<()> {
        let channels = self.format.pcm.channels as usize;
        if !samples.len().is_multiple_of(channels) {
            return Err(Error::malformed(
                &self.path,
                format!(
                    "{} samples is not whole frames of {channels}",
                    samples.len()
                ),
            ));
        }

        let bytes = samples.len() * self.format.pcm.bytes_per_sample();
        if self.staging.len() < bytes {
            self.staging.resize(bytes, 0);
        }
        pcm::encode(samples, &self.format.pcm, &mut self.staging[..bytes]);
        self.inner
            .write_all(&self.staging[..bytes])
            .map_err(|e| Error::io(&self.path, e))?;

        self.frames += (samples.len() / channels) as u64;
        Ok(())
    }

    /// Close the data chunk, append the trailing chunks, and patch every size.
    pub fn finish(mut self) -> Result<()> {
        let data_size = self.frames * self.format.pcm.bytes_per_frame() as u64;

        // Word alignment: the pad byte is written but not counted.
        if !data_size.is_multiple_of(2) {
            self.inner
                .write_all(&[0])
                .map_err(|e| Error::io(&self.path, e))?;
        }

        let mut bytes = Vec::new();
        for chunk in &self.tail {
            push_chunk(&mut bytes, chunk);
        }
        self.inner
            .write_all(&bytes)
            .map_err(|e| Error::io(&self.path, e))?;

        let riff_size = self.data_start + data_size + (data_size & 1) + bytes.len() as u64 - 8;

        if self.kind.is_64_bit() {
            // The 32-bit fields keep their escape; the real sizes go in ds64.
            let offset = self.ds64_offset.ok_or_else(|| {
                Error::malformed(&self.path, "a 64-bit file needs a ds64 slot in its layout")
            })?;
            let mut ds64 = Vec::with_capacity(28);
            ds64.extend_from_slice(&riff_size.to_le_bytes());
            ds64.extend_from_slice(&data_size.to_le_bytes());
            ds64.extend_from_slice(&self.frames.to_le_bytes());
            ds64.extend_from_slice(&0u32.to_le_bytes()); // no table entries
            self.patch(offset, &ds64)?;
        } else {
            if riff_size > SIZE_ESCAPE as u64 {
                return Err(Error::malformed(
                    &self.path,
                    "RIFF cannot describe a file this large; write BW64 instead",
                ));
            }
            self.patch(4, &(riff_size as u32).to_le_bytes())?;
            self.patch(self.data_size_offset, &(data_size as u32).to_le_bytes())?;
        }

        self.inner.flush().map_err(|e| Error::io(&self.path, e))?;
        Ok(())
    }

    fn patch(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.inner
            .seek(SeekFrom::Start(offset))
            .map_err(|e| Error::io(&self.path, e))?;
        self.inner
            .write_all(bytes)
            .map_err(|e| Error::io(&self.path, e))
    }
}

fn push_chunk(out: &mut Vec<u8>, chunk: &Chunk) {
    out.extend_from_slice(&chunk.kind);
    out.extend_from_slice(&(chunk.body.len() as u32).to_le_bytes());
    out.extend_from_slice(&chunk.body);
    if !chunk.body.len().is_multiple_of(2) {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn round_trip(kind: RiffKind, format: WavFormat, samples: &[i32]) -> Vec<i32> {
        let mut buffer = Cursor::new(Vec::new());
        let mut writer = WavWriter::new(&mut buffer, Path::new("t.wav"), kind, format).unwrap();
        writer.write_frames(samples).unwrap();
        writer.finish().unwrap();

        let bytes = buffer.into_inner();
        let length = bytes.len() as u64;
        let mut reader = WavReader::new(Cursor::new(bytes), Path::new("t.wav"), length).unwrap();
        assert_eq!(reader.kind(), kind);
        assert_eq!(reader.format(), &format);

        let mut out = vec![0i32; samples.len()];
        let frames = reader.read_frames(&mut out).unwrap();
        assert_eq!(frames, samples.len() / format.pcm.channels as usize);
        out
    }

    #[test]
    fn every_signature_round_trips() {
        let samples = [0, 1, -1, 8_388_607, -8_388_608, 7];
        for kind in [RiffKind::Riff, RiffKind::Rf64, RiffKind::Bw64] {
            let format = WavFormat::plain(PcmFormat::wav(48000.0, 2, 24));
            assert_eq!(round_trip(kind, format, &samples), samples, "{kind:?}");
        }
    }

    #[test]
    fn an_extensible_format_keeps_its_channel_mask() {
        let format = WavFormat::extensible(PcmFormat::wav(48000.0, 6, 24), 0x3F);
        let samples = [1, 2, 3, 4, 5, 6];
        assert_eq!(round_trip(RiffKind::Bw64, format, &samples), samples);
        assert_eq!(format.extensible.unwrap().channel_mask, 0x3F);
    }

    /// An odd-sized data chunk gets a pad byte that is not counted in its
    /// size. Reading the next chunk from the uncorrected offset finds noise.
    #[test]
    fn an_odd_length_payload_is_padded_without_being_counted() {
        let format = WavFormat::plain(PcmFormat::wav(48000.0, 1, 24));
        let trailing = Chunk {
            kind: *b"axml",
            body: b"<x/>".to_vec(),
        };

        let mut buffer = Cursor::new(Vec::new());
        let layout = [Slot::Fmt, Slot::Data, Slot::Other(trailing.clone())];
        let mut writer = WavWriter::with_layout(
            &mut buffer,
            Path::new("t.wav"),
            RiffKind::Riff,
            format,
            &layout,
        )
        .unwrap();
        // Three frames of three bytes: nine, an odd payload.
        writer.write_frames(&[1, 2, 3]).unwrap();
        writer.finish().unwrap();

        let bytes = buffer.into_inner();
        let length = bytes.len() as u64;
        let reader = WavReader::new(Cursor::new(bytes), Path::new("t.wav"), length).unwrap();
        assert_eq!(reader.frames(), 3);
        assert_eq!(reader.chunk(b"axml"), Some(&trailing));
    }

    /// Chunk order is preserved exactly, including ahead of `fmt `: a `JUNK`
    /// chunk in that position is the space reserved so the file can become
    /// RF64 in place, and moving it behind `fmt ` quietly spends that.
    #[test]
    fn chunk_order_survives_a_rewrite() {
        let format = WavFormat::plain(PcmFormat::wav(48000.0, 1, 16));
        let layout = [
            Slot::Other(Chunk {
                kind: *b"JUNK",
                body: vec![0; 28],
            }),
            Slot::Fmt,
            Slot::Other(Chunk {
                kind: *b"chna",
                body: vec![1, 2, 3, 4],
            }),
            Slot::Data,
            Slot::Other(Chunk {
                kind: *b"axml",
                body: b"<x/>".to_vec(),
            }),
        ];

        let mut buffer = Cursor::new(Vec::new());
        let mut writer = WavWriter::with_layout(
            &mut buffer,
            Path::new("t.wav"),
            RiffKind::Riff,
            format,
            &layout,
        )
        .unwrap();
        writer.write_frames(&[1, 2]).unwrap();
        writer.finish().unwrap();

        let bytes = buffer.into_inner();
        let length = bytes.len() as u64;
        let reader = WavReader::new(Cursor::new(bytes), Path::new("t.wav"), length).unwrap();
        assert_eq!(reader.layout(), layout);
    }

    #[test]
    fn a_compressed_payload_is_refused_rather_than_guessed_at() {
        let mut body = vec![0u8; 16];
        body[0..2].copy_from_slice(&0x0055u16.to_le_bytes()); // MPEG layer 3
        let err = WavFormat::parse(Path::new("t.wav"), &body)
            .expect_err("a compressed format tag was accepted");
        assert!(err.to_string().contains("linear PCM"), "{err}");
    }
}

#[cfg(test)]
mod chunk_size_tests {
    use super::*;

    /// A size field is four bytes out of a file, so it reaches four gigabytes.
    /// Allocating on the strength of one turns a malformed header into an
    /// allocation, and the two chunks this crate *models* have known shapes to
    /// refuse it with.
    #[test]
    fn a_chunk_larger_than_it_can_be_is_refused_rather_than_allocated() {
        let path = Path::new("test.wav");
        let empty: &[u8] = &[];
        for (limit, what) in [(MAX_FMT, "fmt "), (MAX_DS64, "ds64")] {
            let err = read_body(path, &mut &empty[..], u64::from(u32::MAX), limit, what)
                .expect_err("a four-gigabyte fmt chunk is not a large file");
            let said = err.to_string();
            assert!(said.contains(what), "{said}");
            assert!(said.contains("at most"), "{said}");
        }
    }

    /// And a chunk of a size it can have is read.
    #[test]
    fn a_chunk_within_its_shape_is_read() {
        let path = Path::new("test.wav");
        let body = vec![7u8; 40];
        let read = read_body(path, &mut &body[..], 40, MAX_FMT, "fmt ").unwrap();
        assert_eq!(read, body);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    fn format() -> WavFormat {
        WavFormat {
            pcm: PcmFormat {
                sample_rate: 48_000.0,
                channels: 2,
                bits_per_channel: 24,
                sample_format: SampleFormat::Integer,
                endianness: Endianness::Little,
            },
            extensible: None,
        }
    }

    fn write(kind: RiffKind, layout: &[Slot]) -> Result<()> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        WavWriter::with_layout(&mut buffer, Path::new("<test>"), kind, format(), layout)?;
        Ok(())
    }

    /// A layout that cannot become a readable file is refused before a byte of
    /// audio is written, and by name.
    #[test]
    fn a_layout_that_cannot_be_written_is_refused_at_the_top() {
        use Slot::*;
        let other = |kind: &[u8; 4]| {
            Other(Chunk {
                kind: *kind,
                body: Vec::new(),
            })
        };

        assert!(write(RiffKind::Riff, &[Fmt, Data]).is_ok());
        assert!(write(RiffKind::Bw64, &[Ds64, Fmt, Data]).is_ok());
        assert!(write(RiffKind::Bw64, &[Ds64, Fmt, Data, other(b"axml")]).is_ok());

        // No data chunk, or two.
        assert!(write(RiffKind::Riff, &[Fmt]).is_err());
        assert!(write(RiffKind::Riff, &[Fmt, Data, Data]).is_err());
        // No format chunk, or one a reader would reach after the audio.
        assert!(write(RiffKind::Riff, &[Data]).is_err());
        assert!(write(RiffKind::Riff, &[Data, Fmt]).is_err());
        // A 64-bit file with nowhere to put its sizes, which used to be found
        // by `finish` after the file was full.
        assert!(write(RiffKind::Bw64, &[Fmt, Data]).is_err());
        assert!(write(RiffKind::Bw64, &[Fmt, Ds64, Data]).is_err());
        // And a 32-bit one with a ds64 nothing will ever patch.
        assert!(write(RiffKind::Riff, &[Ds64, Fmt, Data]).is_err());
        // A carried-through chunk this writes itself.
        assert!(write(RiffKind::Riff, &[Fmt, Data, other(b"fmt ")]).is_err());
        assert!(write(RiffKind::Riff, &[other(b"data"), Fmt, Data]).is_err());
    }
}
