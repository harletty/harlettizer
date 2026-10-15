//! `harlettizer ac3 --eac3 --joc`: a master's objects carried beside its 5.1
//! core by joint object coding (TS 103 420), their positions by the object
//! metadata, both in an Evolution frame in each syncframe's skip field — where
//! every reference stream carries its own.
//!
//! The gains are fitted to the core a decoder plays, not the one that went in:
//! `hz-ac3` reconstructs it ([`Encoder::reconstruct`]). Two things stand in the
//! way of fitting it and carrying the result in the same frame. The fit of a
//! frame's gains looks half a frame ahead, into a core not yet coded; and the
//! Evolution frame's bytes come out of the audio's, so what it carries changes
//! the core it was fitted to. So every Evolution frame is padded to one size,
//! which makes the core's bits independent of what it carries, and two
//! encoders run: a probe a frame ahead, carrying a placeholder of that size,
//! whose reconstruction the gains are fitted to; and the encoder that writes
//! the stream, a frame behind, carrying the real thing. They code the same
//! audio into the same bits, which is checked on every frame: the stream's
//! core is the one the gains were fitted to, or nothing is written.
//!
//! Each element is one of the master's objects or one of its bed channels,
//! the LFE excepted, which goes to the core's own: at most fifteen, what the
//! reference streams carry. A master with more needs its objects clustered
//! into fifteen first, which is not done here yet.

use hz_ac3::{DELAY, Encoder, FRAME_SAMPLES};
use hz_core::bits::BitWriter;
use hz_core::{Error, Result};
use hz_joc::estimate::Estimator;
use hz_meta::emdf;
use hz_meta::oamd::{Block, Gain, Object, ObjectAudioMetadata, Position, Ramp, Render};
use hz_programme::path::Path as Trajectory;
use std::path::{Path, PathBuf};

/// Elements a payload carries at most, as the reference streams do.
pub const MAX_ELEMENTS: usize = 15;
/// The most bytes a skip field holds.
const SKIP_LIMIT: usize = 511;
/// Payload bytes the Evolution frame's size allows each element: the
/// reference streams' run about eleven on average and twenty at most; past
/// this, the payload's budget quantises gains coarser.
const ROOM_PER_ELEMENT: usize = 16;
/// The core's channels joint object coding reconstructs from, in WAV order:
/// L, R, C, Ls, Rs.
const CORE: [usize; 5] = [0, 1, 2, 4, 5];

/// One element: an object, which moves, or a bed channel, which does not.
pub enum Element {
    Object {
        channel: usize,
        path: Trajectory,
        hint: usize,
    },
    Bed {
        channel: usize,
        position: [f64; 3],
    },
}

impl Element {
    fn channel(&self) -> usize {
        match self {
            Self::Object { channel, .. } | Self::Bed { channel, .. } => *channel,
        }
    }

    /// Where it is, and how loud, at sample `t`.
    fn state(&mut self, t: u64) -> ([f64; 3], f64) {
        match self {
            Self::Object { path, hint, .. } => {
                let state = path.segment(t, hint).at(t);
                (state.position, state.gain)
            }
            Self::Bed { position, .. } => (*position, 1.0),
        }
    }
}

/// The two encoders and the estimator, and what passes between them.
pub struct Writer {
    probe: Encoder,
    real: Encoder,
    estimator: Estimator,
    elements: Vec<Element>,
    lfe: bool,
    /// Every Evolution frame's bytes, and the probe's stand-in for one.
    size: usize,
    placeholder: Vec<u8>,
    /// Per element, the last [`DELAY`] samples of the frame before and this
    /// frame's: the first [`FRAME_SAMPLES`] are on the decoded core's clock.
    signals: Vec<Vec<f32>>,
    /// The last frame of core that went in, for the stream's encoder a frame
    /// behind, and what the probe decoded of it.
    core: Vec<Vec<f32>>,
    probed: Vec<Vec<f32>>,
    frames_in: u64,
    written: u64,
    scratch: Vec<u8>,
    out: PathBuf,
}

