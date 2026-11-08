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
//! reference streams carry. A master with more has them folded into fifteen
//! by `hz-cluster`, a syncframe at a time ([`Folded`]); the core is the
//! master's whole render either way.

use crate::lane::{Lane, Stage};
use hz_ac3::{DELAY, Encoder, FRAME_SAMPLES};
use hz_analysis::truepeak::{self, TruePeak};
use hz_core::bits::BitWriter;
use hz_core::{Error, Result};
use hz_joc::estimate::Estimator;
use hz_meta::emdf;
use hz_meta::oamd::{Block, Gain, Object, ObjectAudioMetadata, Position, Ramp, Render};
use hz_meta::programme::{Information, Loudness, Preprocessing, true_peak_code};
use hz_programme::fold::{self, Folder, Member};
use hz_programme::mix::{Headroom, Mixing};
use hz_programme::path::{self, Path as Trajectory};
use hz_render::{Keyframe, Mode};
use std::path::{Path, PathBuf};

/// Elements a payload carries at most, as the reference streams do.
pub const MAX_ELEMENTS: usize = 15;
/// The most bytes a skip field holds.
const SKIP_LIMIT: usize = 511;
/// Payload bytes the Evolution frame's size allows each element: the
/// reference streams' run about eleven on average and twenty at most; past
/// this, the payload's budget quantises gains coarser.
const ROOM_PER_ELEMENT: usize = 16;
/// Samples a decoder's objects lag the master by: the core's delay, then
/// joint object coding's own (its QMF banks'). The stream has to run this far
/// past the master's end, and the object metadata is timed on this clock.
pub const LATENCY: usize = DELAY + hz_joc::qmf::DELAY;
/// The core's channels joint object coding reconstructs from, in WAV order:
/// L, R, C, Ls, Rs.
const CORE: [usize; 5] = [0, 1, 2, 4, 5];

/// One element: an object, which moves, or a bed channel, which does not.
pub enum Element {
    Object {
        channel: usize,
        path: Trajectory,
        hint: usize,
        /// Its updates, the next one due and the last one in force: what
        /// its size and rendering are, which do not ramp.
        keyframes: Vec<Keyframe>,
        next: usize,
        current: Keyframe,
    },
    Bed {
        channel: usize,
        position: [f64; 3],
    },
}

impl Element {
    /// An object, from its updates.
    pub fn object(channel: usize, keyframes: Vec<Keyframe>) -> Self {
        Self::Object {
            channel,
            path: Trajectory::new(&keyframes),
            hint: 0,
            current: keyframes.first().copied().unwrap_or_default(),
            keyframes,
            next: 0,
        }
    }

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

    /// Where it is at sample `t`, rendered as the master says: an object's
    /// size, and whether it snaps, which zones confine it, how it follows
    /// the screen and whether its height counts.
    fn render(&mut self, t: u64) -> Render {
        let (position, _) = self.state(t);
        match self {
            Self::Object {
                keyframes,
                next,
                current,
                ..
            } => {
                path::advance(keyframes, next, current, t);
                Render {
                    size: crate::encode::size_of(current.spread),
                    ..rendered(position, current.mode)
                }
            }
            Self::Bed { .. } => rendered(position, Mode::default()),
        }
    }
}

/// An element at `position` rendering by `mode`: its height counted where
/// the mode counts it and it has any.
fn rendered(position: [f64; 3], mode: Mode) -> Render {
    Render {
        position: Position::from_master(position),
        elevation: mode.elevation && position[2].abs() > f64::EPSILON,
        zones: mode.zones,
        screen: mode.screen,
        snap: mode.snap,
        ..Render::default()
    }
}

/// The elements, and the encoders that carry them.
pub struct Writer {
    feed: Feed,
    coder: Coder,
}

/// Where the elements come from.
enum Feed {
    /// Each one of the master's, as it is.
    Direct(Box<Direct>),
    /// More of them than a payload carries: folded into fifteen.
    Folded(Box<Folding>),
}

/// The master's elements carried as they are.
struct Direct {
    elements: Vec<Element>,
    /// Per element, the last [`DELAY`] samples of the frame before and this
    /// frame's: the first [`FRAME_SAMPLES`] are on the decoded core's clock.
    signals: Vec<Vec<f32>>,
    renders: Vec<Render>,
}

/// The master's elements folded into [`MAX_ELEMENTS`] by `hz-cluster`, a
/// syncframe a block, the blocks on the clock of the objects a decoder
/// plays, [`LATENCY`] behind the master: each block is the span of the
/// master a frame's objects come out as, the span its object metadata ramps
/// their positions across, so the weights ramp across the same span to the
/// same positions. The estimator takes the elements on the core's clock,
/// [`hz_joc::qmf::DELAY`] into a block and on into the next, so a frame is
/// coded once the block after its own is folded; and the fold places a
/// block once it has read the blocks its window reaches ahead. So the core
/// is held back with the block it was rendered beside.
struct Folded {
    /// Each block carrying the core rendered beside it, planar.
    folder: Folder<Vec<Vec<f32>>>,
    /// The master's last [`LATENCY`] samples of the frame before and this
    /// frame's, interleaved: its first [`FRAME_SAMPLES`] are the block.
    master: Vec<f32>,
    /// Frames read.
    frames: u64,
    /// The block folded last, held until the one after it is, and its
    /// elements: per element, its samples and then the next block's.
    held: Option<fold::Block<Vec<Vec<f32>>>>,
    elements: Vec<Vec<f32>>,
    renders: Vec<Render>,
    /// Frames the coder is done with, to make the next blocks into.
    spare: Vec<Job>,
}

/// The fold on a thread of its own where one can be started ([`Lane`]), a
/// frame ahead of the coder: a frame is read and folded while the blocks the
/// frame before made ready are coded. The fold sees its frames in order, and
/// the coder its blocks, so that both are what a single thread makes of them.
struct Folding {
    lane: Lane<Folded>,
    /// Frames handed to the fold, and frames whose blocks were coded.
    sent: u64,
    taken: u64,
    /// Frames of input the fold is done with, for the next ones to go in.
    spare: Vec<Input>,
}

/// A frame of input for the fold: its core, planar, and the master's
/// samples it was rendered from, `got` of them, interleaved `stride` wide.
#[derive(Default)]
struct Input {
    core: Vec<Vec<f32>>,
    master: Vec<f32>,
    stride: usize,
    got: usize,
}

/// What the fold is handed, with the frames the coder is done with: a frame
/// of input, or the programme's end.
enum Fold {
    Frame { input: Input, spare: Vec<Job> },
    Last { spare: Vec<Job> },
}

/// What the fold hands back: the blocks it made ready for the coder, in
/// order, and what it failed at after them, if it failed; and the input it
/// is done with.
struct Folds {
    jobs: Vec<Job>,
    failed: Option<Error>,
    spent: Option<Input>,
}

/// Frames from one statement of the true peak to the next, as the reference
/// streams state theirs.
const TRUE_PEAK_EVERY: u64 = 16;

/// The estimator, and the two encoders around it, each on a thread of its
/// own where one can be started ([`Lane`]): the probe, coding a frame's core
/// while the gains of the frame before are fitted; and the stream's encoder,
/// writing the frame before that. Each sees its frames in order and keeps its
/// own state from one to the next, so that the stream is the one a single
/// thread writes, to the byte.
struct Coder {
    probe: Lane<Probe>,
    stream: Lane<Stream>,
    estimator: Estimator,
    elements: usize,
    lfe: bool,
    /// Every Evolution frame's bytes.
    size: usize,
    information: Vec<u8>,
    /// How each element renders at the end of the frame fitted last: its
    /// syncframe's object metadata, and so the room its payload has.
    renders: Vec<Render>,
    /// Frames handed to the probe, frames whose gains were fitted, and
    /// syncframes the stream is still writing and has written.
    probed: u64,
    fitted: u64,
    writing: u64,
    written: u64,
    /// Every element is in every frame.
    present: Vec<bool>,
    /// Frames the stream is done with, for the next ones to go in.
    spare: Vec<Job>,
    channels: usize,
    trace: Option<Trace>,
    out: PathBuf,
}

