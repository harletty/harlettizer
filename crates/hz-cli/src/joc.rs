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
    Folded(Box<Folded>),
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
}

/// Frames from one statement of the true peak to the next, as the reference
/// streams state theirs.
const TRUE_PEAK_EVERY: u64 = 16;

/// The two encoders and the estimator, and what passes between them.
struct Coder {
    probe: Encoder,
    real: Encoder,
    estimator: Estimator,
    elements: usize,
    lfe: bool,
    /// Every Evolution frame's bytes, and the probe's stand-in for one.
    size: usize,
    placeholder: Vec<u8>,
    /// Where each element is at the end of the last two frames that went
    /// in, and how it renders, frame `k` at `k % 2`: the object metadata is
    /// written a frame behind.
    renders: [Vec<Render>; 2],
    /// The last frame of core that went in, for the stream's encoder a frame
    /// behind, and what the probe decoded of it.
    core: Vec<Vec<f32>>,
    probed: Vec<Vec<f32>>,
    frames_in: u64,
    written: u64,
    scratch: Vec<u8>,
    /// The true peak of the core since it was last stated, the core
    /// interleaved for it, and the programme information every frame states.
    peak: TruePeak,
    interleaved: Vec<f32>,
    information: Vec<u8>,
    trace: Option<Trace>,
    out: PathBuf,
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
    /// fold that made the core ([`downmix_type`]).
    pub fn new(
        config: hz_ac3::Config,
        elements: Vec<Element>,
        lfe: bool,
        downmix: Option<u8>,
        out: &Path,
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
            let feed = Feed::Folded(Box::new(Folded::new(&config, elements, out)?));
            (feed, MAX_ELEMENTS)
        };
        Ok(Self {
            feed,
            coder: Coder::new(config, carried, lfe, downmix, out)?,
        })
    }

    /// The stream's encoder, for what it reports.
    pub fn encoder(&self) -> &Encoder {
        &self.coder.real
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

    /// The fold, when the master's elements were folded.
    pub fn folder(&self) -> Option<&Folder<Vec<Vec<f32>>>> {
        match &self.feed {
            Feed::Direct(_) => None,
            Feed::Folded(folded) => Some(&folded.folder),
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
            Feed::Folded(folded) => {
                folded.read(core, input, stride, got);
                while folded.folder.ready() {
                    folded.code(&mut self.coder, out)?;
                }
                Ok(())
            }
        }
    }

    /// The frames still held, and the last frame's syncframe, appended to
    /// `out`.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if let Feed::Folded(folded) = &mut self.feed {
            while folded.folder.waiting() > 0 {
                folded.code(&mut self.coder, out)?;
            }
            folded.last(&mut self.coder, out)?;
        }
        self.coder.finish(out)
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

    /// Fold the oldest block, and code the block before it with the core it
    /// was read with, now that its elements run on into this one.
    fn code(&mut self, coder: &mut Coder, out: &mut Vec<u8>) -> Result<()> {
        let Some(block) = self.fold()? else {
            return Ok(());
        };
        for (element, mixed) in self.elements.iter_mut().zip(&self.folder.mixer.mixed) {
            element[FRAME_SAMPLES..].copy_from_slice(mixed);
        }
        let result = self.step(coder, out);
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
        result
    }

    /// Code the last block held: past it there is nothing, so its elements
    /// run on into silence.
    fn last(&mut self, coder: &mut Coder, out: &mut Vec<u8>) -> Result<()> {
        for element in &mut self.elements {
            element[FRAME_SAMPLES..].fill(0.0);
        }
        self.step(coder, out)?;
        if let Some(done) = self.held.take() {
            self.folder.recycle(done);
        }
        Ok(())
    }

    /// Code the block held, if there is one: its core, and its elements on
    /// the core's clock, [`hz_joc::qmf::DELAY`] into them.
    fn step(&self, coder: &mut Coder, out: &mut Vec<u8>) -> Result<()> {
        let Some(held) = &self.held else {
            return Ok(());
        };
        const INTO: usize = hz_joc::qmf::DELAY;
        let core: Vec<&[f32]> = held.carry.iter().map(Vec::as_slice).collect();
        let signals: Vec<&[f32]> = self
            .elements
            .iter()
            .map(|e| &e[INTO..INTO + FRAME_SAMPLES])
            .collect();
        coder.step(&core, &signals, &self.renders, out)
    }
}

impl Coder {
    fn new(
        config: hz_ac3::Config,
        count: usize,
        lfe: bool,
        downmix: Option<u8>,
        out: &Path,
    ) -> Result<Self> {
        let encoder = || {
            let mut encoder =
                Encoder::new(config).map_err(|e| Error::unsupported(out, e.0.clone()))?;
            encoder.reconstruct();
            Ok::<_, Error>(encoder)
        };
        let (probe, real) = (encoder()?, encoder()?);
        let channels = probe.channels();
        let mut coder = Self {
            probe,
            real,
            estimator: Estimator::new(count, 0),
            elements: count,
            lfe,
            size: 0,
            placeholder: Vec::new(),
            renders: [largest(count), largest(count)],
            core: vec![vec![0.0; FRAME_SAMPLES]; channels],
            probed: vec![vec![0.0; FRAME_SAMPLES]; channels],
            frames_in: 0,
            written: 0,
            scratch: Vec::new(),
            peak: TruePeak::new(channels),
            interleaved: vec![0.0; channels * FRAME_SAMPLES],
            information: information(downmix),
            trace: Trace::open(count)?,
            out: out.to_path_buf(),
        };
        // Room for the payloads the references carry, and never less than
        // the smallest one these elements can make, so that the budget can
        // always bring a payload under it. What the object metadata takes
        // depends on how the elements render — a size, a screen factor — and
        // the frame's size is the stream's, so it is measured with every
        // field that costs bits stated — the true peak among them.
        let overhead = coder.overhead(TRUE_PEAK_EVERY - 1)?;
        let floor = overhead + smallest_payload(count);
        coder.size = (overhead + ROOM_PER_ELEMENT * count + 8).max(floor);
        if coder.size > SKIP_LIMIT {
            if floor > SKIP_LIMIT {
                return Err(Error::unsupported(
                    out,
                    format!(
                        "an Evolution frame of at least {floor} bytes for {count} elements, past \
                         the {SKIP_LIMIT} a skip field holds"
                    ),
                ));
            }
            coder.size = SKIP_LIMIT;
        }
        coder.placeholder = vec![0; coder.size];
        Ok(coder)
    }