impl Writer {
    pub fn new(
        config: hz_ac3::Config,
        elements: Vec<Element>,
        lfe: bool,
        out: &Path,
    ) -> Result<Self> {
        let count = elements.len();
        if count == 0 || count > MAX_ELEMENTS {
            return Err(Error::unsupported(
                out,
                format!(
                    "{count} elements for joint object coding, which carries one to \
                     {MAX_ELEMENTS}; a master with more needs its objects clustered first"
                ),
            ));
        }
        let encoder = || {
            let mut encoder =
                Encoder::new(config).map_err(|e| Error::unsupported(out, e.0.clone()))?;
            encoder.reconstruct();
            Ok::<_, Error>(encoder)
        };
        let (probe, real) = (encoder()?, encoder()?);
        let channels = probe.channels();
        let mut writer = Self {
            probe,
            real,
            estimator: Estimator::new(count, 0),
            elements,
            lfe,
            size: 0,
            placeholder: Vec::new(),
            signals: vec![vec![0.0; DELAY + FRAME_SAMPLES]; count],
            core: vec![vec![0.0; FRAME_SAMPLES]; channels],
            probed: vec![vec![0.0; FRAME_SAMPLES]; channels],
            frames_in: 0,
            written: 0,
            scratch: Vec::new(),
            out: out.to_path_buf(),
        };
        // Room for the payloads the references carry, and never less than
        // the smallest one these elements can make, so that the budget can
        // always bring a payload under it.
        let overhead = writer.overhead(0)?;
        let floor = overhead + smallest_payload(count);
        writer.size = (overhead + ROOM_PER_ELEMENT * count + 8).max(floor);
        if writer.size > SKIP_LIMIT {
            if floor > SKIP_LIMIT {
                return Err(Error::unsupported(
                    out,
                    format!(
                        "an Evolution frame of at least {floor} bytes for {count} elements, past \
                         the {SKIP_LIMIT} a skip field holds"
                    ),
                ));
            }
            writer.size = SKIP_LIMIT;
        }
        writer.placeholder = vec![0; writer.size];
        Ok(writer)
    }

    /// The stream's encoder, for what it reports.
    pub fn encoder(&self) -> &Encoder {
        &self.real
    }

    /// Syncframes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Every Evolution frame's bytes.
    pub fn evolution_bytes(&self) -> usize {
        self.size
    }

    /// Payloads the budget made smaller.
    pub fn squeezed(&self) -> usize {
        self.estimator.squeezed()
    }

    /// One frame of input: its core, planar in WAV order, and the master's
    /// samples it was rendered from, `got` of them at sample `at`. Appends
    /// to `out` the syncframe of the frame before, once there is one.
    pub fn frame(
        &mut self,
        core: &[&[f32]],
        input: &[f32],
        stride: usize,
        got: usize,
        at: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        for (element, signal) in self.elements.iter_mut().zip(&mut self.signals) {
            signal.copy_within(FRAME_SAMPLES.., 0);
            let channel = element.channel();
            for n in 0..FRAME_SAMPLES {
                signal[DELAY + n] = if n < got {
                    let (_, gain) = element.state(at + n as u64);
                    (f64::from(input[n * stride + channel]) * gain) as f32
                } else {
                    0.0
                };
            }
        }

        self.probe.carry_skip_field(&self.placeholder);
        self.scratch.clear();
        self.probe
            .encode(core, &mut self.scratch)
            .map_err(|e| Error::refused(&self.out, e.0))?;
        // The payload this frame's push hands back is the last frame's.
        if self.frames_in > 0 {
            self.estimator.budget = self.room(self.frames_in - 1)?;
        }
        let decoded: Vec<&[f32]> = CORE
            .iter()
            .map(|&ch| self.probe.decoded(ch).expect("reconstructing"))
            .collect();
        let objects: Vec<&[f32]> = self.signals.iter().map(|s| &s[..FRAME_SAMPLES]).collect();
        let present = vec![true; objects.len()];
        if let Some(joc) = self.estimator.push(&decoded, &objects, &present) {
            self.write(&joc.to_bytes(), out)?;
        }
        for (ch, (kept, probed)) in self.core.iter_mut().zip(&mut self.probed).enumerate() {
            kept.copy_from_slice(core[ch]);
            probed.copy_from_slice(self.probe.decoded(ch).expect("reconstructing"));
        }
        self.frames_in += 1;
        Ok(())
    }