/// A frame on its way through the coder: the core that went in, what the
/// probe decoded of it, and the elements' samples and renders its gains are
/// fitted from and its object metadata states.
struct Job {
    core: Vec<Vec<f32>>,
    decoded: Vec<Vec<f32>>,
    signals: Vec<Vec<f32>>,
    renders: Vec<Render>,
}

impl Job {
    fn new(channels: usize, elements: usize) -> Self {
        Self {
            core: vec![vec![0.0; FRAME_SAMPLES]; channels],
            decoded: vec![vec![0.0; FRAME_SAMPLES]; channels],
            signals: vec![vec![0.0; FRAME_SAMPLES]; elements],
            renders: vec![Render::default(); elements],
        }
    }
}

/// The encoder ahead of the stream's: it codes each frame first, carrying a
/// placeholder of the Evolution frame's size, so that its core is the one
/// the stream's encoder will code, and decodes it for the gains to be fitted
/// to.
struct Probe {
    encoder: Encoder,
    placeholder: Vec<u8>,
    scratch: Vec<u8>,
    out: PathBuf,
}

/// The encoder that writes the stream, a frame behind the estimator, and the
/// meter of the true peak the stream's loudness states.
struct Stream {
    encoder: Encoder,
    /// The frame that went in last, written once its payload comes.
    held: Option<Job>,
    written: u64,
    lfe: bool,
    size: usize,
    information: Vec<u8>,
    /// The true peak of the core since it was last stated, and the core
    /// interleaved for it.
    peak: TruePeak,
    interleaved: Vec<f32>,
    out: PathBuf,
}

/// What the stream is handed: a frame, with the payload of the frame before
/// it once there is one; or, at the end, the last frame's payload.
enum Write {
    Frame { job: Job, joc: Option<Vec<u8>> },
    Last { joc: Option<Vec<u8>> },
}

/// What the stream hands back: the syncframe it wrote, if it wrote one, and
/// the frame it is done with.
struct Written {
    syncframe: Option<Result<Vec<u8>>>,
    spent: Option<Job>,
}

impl Stage for Probe {
    type In = Job;
    type Out = Result<Job>;

    fn run(&mut self, mut job: Job) -> Result<Job> {
        self.encoder.carry_skip_field(&self.placeholder);
        self.scratch.clear();
        let core: Vec<&[f32]> = job.core.iter().map(Vec::as_slice).collect();
        self.encoder
            .encode(&core, &mut self.scratch)
            .map_err(|e| Error::refused(&self.out, e.0))?;
        for (ch, decoded) in job.decoded.iter_mut().enumerate() {
            decoded.copy_from_slice(self.encoder.decoded(ch).expect("reconstructing"));
        }
        Ok(job)
    }
}

impl Stage for Stream {
    type In = Write;
    type Out = Written;

    fn run(&mut self, write: Write) -> Written {
        let channels = self.encoder.channels();
        let latency = truepeak::LATENCY * channels;
        match write {
            Write::Frame { job, joc } => {
                // The meter closes the frame before's interval when it is
                // written, and its reconstruction runs behind: so the first
                // samples of this frame go in first, and the rest after.
                for (ch, plane) in job.core.iter().enumerate() {
                    for (n, &sample) in plane.iter().enumerate() {
                        self.interleaved[n * channels + ch] = sample;
                    }
                }
                self.peak.push(&self.interleaved[..latency]);
                let syncframe = joc.map(|joc| self.write(&joc));
                self.peak.push(&self.interleaved[latency..]);
                Written {
                    syncframe,
                    spent: self.held.replace(job),
                }
            }
            Write::Last { joc } => {
                // Past the programme's end is silence: the meter's
                // reconstruction of the last frame runs out into it.
                self.interleaved[..latency].fill(0.0);
                self.peak.push(&self.interleaved[..latency]);
                let syncframe = joc.map(|joc| self.write(&joc));
                Written {
                    syncframe,
                    spent: self.held.take(),
                }
            }
        }
    }
}