    /// One frame: its core, each element's samples on the decoded core's
    /// clock, and where each element is at the frame's end. Appends to `out`
    /// the syncframe of the frame before, once there is one.
    fn step(
        &mut self,
        core: &[&[f32]],
        signals: &[&[f32]],
        renders: &[Render],
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.renders[(self.frames_in % 2) as usize].copy_from_slice(renders);
        if let Some(trace) = &mut self.trace {
            trace.frame(signals, renders)?;
        }
        // The meter closes the frame before's interval when it is written,
        // below, and its reconstruction runs behind: so the first samples of
        // this frame go in first, and the rest after.
        let channels = core.len();
        for (ch, plane) in core.iter().enumerate() {
            for (n, &sample) in plane.iter().enumerate() {
                self.interleaved[n * channels + ch] = sample;
            }
        }
        self.peak
            .push(&self.interleaved[..truepeak::LATENCY * channels]);
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
        let present = vec![true; signals.len()];
        if let Some(joc) = self.estimator.push(&decoded, signals, &present) {
            self.write(&joc.to_bytes(), out)?;
        }
        for (ch, (kept, probed)) in self.core.iter_mut().zip(&mut self.probed).enumerate() {
            kept.copy_from_slice(core[ch]);
            probed.copy_from_slice(self.probe.decoded(ch).expect("reconstructing"));
        }
        // When frame `k` is written, the meter has frames up to it and the
        // next frame's first samples, its reconstruction of frame `k` done.
        self.peak
            .push(&self.interleaved[truepeak::LATENCY * channels..]);
        self.frames_in += 1;
        Ok(())
    }

    /// The last frame's syncframe, appended to `out`.
    fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if let Some(trace) = &mut self.trace {
            trace.finish()?;
        }
        if self.frames_in == 0 {
            return Ok(());
        }
        self.estimator.budget = self.room(self.frames_in - 1)?;
        // Past the programme's end is silence: the meter's reconstruction
        // of the last frame runs out into it.
        let channels = self.core.len();
        self.interleaved[..truepeak::LATENCY * channels].fill(0.0);
        self.peak
            .push(&self.interleaved[..truepeak::LATENCY * channels]);
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
        let loudness = self.loudness(frame, true);
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
        let (loudness, oamd) = (self.loudness(frame, false), self.oamd(frame)?);
        Ok(
            evolution_frame(&loudness, &self.information, &oamd, &placeholder).len()
                - placeholder.len(),
        )
    }

    /// Stream frame `frame`'s loudness payload. Every [`TRUE_PEAK_EVERY`]th
    /// frame states the core's true peak since the one before — measured,
    /// and the meter started again, when `measure`; a stand-in of the same
    /// size when only its size is wanted. No practice is claimed, and no
    /// integrated loudness: one has to match the stream's dialnorm, which is
    /// the user's.
    fn loudness(&mut self, frame: u64, measure: bool) -> Vec<u8> {
        let stated = (frame + 1).is_multiple_of(TRUE_PEAK_EVERY);
        let true_peak = stated.then(|| {
            if measure {
                let peak = self.peak.take();
                true_peak_code(if peak > 0.0 {
                    20.0 * peak.log10()
                } else {
                    f64::NEG_INFINITY
                })
            } else {
                0
            }
        });
        Loudness {
            true_peak,
            ..Loudness::default()
        }
        .to_bytes()
    }

    /// Stream frame `frame`'s object metadata: one block, each element moving
    /// across the frame to where it is at the frame's end, as the reference
    /// streams move theirs — timed on the clock of the objects a decoder
    /// plays, [`LATENCY`] behind the master's: the metadata comes with its
    /// access unit, and so does the audio it places. Each element rendered
    /// as its feed says.
    fn oamd(&mut self, frame: u64) -> Result<Vec<u8>> {
        let mut objects = Vec::with_capacity(self.elements + 1);
        if self.lfe {
            objects.push(Object {
                active: true,
                gain: Gain::Unity,
                render: None,
            });
        }
        for &render in &self.renders[(frame % 2) as usize] {
            objects.push(Object {
                active: true,
                gain: Gain::Unity,
                render: Some(render),
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
        let mut writer = Writer::new(config(), beds(2), true, Some(2), &out).unwrap();
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
        let mut kept = Vec::new();
        let width = elements.len();
        let out = PathBuf::from("test.ec3");
        let mut writer = Writer::new(config(), elements, true, Some(2), &out).unwrap();
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