    /// The last frame's syncframe, appended to `out`.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if self.frames_in == 0 {
            return Ok(());
        }
        self.estimator.budget = self.room(self.frames_in - 1)?;
        if let Some(joc) = self.estimator.finish() {
            self.write(&joc.to_bytes(), out)?;
        }
        Ok(())
    }

    /// Stream frame `written` with its payload: the core that went in a frame
    /// ago, carrying its Evolution frame, checked against what the probe made
    /// of it.
    fn write(&mut self, joc: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let frame = self.written;
        let oamd = self.oamd(frame)?;
        let mut evolution = evolution_frame(&oamd, joc);
        if evolution.len() > self.size {
            return Err(Error::refused(
                &self.out,
                format!(
                    "syncframe {frame}: an Evolution frame of {} bytes, past the {} every frame \
                     is given",
                    evolution.len(),
                    self.size
                ),
            ));
        }
        pad(&mut evolution, self.size);
        self.real.carry_skip_field(&evolution);
        let core: Vec<&[f32]> = self.core.iter().map(Vec::as_slice).collect();
        self.real
            .encode(&core, out)
            .map_err(|e| Error::refused(&self.out, e.0))?;
        for (ch, probed) in self.probed.iter().enumerate() {
            if self.real.decoded(ch).expect("reconstructing") != probed.as_slice() {
                return Err(Error::refused(
                    &self.out,
                    format!(
                        "syncframe {frame}: the core written is not the core the gains were \
                         fitted to"
                    ),
                ));
            }
        }
        self.written += 1;
        Ok(())
    }

    /// The payload bytes stream frame `frame` leaves joint object coding.
    fn room(&mut self, frame: u64) -> Result<usize> {
        Ok(self.size.saturating_sub(self.overhead(frame)?))
    }

    /// What stream frame `frame`'s Evolution frame takes besides its joint
    /// object coding payload, the payload's size field at its longest.
    fn overhead(&mut self, frame: u64) -> Result<usize> {
        let placeholder = [0u8; 256];
        Ok(evolution_frame(&self.oamd(frame)?, &placeholder).len() - placeholder.len())
    }

    /// Stream frame `frame`'s object metadata: one block, each element moving
    /// across the frame to where it is at the frame's end — on the decoded
    /// core's clock, [`DELAY`] behind the master's — as the reference streams
    /// move theirs.
    fn oamd(&mut self, frame: u64) -> Result<Vec<u8>> {
        let t = ((frame + 1) * FRAME_SAMPLES as u64).saturating_sub(DELAY as u64);
        let mut objects = Vec::with_capacity(self.elements.len() + 1);
        if self.lfe {
            objects.push(Object {
                active: true,
                gain: Gain::Unity,
                render: None,
            });
        }
        for element in &mut self.elements {
            let (position, _) = element.state(t);
            objects.push(Object {
                active: true,
                gain: Gain::Unity,
                render: Some(Render {
                    position: Position::from_master(position),
                    elevation: position[2].abs() > f64::EPSILON,
                    ..Render::default()
                }),
            });
        }
        ObjectAudioMetadata {
            lfe: self.lfe,
            blocks: vec![Block {
                offset: 0,
                ramp: Ramp::Long,
                objects,
            }],
        }
        .write()
        .map_err(|e| Error::unsupported(&self.out, e.to_string()))
    }
}

/// The bytes of the smallest payload `count` elements can make: every one
/// stepped, which takes a slot's five bits more, and every code nought.
fn smallest_payload(count: usize) -> usize {
    let object = hz_joc::Object {
        bands_index: hz_joc::estimate::BANDS_INDEX,
        fine: true,
        steep: true,
        offsets: vec![0],
        data: hz_joc::Data::Dense(vec![vec![vec![0; hz_joc::estimate::BANDS]; CORE.len()]]),
    };
    hz_joc::Joc {
        downmix_config: 0,
        clip_gain: (0, 0),
        sequence: 0,
        objects: vec![Some(object); count],
        unflagged_tail: 0,
        padding: 0,
        trailing: vec![],
    }
    .to_bytes()
    .len()
}