impl Stream {
    /// Stream frame `written` with its payload: the frame held, carrying its
    /// Evolution frame, checked against what the probe made of it.
    fn write(&mut self, joc: &[u8]) -> Result<Vec<u8>> {
        let frame = self.written;
        let held = self
            .held
            .as_ref()
            .expect("a frame goes in before its payload");
        let oamd = oamd(self.lfe, &held.renders, &self.out)?;
        let loudness = loudness(frame, Some(&mut self.peak));
        let mut evolution = evolution_frame(&loudness, &self.information, &oamd, joc);
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
        self.encoder.carry_skip_field(&evolution);
        let core: Vec<&[f32]> = held.core.iter().map(Vec::as_slice).collect();
        let mut syncframe = Vec::new();
        self.encoder
            .encode(&core, &mut syncframe)
            .map_err(|e| Error::refused(&self.out, e.0))?;
        for (ch, probed) in held.decoded.iter().enumerate() {
            if self.encoder.decoded(ch).expect("reconstructing") != probed.as_slice() {
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
        Ok(syncframe)
    }
}

/// `HZ_JOC_TRACE=<dir>`: what the stream was meant to carry, for a decoder's
/// objects to be held to — `element-<n>.f32`, each element's samples on the
/// master's clock, little-endian; and `positions.csv`, a line a syncframe of
/// where each element is at the end of its block, as the object metadata
/// codes it — which `xtask joc-check --elements` reads.
struct Trace {
    files: Vec<std::io::BufWriter<std::fs::File>>,
    positions: std::io::BufWriter<std::fs::File>,
    /// Samples still to leave out: the decoded core's clock starts
    /// [`DELAY`] before the master's.
    skip: usize,
}

impl Trace {
    fn open(elements: usize) -> Result<Option<Self>> {
        let Some(dir) = std::env::var_os("HZ_JOC_TRACE") else {
            return Ok(None);
        };
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        let files = (0..elements)
            .map(|n| {
                let path = dir.join(format!("element-{n}.f32"));
                std::fs::File::create(&path)
                    .map(std::io::BufWriter::new)
                    .map_err(|e| Error::io(&path, e))
            })
            .collect::<Result<_>>()?;
        let path = dir.join("positions.csv");
        let positions = std::fs::File::create(&path)
            .map(std::io::BufWriter::new)
            .map_err(|e| Error::io(&path, e))?;
        Ok(Some(Self {
            files,
            positions,
            skip: DELAY,
        }))
    }

    fn frame(&mut self, signals: &[&[f32]], renders: &[Render]) -> Result<()> {
        use std::io::Write;
        let fail = |e| Error::io(Path::new("HZ_JOC_TRACE"), e);
        let line: Vec<String> = renders
            .iter()
            .flat_map(|render| render.position.to_master())
            .map(|x| format!("{x}"))
            .collect();
        writeln!(self.positions, "{}", line.join(",")).map_err(fail)?;
        let skip = self.skip.min(FRAME_SAMPLES);
        for (file, signal) in self.files.iter_mut().zip(signals) {
            let bytes: Vec<u8> = signal[skip..FRAME_SAMPLES]
                .iter()
                .flat_map(|x| x.to_le_bytes())
                .collect();
            file.write_all(&bytes).map_err(fail)?;
        }
        self.skip -= skip;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        use std::io::Write;
        for file in &mut self.files {
            file.flush()
                .map_err(|e| Error::io(Path::new("HZ_JOC_TRACE"), e))?;
        }
        self.positions
            .flush()
            .map_err(|e| Error::io(Path::new("HZ_JOC_TRACE"), e))?;
        Ok(())
    }
}

impl Writer {
    /// A writer for the master's `elements`, its LFE carried by the core's
    /// own when `lfe`. More than [`MAX_ELEMENTS`] are folded into that many,
    /// at `config`'s sample rate and dialnorm — see [`Folded`].
    /// `downmix`: the `dmixtyp` the programme information declares for the
    /// fold that made the core ([`downmix_type`]). Its encoders, and the fold,
    /// run on threads of their own when `threads` and they can be started, on
    /// the caller's otherwise.
    pub fn new(
        config: hz_ac3::Config,
        elements: Vec<Element>,
        lfe: bool,
        downmix: Option<u8>,
        out: &Path,
        threads: bool,
    ) -> Result<Self> {
        let count = elements.len();
        if count == 0 {
            return Err(Error::unsupported(
                out,
                "no elements for joint object coding to carry",
            ));
        }
        let (feed, carried) = if count <= MAX_ELEMENTS {
            let feed = Feed::Direct(Box::new(Direct {
                elements,
                signals: vec![vec![0.0; DELAY + FRAME_SAMPLES]; count],
                renders: vec![Render::default(); count],
            }));
            (feed, count)
        } else {
            let folded = Folded::new(&config, elements, out)?;
            let feed = Feed::Folded(Box::new(Folding {
                lane: Lane::new(folded, "joc-fold", threads),
                sent: 0,
                taken: 0,
                spare: Vec::new(),
            }));
            (feed, MAX_ELEMENTS)
        };
        Ok(Self {
            feed,
            coder: Coder::new(config, carried, lfe, downmix, out, threads)?,
        })
    }

    /// The stream's encoder, for what it reports, once the writer is
    /// finished.
    pub fn encoder(&self) -> &Encoder {
        let stream = self.coder.stream.stage();
        &stream.expect("the writer is finished").encoder
    }

    /// Syncframes written so far.
    pub fn written(&self) -> u64 {
        self.coder.written
    }

    /// Elements the stream carries.
    pub fn elements(&self) -> usize {
        self.coder.elements
    }

    /// Every Evolution frame's bytes.
    pub fn evolution_bytes(&self) -> usize {
        self.coder.size
    }

    /// Payloads the budget made smaller.
    pub fn squeezed(&self) -> usize {
        self.coder.estimator.squeezed()
    }

    /// The fold, when the master's elements were folded, once the writer is
    /// finished.
    pub fn folder(&self) -> Option<&Folder<Vec<Vec<f32>>>> {
        match &self.feed {
            Feed::Direct(_) => None,
            Feed::Folded(folding) => {
                let folded = folding.lane.stage().expect("the writer is finished");
                Some(&folded.folder)
            }
        }
    }

    /// One frame of input: its core, planar in WAV order, and the master's
    /// samples it was rendered from, `got` of them at sample `at`. Appends
    /// to `out` the syncframe of a frame before, once there is one.
    pub fn frame(
        &mut self,
        core: &[&[f32]],
        input: &[f32],
        stride: usize,
        got: usize,
        at: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        match &mut self.feed {
            Feed::Direct(direct) => {
                direct.read(input, stride, got, at);
                let signals: Vec<&[f32]> =
                    direct.signals.iter().map(|s| &s[..FRAME_SAMPLES]).collect();
                self.coder.step(core, &signals, &direct.renders, out)
            }
            Feed::Folded(folding) => {
                let mut next = folding.spare.pop().unwrap_or_default();
                next.core.resize_with(core.len(), Vec::new);
                for (kept, plane) in next.core.iter_mut().zip(core) {
                    kept.clear();
                    kept.extend_from_slice(plane);
                }
                next.master.clear();
                next.master.extend_from_slice(&input[..got * stride]);
                (next.stride, next.got) = (stride, got);
                let spare = std::mem::take(&mut self.coder.spare);
                folding.lane.send(Fold::Frame { input: next, spare });
                folding.sent += 1;
                // The blocks the frame before made ready, coded while this
                // one is folded.
                if folding.sent - folding.taken > 1 {
                    let folds = folding.lane.recv();
                    folding.taken += 1;
                    self.coder.folded(folds, &mut folding.spare, out)?;
                }
                Ok(())
            }
        }
    }

    /// The frames still held, and the last frame's syncframe, appended to
    /// `out`.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if let Feed::Folded(folding) = &mut self.feed {
            let spare = std::mem::take(&mut self.coder.spare);
            folding.lane.send(Fold::Last { spare });
            folding.sent += 1;
            while folding.taken < folding.sent {
                let folds = folding.lane.recv();
                folding.taken += 1;
                self.coder.folded(folds, &mut folding.spare, out)?;
            }
            folding.lane.close();
        }
        self.coder.finish(out)
    }

    /// The error an encode stops at, met before its next frame reached the
    /// writer: `e`, once every syncframe the stream's encoder has been given
    /// a payload for is appended to `out`, as a single thread would have
    /// appended them before it met `e` — or the refusal of one of them,
    /// which it would have met first. The stream's encoder writes them on a
    /// thread of its own, and [`Self::frame`] takes only those already
    /// written; the frame it holds for the next payload stays unwritten, as
    /// on one thread.
    pub fn stop(&mut self, e: Error, out: &mut Vec<u8>) -> Error {
        match self.coder.settle(out) {
            Ok(()) => e,
            Err(first) => first,
        }
    }
}

impl Direct {
    /// This frame's samples of each element, on the decoded core's clock,
    /// and where each is at the end of the frame of objects a decoder plays.
    fn read(&mut self, input: &[f32], stride: usize, got: usize, at: u64) {
        // The metadata comes with its access unit, and so does the audio it
        // places: the objects', [`LATENCY`] behind the master.
        let end = (at + FRAME_SAMPLES as u64).saturating_sub(LATENCY as u64);
        for ((element, signal), render) in self
            .elements
            .iter_mut()
            .zip(&mut self.signals)
            .zip(&mut self.renders)
        {
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
            *render = element.render(end);
        }
    }
}

impl Folded {
    fn new(config: &hz_ac3::Config, elements: Vec<Element>, out: &Path) -> Result<Self> {
        // The bed channels are folded as any object is: pinning them would
        // keep an element each, which a 7.1.2 bed's nine would take most of
        // fifteen for, and the core already carries them where they are.
        let members = elements
            .into_iter()
            .map(|element| match element {
                Element::Object {
                    channel, keyframes, ..
                } => (channel, Member::classed(keyframes)),
                Element::Bed { channel, position } => (
                    channel,
                    Member::Bed {
                        position,
                        pinned: false,
                    },
                ),
            })
            .collect();
        // Nothing is rounded and nothing limited: the elements are never
        // coded as samples, only rebuilt from the core, which is clamped on
        // its own; a limiter would take them away from the scene the core
        // was rendered from.
        let mixing = Mixing {
            dialnorm: f64::from(config.metadata.dialnorm),
            headroom: Headroom::Off,
            ..Mixing::default()
        };
        let folder = Folder::new(
            members,
            MAX_ELEMENTS,
            f64::from(config.sample_rate),
            &mixing,
        )
        .map_err(|why| Error::unsupported(out, why.to_string()))?;
        Ok(Self {
            folder,
            master: Vec::new(),
            frames: 0,
            held: None,
            elements: vec![vec![0.0; 2 * FRAME_SAMPLES]; MAX_ELEMENTS],
            renders: vec![Render::default(); MAX_ELEMENTS],
            spare: Vec::new(),
        })
    }

    /// Read one frame: its core into the block's carry, and the master's
    /// samples into the block on the decoded objects' clock — the last
    /// [`LATENCY`] of the frame before and all but the last [`LATENCY`] of
    /// this.
    fn read(&mut self, core: &[&[f32]], input: &[f32], stride: usize, got: usize) {
        if self.master.is_empty() {
            self.master = vec![0.0; (LATENCY + FRAME_SAMPLES) * stride];
        }
        self.master.copy_within(FRAME_SAMPLES * stride.., 0);
        let fresh = &mut self.master[LATENCY * stride..];
        fresh[..got * stride].copy_from_slice(&input[..got * stride]);
        fresh[got * stride..].fill(0.0);

        let mut block = self.folder.block();
        block.carry.resize_with(core.len(), Vec::new);
        for (kept, plane) in block.carry.iter_mut().zip(core) {
            kept.clear();
            kept.extend_from_slice(plane);
        }
        let at = (self.frames * FRAME_SAMPLES as u64) as i64 - LATENCY as i64;
        let master = &self.master;
        self.folder.read(block, at, FRAME_SAMPLES, |_, n, channel| {
            f64::from(master[n * stride + channel])
        });
        self.frames += 1;
    }

