//! The joint object coding payload: TS 103 420 clause 6.2.
//!
//! ```text
//! joc_header   downmix configuration 3, objects − 1 6, extension 3
//! joc_info     clip gain 3 + 5, sequence count 10,
//!              per object: present 1 [bands 3, sparse 1, fine 1,
//!                                     steep 1, points − 1 1, offsets 5 each if steep]
//! joc_data     per present object, per data point:
//!                sparse — the first band's channel in 3 bits, the others' and the
//!                         vector's entries through codebooks;
//!                dense  — every channel's every band through a codebook
//! padding      to the byte
//! ```
//!
//! Written from the clause, and held to the reference streams byte for byte
//! by `cargo xtask eac3`. The model keeps what the stream says as it says it —
//! codes, not dequantised gains — because what this layer has to get right is
//! the stream; what the values mean is the estimator's business.

use crate::huffman::{Codebook, Kind};
use hz_core::bits::BitWriter;
use hz_core::bits_read::{BitReader, Ran};

/// Parameter bands by `joc_num_bands_idx` (clause 6.3.3.3).
pub const BANDS: [usize; 8] = [1, 3, 5, 7, 9, 12, 15, 23];

/// One payload, as the stream codes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joc {
    /// `joc_dmx_config_idx`, nought to four: which downmix the objects are
    /// reconstructed from — five channels for nought and three, seven for the
    /// others.
    pub downmix_config: u8,
    /// `joc_clipgain_x_bits` (3) and `joc_clipgain_y_bits` (5).
    pub clip_gain: (u8, u8),
    /// `joc_seq_count_bits`: ten bits that count frames, so a decoder knows
    /// a splice.
    pub sequence: u16,
    /// One entry per object, `None` where it is not present.
    pub objects: Vec<Option<Object>>,
    /// Objects at the end whose presence bit the payload does not carry at
    /// all: it ends before them, which a decoder reads as absent. Possible
    /// only when no object is present, since the data of a present one would
    /// follow the bits; a decoder tolerates it, and it is kept so that a
    /// payload that does it writes back as it was.
    pub unflagged_tail: usize,
    /// The bits that pad the payload to its byte, as they were.
    pub padding: u8,
    /// Bytes after the padding, as they were.
    pub trailing: Vec<u8>,
}