/// An Evolution frame behind its sync word, carrying `oamd` and `joc` as the
/// reference streams carry theirs: group nought, kept by a decoder that does
/// not know them, the joint object coding frame-aligned; protected as they
/// are.
fn evolution_frame(oamd: &[u8], joc: &[u8]) -> Vec<u8> {
    let retain = |frame_aligned| {
        Some(emdf::Retain {
            frame_aligned,
            priority: 0,
            processing: 0,
        })
    };
    let config = |frame_aligned| emdf::Config {
        duration: None,
        group: Some(0),
        codec_data: None,
        retain: retain(frame_aligned),
    };
    let payloads = [
        emdf::Payload {
            id: emdf::OAMD,
            bytes: oamd,
            sample_offset: 0,
            config: config(None),
        },
        emdf::Payload {
            id: emdf::JOC,
            bytes: joc,
            sample_offset: 0,
            config: config(Some((false, false))),
        },
    ];
    let mut container = BitWriter::new();
    emdf::write(&mut container, &payloads, emdf::Protection::EAC3);
    let container = container.finish();
    let mut synced = emdf::SYNC_WORD.to_be_bytes().to_vec();
    synced.extend_from_slice(&(container.len() as u16).to_be_bytes());
    synced.extend_from_slice(&container);
    synced
}