    /// Fold the oldest block read: its elements are in the mixer's `mixed`,
    /// where each is at its end the folder's `positions`. Hand it back with
    /// `recycle`.
    fn fold(&mut self) -> Result<Option<fold::Block<Vec<Vec<f32>>>>> {
        // Nothing is limited, so there is no ceiling to hold.
        self.folder
            .fold(f64::INFINITY)
            .map_err(|why| Error::unsupported(Path::new("the fold"), why.to_string()))
    }

    /// Fold the oldest block, and make the block before it ready for the
    /// coder with the core it was read with, now that its elements run on
    /// into this one.
    fn code(&mut self, jobs: &mut Vec<Job>) -> Result<()> {
        let Some(block) = self.fold()? else {
            return Ok(());
        };
        for (element, mixed) in self.elements.iter_mut().zip(&self.folder.mixer.mixed) {
            element[FRAME_SAMPLES..].copy_from_slice(mixed);
        }
        self.ready(jobs);
        // This block's elements and metadata, held for the next.
        for element in &mut self.elements {
            element.copy_within(FRAME_SAMPLES.., 0);
        }
        for ((render, &position), &mode) in self
            .renders
            .iter_mut()
            .zip(self.folder.positions())
            .zip(self.folder.modes())
        {
            *render = rendered(position, mode);
        }
        if let Some(done) = self.held.replace(block) {
            self.folder.recycle(done);
        }
        Ok(())
    }

    /// Every block still held made ready for the coder: past the last there
    /// is nothing, so its elements run on into silence.
    fn end(&mut self, jobs: &mut Vec<Job>) -> Result<()> {
        while self.folder.waiting() > 0 {
            self.code(jobs)?;
        }
        for element in &mut self.elements {
            element[FRAME_SAMPLES..].fill(0.0);
        }
        self.ready(jobs);
        if let Some(done) = self.held.take() {
            self.folder.recycle(done);
        }
        Ok(())
    }

    /// The block held, if there is one, made ready for the coder: its core,
    /// and its elements on the core's clock, [`hz_joc::qmf::DELAY`] into
    /// them.
    fn ready(&mut self, jobs: &mut Vec<Job>) {
        let Some(held) = &self.held else {
            return;
        };
        const INTO: usize = hz_joc::qmf::DELAY;
        let mut job = self
            .spare
            .pop()
            .unwrap_or_else(|| Job::new(held.carry.len(), self.elements.len()));
        for (kept, plane) in job.core.iter_mut().zip(&held.carry) {
            kept.copy_from_slice(plane);
        }
        for (kept, element) in job.signals.iter_mut().zip(&self.elements) {
            kept.copy_from_slice(&element[INTO..INTO + FRAME_SAMPLES]);
        }
        job.renders.copy_from_slice(&self.renders);
        jobs.push(job);
    }
}

impl Stage for Folded {
    type In = Fold;
    type Out = Folds;

    fn run(&mut self, fold: Fold) -> Folds {
        let mut jobs = Vec::new();
        let (failed, spent) = match fold {
            Fold::Frame { input, spare } => {
                self.spare.extend(spare);
                let core: Vec<&[f32]> = input.core.iter().map(Vec::as_slice).collect();
                self.read(&core, &input.master, input.stride, input.got);
                let mut failed = None;
                while failed.is_none() && self.folder.ready() {
                    failed = self.code(&mut jobs).err();
                }
                (failed, Some(input))
            }
            Fold::Last { spare } => {
                self.spare.extend(spare);
                (self.end(&mut jobs).err(), None)
            }
        };
        Folds {
            jobs,
            failed,
            spent,
        }
    }
}

impl Coder {
    fn new(
        config: hz_ac3::Config,
        count: usize,
        lfe: bool,
        downmix: Option<u8>,
        out: &Path,
        threads: bool,
    ) -> Result<Self> {
        let encoder = || {
            let mut encoder =
                Encoder::new(config).map_err(|e| Error::unsupported(out, e.0.clone()))?;
            encoder.reconstruct();
            Ok::<_, Error>(encoder)
        };
        let (probe, real) = (encoder()?, encoder()?);
        let channels = probe.channels();
        let information = information(downmix);
        let renders = largest(count);
        // Room for the payloads the references carry, and never less than
        // the smallest one these elements can make, so that the budget can
        // always bring a payload under it. What the object metadata takes
        // depends on how the elements render — a size, a screen factor — and
        // the frame's size is the stream's, so it is measured with every
        // field that costs bits stated — the true peak among them.
        let overhead = overhead(TRUE_PEAK_EVERY - 1, lfe, &renders, &information, out)?;
        let floor = overhead + smallest_payload(count);
        let mut size = (overhead + ROOM_PER_ELEMENT * count + 8).max(floor);
        if size > SKIP_LIMIT {
            if floor > SKIP_LIMIT {
                return Err(Error::unsupported(
                    out,
                    format!(
                        "an Evolution frame of at least {floor} bytes for {count} elements, past \
                         the {SKIP_LIMIT} a skip field holds"
                    ),
                ));
            }
            size = SKIP_LIMIT;
        }
        let probe = Probe {
            encoder: probe,
            placeholder: vec![0; size],
            scratch: Vec::new(),
            out: out.to_path_buf(),
        };
        let stream = Stream {
            encoder: real,
            held: None,
            written: 0,
            lfe,
            size,
            information: information.clone(),
            peak: TruePeak::new(channels),
            interleaved: vec![0.0; channels * FRAME_SAMPLES],
            out: out.to_path_buf(),
        };
        Ok(Self {
            probe: Lane::new(probe, "joc-probe", threads),
            stream: Lane::new(stream, "joc-stream", threads),
            estimator: Estimator::new(count, 0),
            elements: count,
            lfe,
            size,
            information,
            renders,
            probed: 0,
            fitted: 0,
            writing: 0,
            written: 0,
            present: vec![true; count],
            spare: Vec::new(),
            channels,
            trace: Trace::open(count)?,
            out: out.to_path_buf(),
        })
    }

    /// One frame: its core, each element's samples on the decoded core's
    /// clock, and where each element is at the frame's end. Appends to `out`
    /// the syncframes the stream has written since, the first once there is
    /// one: the probe codes this frame while the gains of the one before are
    /// fitted, and the stream's encoder writes the one before that.
    fn step(
        &mut self,
        core: &[&[f32]],
        signals: &[&[f32]],
        renders: &[Render],
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let (channels, elements) = (self.channels, self.elements);
        let mut job = self
            .spare
            .pop()
            .unwrap_or_else(|| Job::new(channels, elements));
        for (kept, plane) in job.core.iter_mut().zip(core) {
            kept.copy_from_slice(plane);
        }
        for (kept, signal) in job.signals.iter_mut().zip(signals) {
            kept.copy_from_slice(signal);
        }
        job.renders.copy_from_slice(renders);
        self.push(job, out)
    }

    /// The blocks the fold made ready, coded in order; then what it failed
    /// at, if it failed, once the syncframes before it are written. The input
    /// it is done with is kept in `inputs` for another frame.
    fn folded(&mut self, folds: Folds, inputs: &mut Vec<Input>, out: &mut Vec<u8>) -> Result<()> {
        inputs.extend(folds.spent);
        for job in folds.jobs {
            self.push(job, out)?;
        }
        match folds.failed {
            Some(e) => self.refuse(e, out),
            None => Ok(()),
        }
    }