/// A present object's parameters for this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// `joc_num_bands_idx`: an index into [`BANDS`].
    pub bands_index: u8,
    /// `joc_num_quant_idx`: the fine quantiser rather than the coarse.
    pub fine: bool,
    /// `joc_slope_idx`: the parameters step at the offsets below rather than
    /// sliding between data points.
    pub steep: bool,
    /// With `steep`, each data point's `joc_offset_ts_bits`: the time slot it
    /// takes effect at, less one.
    pub offsets: Vec<u8>,
    /// One or two data points, each the whole set of parameters.
    pub data: Data,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Data {
    /// Per data point and band, one downmix channel and its coefficient.
    Sparse(Vec<SparsePoint>),
    /// Per data point, channel and band, a coefficient: `[point][channel][band]`.
    Dense(Vec<Vec<Vec<u16>>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparsePoint {
    /// Per band, the downmix channel, coded as a step from the band before
    /// it through the index codebook — the first band's in three bits.
    pub channels: Vec<u8>,
    /// Per band, the coefficient's code.
    pub vector: Vec<u16>,
}

impl Object {
    pub fn bands(&self) -> usize {
        BANDS[usize::from(self.bands_index)]
    }

    pub fn points(&self) -> usize {
        match &self.data {
            Data::Sparse(points) => points.len(),
            Data::Dense(points) => points.len(),
        }
    }

    pub fn sparse(&self) -> bool {
        matches!(self.data, Data::Sparse(_))
    }
}

impl Joc {
    /// Downmix channels the objects are made from.
    pub fn channels(&self) -> usize {
        channels(self.downmix_config)
    }
}

fn channels(downmix_config: u8) -> usize {
    match downmix_config {
        0 | 3 => 5,
        _ => 7,
    }
}

/// A payload this cannot read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable(pub String);

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreadable {}

impl From<Ran> for Unreadable {
    fn from(ran: Ran) -> Self {
        Self(ran.0)
    }
}

/// What `joc_info` says about a present object before its data.
struct Header {
    bands_index: u8,
    sparse: bool,
    fine: bool,
    steep: bool,
    points: usize,
    offsets: Vec<u8>,
}

/// Read one payload's bytes.
pub fn read(bytes: &[u8]) -> Result<Joc, Unreadable> {
    let mut r = BitReader::new(bytes);
    let downmix_config = r.get(3)? as u8;
    if downmix_config > 4 {
        return Err(Unreadable(format!(
            "downmix configuration {downmix_config}, which is reserved"
        )));
    }
    let count = r.get(6)? as usize + 1;
    let extension = r.get(3)?;
    if extension != 0 {
        return Err(Unreadable(format!(
            "extension configuration {extension}, whose data nothing here reads"
        )));
    }
    let clip_gain = (r.get(3)? as u8, r.get(5)? as u8);
    let sequence = r.get(10)? as u16;

    let mut headers: Vec<Option<Header>> = Vec::with_capacity(count);
    let mut unflagged_tail = 0;
    for index in 0..count {
        if r.bits_left() == 0 {
            unflagged_tail = count - index;
            break;
        }
        if !r.get_bit()? {
            headers.push(None);
            continue;
        }
        let bands_index = r.get(3)? as u8;
        let sparse = r.get_bit()?;
        let fine = r.get_bit()?;
        let steep = r.get_bit()?;
        let points = r.get(1)? as usize + 1;
        let mut offsets = Vec::new();
        if steep {
            for _ in 0..points {
                offsets.push(r.get(5)? as u8);
            }
        }
        headers.push(Some(Header {
            bands_index,
            sparse,
            fine,
            steep,
            points,
            offsets,
        }));
    }

    let channels = channels(downmix_config);
    let mut objects = Vec::with_capacity(count);
    for header in headers {
        let Some(h) = header else {
            objects.push(None);
            continue;
        };
        let bands = BANDS[usize::from(h.bands_index)];
        let data = if h.sparse {
            let index = Codebook::get(Kind::Index {
                seven: channels == 7,
            });
            let vector = Codebook::get(Kind::Vector { fine: h.fine });
            let mut points = Vec::with_capacity(h.points);
            for _ in 0..h.points {
                let mut channel_codes = Vec::with_capacity(bands);
                channel_codes.push(r.get(3)? as u8);
                for _ in 1..bands {
                    channel_codes.push(index.read(&mut r)? as u8);
                }
                let mut coefficients = Vec::with_capacity(bands);
                for _ in 0..bands {
                    coefficients.push(vector.read(&mut r)?);
                }
                points.push(SparsePoint {
                    channels: channel_codes,
                    vector: coefficients,
                });
            }
            Data::Sparse(points)
        } else {
            let matrix = Codebook::get(Kind::Matrix { fine: h.fine });
            let mut points = Vec::with_capacity(h.points);
            for _ in 0..h.points {
                let mut point = Vec::with_capacity(channels);
                for _ in 0..channels {
                    let mut row = Vec::with_capacity(bands);
                    for _ in 0..bands {
                        row.push(matrix.read(&mut r)?);
                    }
                    point.push(row);
                }
                points.push(point);
            }
            Data::Dense(points)
        };
        objects.push(Some(Object {
            bands_index: h.bands_index,
            fine: h.fine,
            steep: h.steep,
            offsets: h.offsets,
            data,
        }));
    }
    objects.resize(count, None);

    let pad = (8 - r.bit_position() % 8) % 8;
    let padding = if pad > 0 { r.get(pad as u32)? as u8 } else { 0 };
    let trailing = bytes[r.byte_position()..].to_vec();
    Ok(Joc {
        downmix_config,
        clip_gain,
        sequence,
        objects,
        unflagged_tail,
        padding,
        trailing,
    })
}

impl Joc {
    /// The payload's bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut writer = BitWriter::with_capacity(512);
        self.write(&mut writer);
        writer.finish()
    }

    /// Write the payload from a byte boundary, padding to the next one.
    ///
    /// # Panics
    /// If the model is not one the syntax can say: an object count outside
    /// one to sixty-four, a reserved downmix configuration, data points other
    /// than one or two, offsets that do not match them, a coefficient or
    /// channel step a codebook has no codeword for, or a tail longer than the
    /// absent objects at the end.
    pub fn write(&self, w: &mut BitWriter) {
        assert!(self.downmix_config <= 4, "a reserved downmix configuration");
        assert!(
            (1..=64).contains(&self.objects.len()),
            "one to sixty-four objects"
        );
        assert!(
            self.unflagged_tail == 0 || self.objects.iter().all(Option::is_none),
            "presence bits go unflagged only when nothing follows them"
        );
        assert!(self.unflagged_tail < self.objects.len() + 1);
        let start = w.bit_position();
        w.put(3, u32::from(self.downmix_config));
        w.put(6, self.objects.len() as u32 - 1);
        w.put(3, 0); // no extension
        w.put(3, u32::from(self.clip_gain.0));
        w.put(5, u32::from(self.clip_gain.1));
        w.put(10, u32::from(self.sequence));

        let flagged = self.objects.len() - self.unflagged_tail;
        for object in &self.objects[..flagged] {
            let Some(o) = object else {
                w.put_bit(false);
                continue;
            };
            let points = o.points();
            assert!((1..=2).contains(&points), "one or two data points");
            w.put_bit(true);
            w.put(3, u32::from(o.bands_index));
            w.put_bit(o.sparse());
            w.put_bit(o.fine);
            w.put_bit(o.steep);
            w.put(1, points as u32 - 1);
            if o.steep {
                assert_eq!(o.offsets.len(), points, "an offset per data point");
                for offset in &o.offsets {
                    w.put(5, u32::from(*offset));
                }
            }
        }

        let channels = self.channels();
        for o in self.objects.iter().flatten() {
            let bands = o.bands();
            match &o.data {
                Data::Sparse(points) => {
                    let index = Codebook::get(Kind::Index {
                        seven: channels == 7,
                    });
                    let vector = Codebook::get(Kind::Vector { fine: o.fine });
                    for point in points {
                        assert_eq!(point.channels.len(), bands);
                        assert_eq!(point.vector.len(), bands);
                        w.put(3, u32::from(point.channels[0]));
                        for step in &point.channels[1..] {
                            index.write(w, u16::from(*step));
                        }
                        for coefficient in &point.vector {
                            vector.write(w, *coefficient);
                        }
                    }
                }
                Data::Dense(points) => {
                    let matrix = Codebook::get(Kind::Matrix { fine: o.fine });
                    for point in points {
                        assert_eq!(point.len(), channels, "a row per downmix channel");
                        for row in point {
                            assert_eq!(row.len(), bands);
                            for coefficient in row {
                                matrix.write(w, *coefficient);
                            }
                        }
                    }
                }
            }
        }

        let pad = (8 - (w.bit_position() - start) % 8) % 8;
        if pad > 0 {
            w.put(pad as u32, u32::from(self.padding));
        }
        w.put_bytes(&self.trailing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(downmix_config: u8) -> Joc {
        let channels = channels(downmix_config);
        Joc {
            downmix_config,
            clip_gain: (2, 17),
            sequence: 1001,
            objects: vec![
                Some(Object {
                    bands_index: 3,
                    fine: false,
                    steep: false,
                    offsets: vec![],
                    data: Data::Dense(vec![
                        (0..channels)
                            .map(|ch| (0..7).map(|b| ((ch * 3 + b) % 9) as u16).collect())
                            .collect(),
                    ]),
                }),
                None,
                Some(Object {
                    bands_index: 1,
                    fine: true,
                    steep: true,
                    offsets: vec![3, 20],
                    data: Data::Sparse(vec![
                        SparsePoint {
                            channels: vec![2, 0, 1],
                            vector: vec![5, 0, 12],
                        },
                        SparsePoint {
                            channels: vec![4, 3, 0],
                            vector: vec![0, 7, 1],
                        },
                    ]),
                }),
                None,
                None,
            ],
            unflagged_tail: 0,
            padding: 0,
            trailing: vec![],
        }
    }

    /// What is written reads back as itself, for both channel counts, sparse
    /// and dense, steep and smooth, one and two data points.
    #[test]
    fn a_payload_reads_back_as_itself() {
        for config in [0, 1, 2, 3, 4] {
            let joc = sample(config);
            let bytes = joc.to_bytes();
            assert_eq!(read(&bytes).unwrap(), joc, "configuration {config}");
            assert_eq!(read(&bytes).unwrap().to_bytes(), bytes);
        }
    }

    /// A payload of absent objects that ends before its last presence bits:
    /// read, they are absent and unflagged, and the bytes write back as they
    /// were. Thirty header bits and two presence bits end on a byte, so the
    /// other thirty objects of thirty-two go unflagged.
    #[test]
    fn an_unflagged_tail_writes_back_without_its_bits() {
        let mut joc = sample(0);
        joc.objects = vec![None; 32];
        joc.unflagged_tail = 30;
        let bytes = joc.to_bytes();
        assert_eq!(bytes.len(), 4);
        let back = read(&bytes).unwrap();
        assert_eq!(back.unflagged_tail, 30);
        assert_eq!(back.objects.len(), 32);
        assert_eq!(back.to_bytes(), bytes);
    }

    #[test]
    fn padding_and_trailing_bytes_come_back() {
        let mut joc = sample(3);
        let bits = joc.to_bytes();
        joc.trailing = vec![0xde, 0xad];
        let with = joc.to_bytes();
        assert_eq!(with.len(), bits.len() + 2);
        assert_eq!(read(&with).unwrap(), joc);
    }

    #[test]
    fn a_reserved_configuration_is_refused() {
        let mut w = BitWriter::new();
        w.put(3, 5);
        w.put(13, 0);
        assert!(read(&w.finish()).is_err());
        let mut w = BitWriter::new();
        w.put(3, 0);
        w.put(6, 0);
        w.put(3, 1); // an extension
        w.put(4, 0);
        assert!(read(&w.finish()).is_err());
    }
}