/// Pad an Evolution frame behind its sync word to `size` bytes, its length
/// field saying so: the container's tail past its last field is nought, and
/// a decoder reads to its length (TS 102 366 H.2.2.1.2).
fn pad(frame: &mut Vec<u8>, size: usize) {
    frame.resize(size, 0);
    let length = (size - 4) as u16;
    frame[2..4].copy_from_slice(&length.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    }

    /// The Evolution frame reads back as the reference streams' do: the
    /// object metadata then the joint object coding, group nought, kept, the
    /// second frame-aligned — padded or not.
    #[test]
    fn an_evolution_frame_reads_back_as_the_references_carry_theirs() {
        let (oamd, joc) = (vec![1u8, 2, 3], vec![4u8, 5, 6, 7]);
        let mut frame = evolution_frame(&oamd, &joc);
        for padded in [false, true] {
            if padded {
                let size = frame.len() + 40;
                pad(&mut frame, size);
            }
            let (read, _) = emdf::read_synced(&frame, 0).expect("an Evolution frame");
            let ids: Vec<u32> = read.payloads.iter().map(|p| p.id).collect();
            assert_eq!(ids, [emdf::OAMD, emdf::JOC]);
            assert_eq!(read.payloads[0].bytes, oamd);
            assert_eq!(read.payloads[1].bytes, joc);
            for (payload, aligned) in read.payloads.iter().zip([None, Some((false, false))]) {
                assert_eq!(payload.sample_offset, None);
                assert_eq!(payload.config.group, Some(0));
                let retain = payload.config.retain.expect("kept");
                assert_eq!(retain.frame_aligned, aligned);
            }
        }
    }

    /// One syncframe for every frame that went in, each carrying an
    /// Evolution frame of the one size, whose object metadata names the
    /// elements and whose payload carries them — and the stream's core the
    /// one the gains were fitted to, which the writer checks on every frame.
    #[test]
    fn the_writer_writes_a_syncframe_a_frame_carrying_its_objects() {
        const FRAMES: usize = 6;
        let config = hz_ac3::Config {
            format: hz_ac3::Format::Eac3,
            sample_rate: 48_000,
            mode: hz_ac3::ChannelMode::ThreeTwo,
            lfe: true,
            bit_rate: Some(448),
            coupling: true,
            metadata: hz_ac3::Metadata::default(),
        };
        let elements = vec![
            Element::Bed {
                channel: 0,
                position: [-1.0, 1.0, 0.0],
            },
            Element::Bed {
                channel: 1,
                position: [1.0, 1.0, 0.0],
            },
        ];
        let out = PathBuf::from("test.ec3");
        let mut writer = Writer::new(config, elements, true, &out).unwrap();
        let mut seed = 9u32;
        let mut stream = Vec::new();
        for k in 0..FRAMES {
            // The two elements, and a core with them on L and R.
            let input: Vec<f32> = (0..2 * FRAME_SAMPLES)
                .map(|_| 0.3 * noise(&mut seed))
                .collect();
            let core: Vec<Vec<f32>> = (0..6)
                .map(|ch| {
                    (0..FRAME_SAMPLES)
                        .map(|n| match ch {
                            0 | 1 => input[n * 2 + ch],
                            _ => 0.05 * noise(&mut seed),
                        })
                        .collect()
                })
                .collect();
            let refs: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
            writer
                .frame(
                    &refs,
                    &input,
                    2,
                    FRAME_SAMPLES,
                    (k * FRAME_SAMPLES) as u64,
                    &mut stream,
                )
                .unwrap();
        }
        writer.finish(&mut stream).unwrap();
        assert_eq!(writer.written(), FRAMES as u64);

        // Every Evolution frame, found by its sync word: a skip field is not
        // byte-aligned in the frame, so at each of the eight bit alignments.
        let size = writer.evolution_bytes();
        let sync = emdf::SYNC_WORD.to_be_bytes();
        let mut sequences = std::collections::BTreeSet::new();
        for shift in 0..8 {
            let bytes: Vec<u8> = stream
                .windows(2)
                .map(|w| {
                    if shift == 0 {
                        w[0]
                    } else {
                        (w[0] << shift) | (w[1] >> (8 - shift))
                    }
                })
                .collect();
            let mut at = 0;
            while let Some(offset) = bytes[at..].windows(2).position(|w| w == sync) {
                let start = at + offset;
                at = start + 2;
                let Some((frame, _)) = emdf::read_synced(&bytes, start) else {
                    continue;
                };
                let ids: Vec<u32> = frame.payloads.iter().map(|p| p.id).collect();
                if ids != [emdf::OAMD, emdf::JOC] {
                    continue;
                }
                let length = usize::from(u16::from_be_bytes([bytes[start + 2], bytes[start + 3]]));
                assert_eq!(length + 4, size, "every Evolution frame the one size");
                let metadata = ObjectAudioMetadata::read(&frame.payloads[0].bytes).unwrap();
                assert!(metadata.lfe);
                assert_eq!(
                    metadata.blocks[0].objects.len(),
                    3,
                    "the LFE and two elements"
                );
                let joc = hz_joc::read(&frame.payloads[1].bytes).unwrap();
                assert_eq!(joc.objects.iter().flatten().count(), 2);
                sequences.insert(joc.sequence);
                at = start + size;
            }
        }
        let expected: std::collections::BTreeSet<u16> = (0..FRAMES as u16).collect();
        assert_eq!(sequences, expected, "one Evolution frame each syncframe");
    }

    /// The smallest payload is the floor the budget can always reach: a
    /// payload made under a budget nothing fits is no larger.
    #[test]
    fn the_smallest_payload_bounds_what_the_budget_makes() {
        for count in [1, 5, MAX_ELEMENTS] {
            let mut estimator = Estimator::new(count, 0);
            estimator.budget = 1;
            let mut seed = 3u32;
            let core: Vec<Vec<f32>> = (0..5)
                .map(|_| (0..FRAME_SAMPLES).map(|_| noise(&mut seed)).collect())
                .collect();
            let objects: Vec<Vec<f32>> = (0..count)
                .map(|_| (0..FRAME_SAMPLES).map(|_| noise(&mut seed)).collect())
                .collect();
            let core: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
            let objects: Vec<&[f32]> = objects.iter().map(Vec::as_slice).collect();
            let present = vec![true; count];
            for _ in 0..3 {
                if let Some(joc) = estimator.push(&core, &objects, &present) {
                    assert!(joc.to_bytes().len() <= smallest_payload(count));
                }
            }
            assert!(smallest_payload(count) < SKIP_LIMIT);
        }
    }
}