    /// [`Coder::step`] with the frame made: handed to the probe, and the
    /// frame before it fitted.
    fn push(&mut self, job: Job, out: &mut Vec<u8>) -> Result<()> {
        if let Some(trace) = &mut self.trace {
            let signals: Vec<&[f32]> = job.signals.iter().map(Vec::as_slice).collect();
            if let Err(e) = trace.frame(&signals, &job.renders) {
                return self.refuse(e, out);
            }
        }
        self.probe.send(job);
        self.probed += 1;
        if self.probed - self.fitted > 1 {
            self.fit(out)?;
        }
        Ok(())
    }

    /// The last frames' syncframes, appended to `out`.
    fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if let Some(trace) = &mut self.trace {
            if let Err(e) = trace.finish() {
                return self.refuse(e, out);
            }
        }
        if self.probed > self.fitted {
            self.fit(out)?;
        }
        if self.fitted > 0 {
            match self.room(self.fitted - 1) {
                Ok(room) => self.estimator.budget = room,
                Err(e) => return self.refuse(e, out),
            }
            let joc = self.estimator.finish().map(|joc| joc.to_bytes());
            self.stream.send(Write::Last { joc });
            self.writing += 1;
        }
        self.settle(out)?;
        self.probe.close();
        self.stream.close();
        Ok(())
    }

    /// Fit the gains of the oldest frame the probe has, and hand it to the
    /// stream with the payload of the frame before it, which the fit hands
    /// back.
    fn fit(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let job = match self.probe.recv() {
            Ok(job) => job,
            Err(e) => return self.refuse(e, out),
        };
        if self.fitted > 0 {
            match self.room(self.fitted - 1) {
                Ok(room) => self.estimator.budget = room,
                Err(e) => return self.refuse(e, out),
            }
        }
        let decoded = CORE.map(|ch| job.decoded[ch].as_slice());
        let signals: Vec<&[f32]> = job.signals.iter().map(Vec::as_slice).collect();
        let joc = self
            .estimator
            .push(&decoded, &signals, &self.present)
            .map(|joc| joc.to_bytes());
        self.renders.copy_from_slice(&job.renders);
        self.stream.send(Write::Frame { job, joc });
        self.writing += 1;
        self.fitted += 1;
        while let Some(written) = self.stream.try_recv() {
            self.take(written, out)?;
        }
        Ok(())
    }

    /// A syncframe the stream wrote, appended to `out`, and the frame it is
    /// done with kept for another.
    fn take(&mut self, written: Written, out: &mut Vec<u8>) -> Result<()> {
        self.writing -= 1;
        if let Some(spent) = written.spent {
            self.spare.push(spent);
        }
        if let Some(syncframe) = written.syncframe {
            out.extend_from_slice(&syncframe?);
            self.written += 1;
        }
        Ok(())
    }

    /// Every syncframe the stream is still writing, waited for and appended
    /// to `out`.
    fn settle(&mut self, out: &mut Vec<u8>) -> Result<()> {
        while self.writing > 0 {
            let written = self.stream.recv();
            self.take(written, out)?;
        }
        Ok(())
    }

    /// Stop at `e`, found coding a frame the stream's encoder has not reached:
    /// once the syncframes before it are written, as a single thread would
    /// have written them before it found it — or at the refusal of one of
    /// them, which it would have found first.
    fn refuse(&mut self, e: Error, out: &mut Vec<u8>) -> Result<()> {
        self.settle(out)?;
        Err(e)
    }

    /// The payload bytes stream frame `frame` leaves joint object coding,
    /// the frame fitted last.
    fn room(&self, frame: u64) -> Result<usize> {
        let overhead = overhead(frame, self.lfe, &self.renders, &self.information, &self.out)?;
        Ok(self.size.saturating_sub(overhead))
    }
}

/// What stream frame `frame`'s Evolution frame takes besides its joint
/// object coding payload, the payload's size field at its longest: its
/// elements rendering as `renders`.
fn overhead(
    frame: u64,
    lfe: bool,
    renders: &[Render],
    information: &[u8],
    out: &Path,
) -> Result<usize> {
    let placeholder = [0u8; 256];
    let (loudness, oamd) = (loudness(frame, None), oamd(lfe, renders, out)?);
    Ok(evolution_frame(&loudness, information, &oamd, &placeholder).len() - placeholder.len())
}

/// Stream frame `frame`'s loudness payload. Every [`TRUE_PEAK_EVERY`]th
/// frame states the core's true peak since the one before — measured, and
/// the meter started again, given the meter; a stand-in of the same size
/// when only its size is wanted. No practice is claimed, and no integrated
/// loudness: one has to match the stream's dialnorm, which is the user's.
fn loudness(frame: u64, meter: Option<&mut TruePeak>) -> Vec<u8> {
    let stated = (frame + 1).is_multiple_of(TRUE_PEAK_EVERY);
    let true_peak = stated.then(|| match meter {
        Some(meter) => {
            let peak = meter.take();
            true_peak_code(if peak > 0.0 {
                20.0 * peak.log10()
            } else {
                f64::NEG_INFINITY
            })
        }
        None => 0,
    });
    Loudness {
        true_peak,
        ..Loudness::default()
    }
    .to_bytes()
}

/// A syncframe's object metadata: one block, each element moving across the
/// frame to where it is at the frame's end, as the reference streams move
/// theirs — timed on the clock of the objects a decoder plays, [`LATENCY`]
/// behind the master's: the metadata comes with its access unit, and so does
/// the audio it places. Each element rendered as its feed says.
fn oamd(lfe: bool, renders: &[Render], out: &Path) -> Result<Vec<u8>> {
    let mut objects = Vec::with_capacity(renders.len() + 1);
    if lfe {
        objects.push(Object {
            active: true,
            gain: Gain::Unity,
            render: None,
        });
    }
    for &render in renders {
        objects.push(Object {
            active: true,
            gain: Gain::Unity,
            render: Some(render),
        });
    }
    ObjectAudioMetadata {
        lfe,
        blocks: vec![Block {
            offset: 0,
            ramp: Ramp::Long,
            objects,
        }],
    }
    .write()
    .map_err(|e| Error::unsupported(out, e.to_string()))
}

/// Renders for `count` elements that make the largest object metadata
/// they can: every field that costs bits stated — a size, a screen factor.
/// A position costs the same wherever it is: a block's is a step only from
/// the block before it in the same payload, and these carry one.
fn largest(count: usize) -> Vec<Render> {
    (0..count)
        .map(|_| Render {
            position: Position::from_master([1.0, -1.0, 1.0]),
            size: hz_meta::oamd::Size::Uniform(31),
            screen: Some((7, 3)),
            snap: true,
            elevation: true,
            zones: 7,
            ..Render::default()
        })
        .collect()
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

/// What every frame states of the programme: how its 5.1 core was folded
/// down, when the fold is one the Annex names (`dmixtyp`); what else was done
/// to it before coding — nothing: no surround attenuation, no phase shift, no
/// LFE filter — and the profile its compression words were computed with,
/// none, since it carries none.
fn information(downmix: Option<u8>) -> Vec<u8> {
    Information {
        downmix,
        preprocessing: Some(Preprocessing::default()),
        compression_profiles: Some((0, 0)),
        ..Information::default()
    }
    .to_bytes()
}

/// The `dmixtyp` a 3/2 core folded from the 7.1 render by `mode` declares
/// (TS 102 366 table H.3.9), where the fold is one it names. The spread
/// fold, each rear surround on its own side at √3/2 and the far side at a
/// half, is type 2: Ls' = Ls + (−1,2 dB)·Lrs + (−6,2 dB)·Rrs. The same-side
/// fold, the rears at unity, is none of them, and declares nothing rather
/// than the wrong one.
pub fn downmix_type(mode: hz_render::fold::FoldMode) -> Option<u8> {
    match mode {
        hz_render::fold::FoldMode::Spread => Some(2),
        hz_render::fold::FoldMode::SameSide => None,
    }
}

/// An Evolution frame behind its sync word, carrying the programme's
/// loudness and information, `oamd` and `joc`, in that order, as the
/// reference streams carry theirs: the loudness for the frame's 1536 samples
/// and kept, the information discarded by a decoder that does not know it,
/// the object metadata and joint object coding in group nought and kept, the
/// joint object coding frame-aligned; protected as they are.
fn evolution_frame(loudness: &[u8], information: &[u8], oamd: &[u8], joc: &[u8]) -> Vec<u8> {
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
            id: emdf::LOUDNESS,
            bytes: loudness,
            sample_offset: 0,
            config: emdf::Config {
                duration: Some(FRAME_SAMPLES as u32),
                group: None,
                codec_data: None,
                retain: retain(None),
            },
        },
        emdf::Payload {
            id: emdf::PROGRAMME_INFORMATION,
            bytes: information,
            sample_offset: 0,
            config: emdf::Config::default(),
        },
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
    /// loudness for the frame and kept, the information discarded if unknown,
    /// then the object metadata and the joint object coding, group nought,
    /// kept, the last frame-aligned — padded or not.
    #[test]
    fn an_evolution_frame_reads_back_as_the_references_carry_theirs() {
        let (loudness, information) = (vec![9u8, 8], information(Some(2)));
        let (oamd, joc) = (vec![1u8, 2, 3], vec![4u8, 5, 6, 7]);
        let mut frame = evolution_frame(&loudness, &information, &oamd, &joc);
        for padded in [false, true] {
            if padded {
                let size = frame.len() + 40;
                pad(&mut frame, size);
            }
            let (read, _) = emdf::read_synced(&frame, 0).expect("an Evolution frame");
            let ids: Vec<u32> = read.payloads.iter().map(|p| p.id).collect();
            assert_eq!(
                ids,
                [
                    emdf::LOUDNESS,
                    emdf::PROGRAMME_INFORMATION,
                    emdf::OAMD,
                    emdf::JOC
                ]
            );
            let bytes: Vec<&[u8]> = read.payloads.iter().map(|p| p.bytes.as_slice()).collect();
            assert_eq!(bytes, [&loudness[..], &information, &oamd, &joc]);
            assert!(read.payloads.iter().all(|p| p.sample_offset.is_none()));
            let loudness = &read.payloads[0].config;
            assert_eq!(loudness.duration, Some(1536));
            assert_eq!(loudness.retain.expect("kept").frame_aligned, None);
            assert_eq!(read.payloads[1].config.retain, None, "discarded if unknown");
            for (payload, aligned) in read.payloads[2..].iter().zip([None, Some((false, false))]) {
                assert_eq!(payload.config.group, Some(0));
                let retain = payload.config.retain.expect("kept");
                assert_eq!(retain.frame_aligned, aligned);
            }
        }
        // What the programme information states.
        let stated = Information::read(&information).unwrap();
        assert_eq!(
            stated.compression_profiles,
            Some((0, 0)),
            "no compression words"
        );
        assert_eq!(stated.preprocessing, Some(Preprocessing::default()));
        assert_eq!(stated.downmix, Some(2), "folded from 7.1, type 2");
        assert_eq!((stated.upmix, stated.active_channels), (None, None));
    }

    /// The true peak each interval states of a unit impulse at `sample` of
    /// frame `frame`, silence everywhere else, over 33 frames: frame 15 and
    /// frame 31, by the sequence counter.
    fn peaks_stated_of_an_impulse(
        frame: usize,
        sample: usize,
    ) -> std::collections::BTreeMap<u16, f64> {
        const FRAMES: usize = 33;
        let out = PathBuf::from("test.ec3");
        let mut writer = Writer::new(config(), beds(2), true, Some(2), &out, true).unwrap();
        let mut stream = Vec::new();
        let input = vec![0f32; 2 * FRAME_SAMPLES];
        for k in 0..FRAMES {
            let mut core = vec![vec![0f32; FRAME_SAMPLES]; 6];
            if k == frame {
                core[0][sample] = 1.0;
            }
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
        let mut stated = std::collections::BTreeMap::new();
        for frame in evolution_frames(&stream, writer.evolution_bytes()) {
            let joc = hz_joc::read(&frame.payloads[3].bytes).unwrap();
            let loudness = Loudness::read(&frame.payloads[0].bytes).unwrap();
            if let Some(peak) = loudness.true_peak {
                stated.insert(joc.sequence, hz_meta::programme::true_peak_db(peak));
            }
        }
        assert_eq!(stated.keys().copied().collect::<Vec<_>>(), [15, 31]);
        stated
    }

    /// A transient at the end of an interval is counted in it, not in the
    /// next: the meter's reconstruction runs behind, and the interval closes
    /// only once it has caught up. A unit impulse on frame 15's last sample,
    /// or three before it: frame 15 states it, near 0 dBTP, and frame 31 the
    /// silence after, all but its ringing. Closed at the frame's last sample,
    /// as the first version closed it, the three-before case came out at −39
    /// and 0 dBTP; closed a sample short of the meter's latency, the
    /// last-sample case still put 0 dBTP in frame 31.
    #[test]
    fn a_transient_at_an_intervals_end_is_counted_in_it() {
        // The ringing is louder the nearer the impulse is to the boundary.
        for (sample, ringing) in [(FRAME_SAMPLES - 3, -20.0), (FRAME_SAMPLES - 1, -10.0)] {
            let stated = peaks_stated_of_an_impulse(15, sample);
            assert!(
                stated[&15] > -1.0,
                "sample {sample}, the impulse's interval: {} dBTP",
                stated[&15]
            );
            assert!(
                stated[&31] < ringing,
                "sample {sample}, the silence after it: {} dBTP",
                stated[&31]
            );
        }
    }

    /// A transient at the start of an interval is counted in it: on frame
    /// 16's first sample, frame 31 states it.
    #[test]
    fn a_transient_at_an_intervals_start_is_counted_in_it() {
        let stated = peaks_stated_of_an_impulse(16, 0);
        assert!(stated[&31] > -1.0, "its interval: {} dBTP", stated[&31]);
    }

    /// The downmix declared is the fold made: the spread fold's surrounds are
    /// table H.3.9's type 2 — the near rear at −1,2 dB and the far at −6,2,
    /// to the tenth the table states them — and the same-side fold, the
    /// rears at unity, is none of the five and declares none.
    #[test]
    fn the_downmix_declared_is_the_fold_made() {
        use hz_render::fold::{FoldMode, presentation_matrix};
        let five_one = hz_render::Layout::surround_5_1();
        let seven_one = hz_render::Layout::surround_7_1();
        let matrix = presentation_matrix(&five_one, FoldMode::Spread)
            .unwrap()
            .unwrap();
        let gain = |from: &str, to: &str| {
            let i = seven_one.index_of(from).unwrap();
            let j = five_one.index_of(to).unwrap();
            20.0 * matrix[i][j].log10()
        };
        assert_eq!(downmix_type(FoldMode::Spread), Some(2));
        assert!((gain("M+090", "M+110") - 0.0).abs() < 0.05, "Ls itself");
        assert!((gain("M+135", "M+110") + 1.2).abs() < 0.1, "the near rear");
        assert!((gain("M-135", "M+110") + 6.2).abs() < 0.2, "the far rear");
        for front in ["M+030", "M-030", "M+000", "LFE1"] {
            assert!(gain(front, front).abs() < 1e-9, "{front} unchanged");
        }
        assert_eq!(downmix_type(FoldMode::SameSide), None);
    }

    fn config() -> hz_ac3::Config {
        hz_ac3::Config {
            format: hz_ac3::Format::Eac3,
            sample_rate: 48_000,
            mode: hz_ac3::ChannelMode::ThreeTwo,
            lfe: true,
            bit_rate: Some(448),
            coupling: true,
            metadata: hz_ac3::Metadata::default(),
        }
    }

    /// `elements` bed channels at distinct places, around the room and up.
    fn beds(elements: usize) -> Vec<Element> {
        (0..elements)
            .map(|channel| {
                let angle = channel as f64 / elements as f64 * std::f64::consts::TAU;
                Element::Bed {
                    channel,
                    position: [angle.sin(), angle.cos(), (channel % 2) as f64],
                }
            })
            .collect()
    }

    /// `frames` frames of noise on each of `elements` master channels, the
    /// first two of them (where there are two) on the core's L and R,
    /// through a writer.
    fn encode(elements: Vec<Element>, frames: usize) -> (Writer, Vec<u8>) {
        let (writer, stream, _) = encode_keeping_core(elements, frames);
        (writer, stream)
    }

    /// [`encode`], and the core that went in, interleaved.
    fn encode_keeping_core(elements: Vec<Element>, frames: usize) -> (Writer, Vec<u8>, Vec<f32>) {
        encode_on(elements, frames, true)
    }

    /// [`encode_keeping_core`], the encoders on threads of their own when
    /// `threads`, on the caller's otherwise.
    fn encode_on(
        elements: Vec<Element>,
        frames: usize,
        threads: bool,
    ) -> (Writer, Vec<u8>, Vec<f32>) {
        let mut kept = Vec::new();
        let width = elements.len();
        let out = PathBuf::from("test.ec3");
        let mut writer = Writer::new(config(), elements, true, Some(2), &out, threads).unwrap();
        let fold = match &writer.feed {
            Feed::Direct(_) => None,
            Feed::Folded(folding) => Some(matches!(folding.lane, Lane::Thread { .. })),
        };
        for lane in [
            matches!(writer.coder.probe, Lane::Thread { .. }),
            matches!(writer.coder.stream, Lane::Thread { .. }),
        ]
        .into_iter()
        .chain(fold)
        {
            assert_eq!(lane, threads, "the stages where they were asked to be");
        }
        let mut seed = 9u32;
        let mut stream = Vec::new();
        for k in 0..frames {
            let input: Vec<f32> = (0..width * FRAME_SAMPLES)
                .map(|_| 0.3 * noise(&mut seed))
                .collect();
            let core: Vec<Vec<f32>> = (0..6)
                .map(|ch| {
                    (0..FRAME_SAMPLES)
                        .map(|n| match ch {
                            0 | 1 if ch < width => input[n * width + ch],
                            _ => 0.05 * noise(&mut seed),
                        })
                        .collect()
                })
                .collect();
            for n in 0..FRAME_SAMPLES {
                kept.extend(core.iter().map(|plane| plane[n]));
            }
            let refs: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
            writer
                .frame(
                    &refs,
                    &input,
                    width,
                    FRAME_SAMPLES,
                    (k * FRAME_SAMPLES) as u64,
                    &mut stream,
                )
                .unwrap();
        }
        writer.finish(&mut stream).unwrap();
        (writer, stream, kept)
    }

    /// Every Evolution frame of `size` bytes in `stream`, found by its sync
    /// word — a skip field is not byte-aligned in the frame, so at each of
    /// the eight bit alignments — and what it carries.
    fn carried(stream: &[u8], size: usize) -> Vec<(ObjectAudioMetadata, hz_joc::Joc)> {
        evolution_frames(stream, size)
            .into_iter()
            .map(|frame| {
                (
                    ObjectAudioMetadata::read(&frame.payloads[2].bytes).unwrap(),
                    hz_joc::read(&frame.payloads[3].bytes).unwrap(),
                )
            })
            .collect()
    }

    /// Every Evolution frame of `size` bytes in `stream`, carrying the four
    /// payloads every frame does.
    fn evolution_frames(stream: &[u8], size: usize) -> Vec<emdf::Frame> {
        let sync = emdf::SYNC_WORD.to_be_bytes();
        let mut found = Vec::new();
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
                if ids
                    != [
                        emdf::LOUDNESS,
                        emdf::PROGRAMME_INFORMATION,
                        emdf::OAMD,
                        emdf::JOC,
                    ]
                {
                    continue;
                }
                let length = usize::from(u16::from_be_bytes([bytes[start + 2], bytes[start + 3]]));
                assert_eq!(length + 4, size, "every Evolution frame the one size");
                found.push(frame);
                at = start + size;
            }
        }
        found
    }

    /// One syncframe for every frame that went in, each carrying an
    /// Evolution frame of the one size, whose object metadata names the
    /// elements and whose payload carries them — and the stream's core the
    /// one the gains were fitted to, which the writer checks on every frame.
    #[test]
    fn the_writer_writes_a_syncframe_a_frame_carrying_its_objects() {
        const FRAMES: usize = 6;
        let (writer, stream) = encode(beds(2), FRAMES);
        assert_eq!(writer.written(), FRAMES as u64);
        assert!(
            writer.folder().is_none(),
            "two elements are carried as they are"
        );
        let mut sequences = std::collections::BTreeSet::new();
        for (metadata, joc) in carried(&stream, writer.evolution_bytes()) {
            assert!(metadata.lfe);
            assert_eq!(
                metadata.blocks[0].objects.len(),
                3,
                "the LFE and two elements"
            );
            assert_eq!(joc.objects.iter().flatten().count(), 2);
            sequences.insert(joc.sequence);
        }
        let expected: std::collections::BTreeSet<u16> = (0..FRAMES as u16).collect();
        assert_eq!(sequences, expected, "one Evolution frame each syncframe");
    }

    /// Every frame states the programme information, and every sixteenth the
    /// core's true peak since the last statement — the peak of exactly the
    /// frames up to it — and nothing else of the loudness.
    #[test]
    fn every_sixteenth_frame_states_the_true_peak_since_the_last() {
        const FRAMES: usize = 18;
        let (writer, stream, core) = encode_keeping_core(beds(2), FRAMES);
        let mut stated = std::collections::BTreeMap::new();
        for frame in evolution_frames(&stream, writer.evolution_bytes()) {
            let joc = hz_joc::read(&frame.payloads[3].bytes).unwrap();
            let loudness = Loudness::read(&frame.payloads[0].bytes).unwrap();
            assert_eq!(frame.payloads[1].bytes, information(Some(2)));
            assert_eq!(loudness.practice, 0, "no practice claimed");
            assert_eq!(loudness.relative_gated, None, "no integrated loudness");
            stated.insert(joc.sequence, loudness.true_peak);
        }
        assert_eq!(stated.len(), FRAMES);
        let mut meter = TruePeak::new(6);
        meter.push(&core[..16 * FRAME_SAMPLES * 6]);
        let want = true_peak_code(meter.peak_db());
        for (sequence, peak) in stated {
            if sequence == 15 {
                assert_eq!(peak, Some(want), "the peak of frames 0 to 15");
            } else {
                assert_eq!(peak, None, "frame {sequence}");
            }
        }
    }

    /// `count` objects, each crossing the room on a path of its own from a
    /// frame of its own, on the master's channels in order.
    fn movers(count: usize) -> Vec<Element> {
        (0..count)
            .map(|channel| {
                let angle = channel as f64 / count as f64 * std::f64::consts::TAU;
                let start = [angle.sin(), angle.cos(), (channel % 3) as f64 / 2.0];
                let end = [-start[0], -start[1], 1.0 - start[2]];
                let keyframes = vec![
                    Keyframe {
                        position: start,
                        ..Keyframe::default()
                    },
                    Keyframe {
                        sample_pos: (channel % 5 * FRAME_SAMPLES) as u64,
                        position: end,
                        ramp_samples: (6 + channel % 7) as u32 * FRAME_SAMPLES as u32,
                        ..Keyframe::default()
                    },
                ];
                Element::object(channel, keyframes)
            })
            .collect()
    }

    /// The stages on threads of their own write the stream the caller's
    /// writes alone, to the byte — the fold a frame ahead of the coder, the
    /// probe a frame ahead of the fit, the stream's encoder a frame behind
    /// it, the true peak stated across them: elements carried as they are,
    /// and folded into fifteen, still and moving.
    #[test]
    fn the_stages_on_threads_write_what_one_thread_writes() {
        const FRAMES: usize = 18;
        for what in ["2 beds", "20 beds", "20 moving objects"] {
            let elements = || match what {
                "2 beds" => beds(2),
                "20 beds" => beds(20),
                _ => movers(20),
            };
            let (alone, one, _) = encode_on(elements(), FRAMES, false);
            let (apart, threads, _) = encode_on(elements(), FRAMES, true);
            assert_eq!(apart.written(), FRAMES as u64, "{what}");
            assert_eq!(alone.written(), apart.written(), "{what}");
            assert!(one == threads, "{what}: the streams differ");
            assert_eq!(apart.folder().is_some(), what.starts_with("20"), "{what}");
            assert!(
                matches!(apart.coder.stream, Lane::Here { .. }),
                "{what}: the threads done with once the writer is finished"
            );
        }
    }

    /// A master with more elements than a payload carries is folded into
    /// fifteen, and still comes out a syncframe for every frame that went
    /// in, though the fold holds frames back to look ahead.
    #[test]
    fn a_master_with_more_elements_than_a_payload_carries_is_folded_into_fifteen() {
        const FRAMES: usize = 6;
        let (writer, stream) = encode(beds(20), FRAMES);
        assert_eq!(writer.elements(), MAX_ELEMENTS);
        assert!(writer.folder().is_some());
        assert_eq!(writer.written(), FRAMES as u64);
        let mut sequences = std::collections::BTreeSet::new();
        for (metadata, joc) in carried(&stream, writer.evolution_bytes()) {
            assert!(metadata.lfe);
            assert_eq!(metadata.blocks[0].objects.len(), 1 + MAX_ELEMENTS);
            assert_eq!(joc.objects.iter().flatten().count(), MAX_ELEMENTS);
            sequences.insert(joc.sequence);
        }
        let expected: std::collections::BTreeSet<u16> = (0..FRAMES as u16).collect();
        assert_eq!(sequences, expected, "one Evolution frame each syncframe");
    }

    /// A fold's block is the span a frame of objects comes out as, on the
    /// decoded objects' clock: a master's sample `n` is the block stream's
    /// `n + LATENCY`, all of it — the weights of an object have unit power —
    /// and the element that carries a lone sound ends where it is.
    #[test]
    fn a_fold_reads_the_master_on_the_decoded_objects_clock() {
        const MEMBERS: usize = 16;
        const LOUD: usize = 3;
        const IMPULSE: usize = 1000;
        let elements = beds(MEMBERS);
        let Element::Bed { position, .. } = elements[LOUD] else {
            unreachable!()
        };
        let mut folded = Folded::new(&config(), elements, Path::new("test.ec3")).unwrap();
        let core = vec![vec![0f32; FRAME_SAMPLES]; 6];
        let core: Vec<&[f32]> = core.iter().map(Vec::as_slice).collect();
        let mut seed = 5u32;
        let mut stream = vec![Vec::new(); MAX_ELEMENTS];
        for k in 0..4 {
            // The loud member: an impulse, and quiet noise after it so the
            // fold has something to place for the rest.
            let mut input = vec![0f32; MEMBERS * FRAME_SAMPLES];
            for n in 0..FRAME_SAMPLES {
                let t = k * FRAME_SAMPLES + n;
                input[n * MEMBERS + LOUD] = match t {
                    IMPULSE => 1.0,
                    t if t > 2 * IMPULSE => 0.01 * noise(&mut seed),
                    _ => 0.0,
                };
            }
            folded.read(&core, &input, MEMBERS, FRAME_SAMPLES);
            while folded.folder.waiting() > usize::from(k < 3) * 2 {
                let block = folded.fold().unwrap().expect("a block waiting");
                for (kept, mixed) in stream.iter_mut().zip(&folded.folder.mixer.mixed) {
                    kept.extend_from_slice(mixed);
                }
                if k == 3 {
                    let near = folded
                        .folder
                        .positions()
                        .iter()
                        .any(|p| (0..3).all(|i| (p[i] - position[i]).abs() < 0.05));
                    assert!(near, "an element where the sound is");
                }
                folded.folder.recycle(block);
            }
        }
        let at = IMPULSE + LATENCY;
        let power: f64 = stream.iter().map(|s| f64::from(s[at]).powi(2)).sum();
        assert!((power - 1.0).abs() < 1e-6, "{power}");
        for (n, _) in stream[0].iter().enumerate().take(2 * IMPULSE + LATENCY) {
            if n != at {
                assert!(stream.iter().all(|s| s[n] == 0.0), "{n}");
            }
        }
    }

    /// A moving object is placed where it is on the clock of the objects a
    /// decoder plays — [`LATENCY`] behind the master, not the core's
    /// [`DELAY`] — and rendered as the master says: its size, snapping,
    /// zones, screen and whether its height counts carried, not reset.
    #[test]
    fn an_object_is_placed_on_the_decoded_objects_clock_and_rendered_as_authored() {
        const FRAMES: usize = 12;
        let mode = hz_render::Mode {
            snap: true,
            zones: 1,
            elevation: false,
            screen: Some((3, 1)),
        };
        // From the left to the right, from sample 1536 over 15 360.
        let keyframes = vec![
            Keyframe {
                sample_pos: 0,
                position: [-1.0, 1.0, 0.75],
                spread: 0.8,
                mode,
                ..Keyframe::default()
            },
            Keyframe {
                sample_pos: 1536,
                position: [1.0, 1.0, 0.75],
                ramp_samples: 15_360,
                spread: 0.8,
                mode,
                ..Keyframe::default()
            },
        ];
        let (writer, stream) = encode(vec![Element::object(0, keyframes.clone())], FRAMES);
        let mut frames = carried(&stream, writer.evolution_bytes());
        frames.sort_by_key(|(_, joc)| joc.sequence);
        assert_eq!(frames.len(), FRAMES);
        let trajectory = Trajectory::new(&keyframes);
        let mut hint = 0;
        for (frame, (metadata, _)) in frames.iter().enumerate() {
            let render = metadata.blocks[0].objects[1].render.unwrap();
            let t = ((frame as u64 + 1) * FRAME_SAMPLES as u64).saturating_sub(LATENCY as u64);
            let want = trajectory.segment(t, &mut hint).at(t).position;
            assert_eq!(
                render.position,
                Position::from_master(want),
                "frame {frame}"
            );
            assert_eq!(render.size, hz_meta::oamd::Size::Uniform(25), "size 0.8");
            assert!(render.snap, "frame {frame}");
            assert_eq!(render.zones, 1);
            assert_eq!(render.screen, Some((3, 1)));
            assert!(!render.elevation, "its height not to count, at z = 0.75");
        }
        // Frame 3's objects end at master sample 5311, a quarter of the way
        // across: x code 15, where the core's clock (5888) would say 18.
        let render = frames[3].0.blocks[0].objects[1].render.unwrap();
        assert_eq!(render.position.x, 15);
    }

    /// A folded element renders as the class of what it carries: objects
    /// that snap, folded with others that do not, keep an element that
    /// snaps.
    #[test]
    fn a_folded_element_renders_as_its_class() {
        const FRAMES: usize = 6;
        let snapping = hz_render::Mode {
            snap: true,
            ..hz_render::Mode::default()
        };
        let elements: Vec<Element> = (0..20)
            .map(|channel| {
                let angle = channel as f64 / 20.0 * std::f64::consts::TAU;
                Element::object(
                    channel,
                    vec![Keyframe {
                        position: [angle.sin(), angle.cos(), 0.0],
                        mode: if channel % 4 == 0 {
                            snapping
                        } else {
                            hz_render::Mode::default()
                        },
                        ..Keyframe::default()
                    }],
                )
            })
            .collect();
        let (writer, stream) = encode(elements, FRAMES);
        assert!(writer.folder().is_some());
        for (metadata, _) in carried(&stream, writer.evolution_bytes()) {
            let renders: Vec<Render> = metadata.blocks[0].objects[1..]
                .iter()
                .map(|object| object.render.unwrap())
                .collect();
            assert!(renders.iter().any(|r| r.snap), "an element that snaps");
            assert!(renders.iter().any(|r| !r.snap), "and one that does not");
        }
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
