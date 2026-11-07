//! DTS-HD Master Audio: a core and a lossless layer in one stream.
//!
//! Each frame is a core frame ([`crate::encoder`]) followed by an extension
//! substream ([`crate::exss`]) carrying one audio asset whose only component
//! is the lossless layer ([`crate::lossless`]). The core is the 5.1 a
//! decoder of the lossless layer's first channel set alone presents — the
//! input itself for a 5.1, the fold for a 7.1 — so a core-only decoder and a
//! 5.1 lossless decoder play the same mix.
//!
//! The lossless layer codes each channel the core carries as its difference
//! from the core's reconstruction where that is cheaper
//! ([`lossless::Config::residual`]), so the encoder decodes every core frame
//! it sends as a decoder will ([`CoreDecoder`]). A decoder's core output is
//! a frame behind its input, so the core runs a frame ahead of the lossless
//! layer: the stream's frame *n* carries the core encoded from input frame
//! *n* + 1, which decodes to input frame *n*, beside the lossless frame of
//! input frame *n*. The core frame made from the first input frame decodes to
//! the silence before it and is not sent, and [`Encoder::finish`] sends the
//! last input frame with a core made from silence. So the stream has a frame
//! per input frame, and both layers decode in step with the input — the
//! lossless layer to the sample. With the residual off, every channel is
//! coded as itself and the layers are independent, but stay in step.
//!
//! The substream header is the reference streams' (one presentation, one
//! asset, the core backward-compatible in the core substream). The stream is
//! held under a peak bit rate ceiling, the core and every header included
//! ([`peak::Config`], 24.5 Mb/s by default). A lossless frame goes whole in
//! its own substream, so the substreams vary in size frame to frame as the
//! reference streams' do; only a frame too large for its own substream has
//! part of it sent ahead, in the substreams before it, into the decoder's
//! smoothing buffer ([`peak::Smoother`]). Where no frame the lossless layer
//! can write is too large — at the default ceiling, any — the stream states
//! no smoothing buffer and every frame goes out as soon as it is encoded.
//! Otherwise the encoder looks [`peak::Config::lookahead`] frames ahead, and
//! a frame that cannot reach the decoder in time is refused with
//! [`Refused::PeakRate`], naming it: lossless data is never coded coarser,
//! and the stream ends before it.
//!
//! A 7.1.4 is the DTS:X extension's standard profile ([`crate::dtsx`]): the
//! stream above, its 7.1 the composite the heights are folded into, and at
//! the end of each lossless frame the block that carries the heights. The
//! asset still states the 7.1; its descriptor's tail carries the navigation
//! word that locates the block. A frame the word cannot locate is refused
//! with [`Refused::Navigation`], naming it, and the stream ends there: the
//! frame is not cut to fit, and the stream cannot go on without it.

use crate::bits::Bits;
use crate::core_decode::CoreDecoder;
use crate::encoder;
use crate::encoder::FRAME_SAMPLES;
use crate::exss::{
    AssetDescriptor, AssetStatics, ChannelMapping, Coding, ExssHeader, Presentation, StaticFields,
    Xll, XllSync, component,
};
use crate::lossless::{self, Layout};
use crate::navigation::Navigation;
use crate::peak::{self, Smoother};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};

pub use crate::encoder::Unsupported;
pub use crate::lossless::Refused;

/// What to encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub lossless: lossless::Config,
    /// The core: 5.1 at 48 kHz, whatever else it is.
    pub core: encoder::Config,
    /// The peak bit rate the stream is held under.
    pub peak: peak::Config,
}

impl Config {
    /// The reference streams' shape: the lossless layer as
    /// [`lossless::Config::reference`] lays it out, and a 1509 kb/s core
    /// stating the source's word length as they do — 6 for 24 bits, 0 for
    /// sixteen — under the default ceiling.
    pub fn reference(layout: Layout, bits: u8) -> Self {
        Self {
            lossless: lossless::Config::reference(layout, bits),
            core: encoder::Config {
                pcm_resolution_code: if bits > 16 { 6 } else { 0 },
                ..encoder::Config::reference_five_one()
            },
            peak: peak::Config::default(),
        }
    }
}

/// The largest substream the wide size field describes, and the largest
/// header its 12-bit header size does.
const WIDEST_SUBSTREAM: usize = 1 << 20;
const MAX_HEADER: usize = 1 << 12;

/// The most frames the encoder looks ahead: about twelve minutes.
const MAX_LOOKAHEAD: usize = 1 << 16;

/// How the stream has used its ceiling so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rate {
    /// The most bytes a frame may take under the ceiling.
    pub budget: usize,
    /// The largest frame sent, core and headers included.
    pub largest: usize,
    /// The smoothing buffer the stream states, in bytes: 0 when no frame
    /// can need it.
    pub buffer: usize,
    /// The most bytes the decoder's buffer has held at once.
    pub held: u64,
    /// Frames whose substream carried other than exactly their own
    /// lossless frame.
    pub ahead: u64,
}

/// The encoder.
pub struct Encoder {
    core: encoder::Encoder,
    lossless: lossless::Encoder,
    exss: ExssHeader,
    /// The core as a decoder of the stream reconstructs it.
    decoder: CoreDecoder,
    /// The input frame waiting for the core frame that decodes to it, and
    /// whether there is one.
    pending: Vec<i32>,
    waiting: bool,
    /// The core frame last encoded.
    core_frame: Vec<u8>,
    /// Frames encoded.
    frames: u64,
    /// Bytes of the lossless frames encoded, and how many of their channels
    /// were coded against the core.
    lossless_bytes: u64,
    against_core: u64,
    /// The furthest offset and the largest DTS:X block navigated so far.
    reach: Option<(usize, usize)>,
    /// The frames encoded and not yet sent: their cores end to end, the
    /// lossless stream from the first byte not sent, and each frame's
    /// navigation word when it has one.
    cores: VecDeque<u8>,
    lossless_stream: VecDeque<u8>,
    words: VecDeque<Option<u64>>,
    smoother: Smoother,
    /// The buffer code the substreams state.
    buffer_code: u8,
    rate: Rate,
    /// Once a frame cannot be carried, the stream has ended: the refusal.
    ended: Option<Refused>,
    /// The threads the lossless frames are coded on, when they are not
    /// coded on the caller's ([`Encoder::with_threads`]).
    pool: Option<Pool>,
}

/// A lossless frame to code on a thread of the pool: the input frame, the
/// core's reconstruction of it, and its core frame, which stays with it to
/// go out beside it. It comes back with the frame coded into `coded`.
struct Job {
    index: u64,
    pcm: Vec<i32>,
    /// The reconstruction, and whether the frame is coded against it.
    core: Box<CoreDecoder>,
    against: bool,
    core_frame: Vec<u8>,
    coded: lossless::Coded,
}

/// Threads that code lossless frames apart ([`lossless::Encoder::code_apart`])
/// for the encoder to settle in order. A frame's coding depends on nothing
/// before it but the width its size is written at, which settling decides,
/// so the stream is the one the encoder writes alone, to the byte.
struct Pool {
    jobs: Option<mpsc::Sender<Job>>,
    done: mpsc::Receiver<Job>,
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Frames handed to the pool so far: the index of the next.
    queued: u64,
    /// Frames back from the pool and not yet settled, from the next to
    /// settle on: a frame can come back before the one ahead of it.
    back: VecDeque<Option<Job>>,
    /// Frames handed out and not yet settled.
    in_flight: usize,
    /// The most frames handed out at once.
    most: usize,
    /// Jobs settled, to code the next frames into.
    spare: Vec<Job>,
}

impl Pool {
    /// `threads` threads, each with a lossless encoder of `config`; `None`
    /// when not one of them can be started.
    fn new(threads: usize, config: &lossless::Config) -> Option<Self> {
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (outbox, done) = mpsc::channel::<Job>();
        let inbox = Arc::new(Mutex::new(inbox));
        let mut workers = Vec::with_capacity(threads);
        for n in 0..threads {
            let Ok(mut encoder) = lossless::Encoder::new(config.clone()) else {
                break;
            };
            let inbox = Arc::clone(&inbox);
            let outbox = outbox.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("lossless-{n}"))
                .spawn(move || {
                    loop {
                        // The lock is held only to take a job: the others
                        // would be waiting for one anyway.
                        let next = inbox.lock().map(|inbox| inbox.recv());
                        let Ok(Ok(mut job)) = next else {
                            return;
                        };
                        let core = job.against.then_some(&*job.core);
                        encoder.code_apart(&job.pcm, core, &mut job.coded);
                        if outbox.send(job).is_err() {
                            return;
                        }
                    }
                });
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(_) => break,
            }
        }
        if workers.is_empty() {
            return None;
        }
        let most = 2 * workers.len() + 2;
        Some(Self {
            jobs: Some(jobs),
            done,
            workers,
            queued: 0,
            back: VecDeque::new(),
            in_flight: 0,
            most,
            spare: Vec::new(),
        })
    }

    /// Put a frame back from the pool in its place.
    fn place(&mut self, job: Job, settled: u64) {
        let at = (job.index - settled) as usize;
        if self.back.len() <= at {
            self.back.resize_with(at + 1, || None);
        }
        self.back[at] = Some(job);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Closing the queue is what tells the threads to stop.
        self.jobs = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// How far [`Encoder::settle_back`] goes before it returns.
#[derive(Clone, Copy)]
enum Until {
    /// Until no more frames are out than the pool takes at once.
    Room,
    /// Until every frame before this one is settled.
    Settled(u64),
}

/// Fit `exss` around `size` bytes of data: narrow fields where the whole
/// substream fits them, wide otherwise, and the header padded to a 32-bit
/// word as every reference stream's is. Its bytes.
fn header(exss: &mut ExssHeader, size: u32) -> Vec<u8> {
    exss.wide = false;
    loop {
        exss.reserved = Bits::new();
        exss.fit()
            .expect("a lossless frame fits a substream's wide fields");
        // Every reference stream's header ends on a 32-bit word — 28 to 44
        // bytes, never between — so that what follows starts on one: zeroes
        // in the reserved bits to get there.
        let short = (4 - exss.header_size % 4) % 4;
        if short > 0 {
            exss.reserved.pad(8 * short as usize);
            exss.fit()
                .expect("a lossless frame fits a substream's wide fields");
        }
        exss.size = exss.header_size + size;
        match exss.write() {
            Ok(header) => return header,
            Err(_) if !exss.wide => exss.wide = true,
            Err(e) => panic!("a fitted substream header does not write: {}", e.0),
        }
    }
}

impl Encoder {
    pub fn new(config: Config) -> Result<Self, Unsupported> {
        let bits = config.lossless.bits;
        if config.core.layout != encoder::Layout::FivePointOne || config.core.sample_rate != 48000 {
            return Err(Unsupported(format!(
                "a {:?} core at {} Hz: the lossless layer sits beside a 5.1 core at 48 kHz",
                config.core.layout, config.core.sample_rate
            )));
        }
        let lossless = lossless::Encoder::new(config.lossless).map_err(|e| Unsupported(e.0))?;
        let core = encoder::Encoder::new(config.core)?;
        let layout = lossless.config().layout;
        let exss = ExssHeader {
            user_defined: 0,
            index: 0,
            wide: false,
            header_size: 0,
            size: 0,
            statics: Some(StaticFields {
                // 48 kHz; frames of 512 samples of it.
                reference_clock: 2,
                frame_duration_code: 0,
                timecode: None,
                presentations: vec![Presentation {
                    active_substreams: 1,
                    active_assets: vec![1],
                }],
                asset_count: 1,
                mixing: None,
            }),
            asset_sizes: vec![1],
            assets: vec![AssetDescriptor {
                size: 0,
                index: 0,
                statics: Some(AssetStatics {
                    asset_type: None,
                    language: None,
                    text: None,
                    pcm_bit_res: bits,
                    max_sample_rate_code: 12,
                    channels: layout.bed_channels() as u16,
                    mapping: ChannelMapping::OneToOne {
                        embedded_stereo: false,
                        embedded_6ch: layout != Layout::FivePointOne,
                        speaker_mask: Some(lossless.speaker_activity()),
                        remaps: vec![],
                    },
                }),
                drc_code: None,
                dialnorm_code: None,
                stereo_drc_code: None,
                mixing: None,
                coding: Coding::Components {
                    // The core in the core substream, the lossless layer here.
                    mask: 0x001 | component::XLL,
                    core: None,
                    xbr: None,
                    xxch: None,
                    x96: None,
                    lbr: None,
                    xll: Some(Xll {
                        size: 1,
                        sync: Some(XllSync {
                            peak_buffer_code: 0,
                            delay_bits: 1,
                            delay_frames: 0,
                            sync_offset: 0,
                        }),
                    }),
                    rsv1: None,
                    rsv2: None,
                },
                hd_stream_id: Some(0),
                tail: Bits::new(),
            }],
            backward_core: vec![Some((0, 0))],
            reserved: Bits::new(),
            crc: 0,
        };

        // The room a substream has for lossless data under the ceiling: the
        // frame's budget, less the core and the largest header the stream
        // can need — sync fields at their widest, the navigation word.
        let peak = config.peak;
        if peak.buffer_code > 15 {
            return Err(Unsupported(format!(
                "a smoothing buffer code of {}: the field holds 0 to 15",
                peak.buffer_code
            )));
        }
        if peak.lookahead > MAX_LOOKAHEAD {
            return Err(Unsupported(format!(
                "a lookahead of {} frames: {MAX_LOOKAHEAD} at most",
                peak.lookahead
            )));
        }
        let budget = peak::frame_budget(peak.ceiling, FRAME_SAMPLES as u32, 48000);
        let unsupported = |overhead: usize| {
            Unsupported(format!(
                "a ceiling of {} b/s: {budget} bytes a frame, which the core and the headers \
                 ({overhead} bytes) leave no room in for the lossless layer",
                peak.ceiling
            ))
        };
        if budget < core.frame_size() + 2 {
            return Err(unsupported(core.frame_size()));
        }
        // The most lossless data a substream can carry: what the budget
        // leaves beside the core, and no more than the wide size field
        // describes with the largest header around it.
        let largest = (budget - core.frame_size()).min(WIDEST_SUBSTREAM - MAX_HEADER) as u32;
        let mut widest = exss.clone();
        widest.asset_sizes[0] = largest;
        if let Coding::Components { xll: Some(x), .. } = &mut widest.assets[0].coding {
            x.size = largest;
            let delay = peak.lookahead as u32 + 1;
            x.sync = Some(XllSync {
                peak_buffer_code: peak.buffer_code,
                delay_bits: peak::delay_bits(delay),
                delay_frames: delay,
                sync_offset: largest - 1,
            });
        }
        if layout == Layout::SevenPointOneFour {
            widest.assets[0].tail = tail(0);
        }
        let overhead = core.frame_size() + header(&mut widest, largest).len();
        let room = budget.saturating_sub(overhead).min(largest as usize);
        if room < 2 {
            return Err(unsupported(overhead));
        }
        // Smoothing only where a frame can need it.
        let smoothing = peak.buffer_code > 0 && lossless.frame_bound() > room;
        let buffer_code = if smoothing { peak.buffer_code } else { 0 };
        let smoother = Smoother::new(
            room,
            peak::buffer_bytes(buffer_code),
            if smoothing { peak.lookahead } else { 0 },
        );
        let rate = Rate {
            budget,
            buffer: peak::buffer_bytes(buffer_code),
            ..Rate::default()
        };
        Ok(Self {
            pending: vec![0; FRAME_SAMPLES * layout.channels()],
            waiting: false,
            core_frame: Vec::with_capacity(core.frame_size()),
            frames: 0,
            lossless_bytes: 0,
            against_core: 0,
            reach: None,
            decoder: CoreDecoder::new(),
            cores: VecDeque::new(),
            lossless_stream: VecDeque::new(),
            words: VecDeque::new(),
            smoother,
            buffer_code,
            rate,
            ended: None,
            pool: None,
            core,
            lossless,
            exss,
        })
    }

    /// Code the lossless frames on `threads` threads of their own, the core
    /// staying on the caller's: the lossless layer is most of the work, and
    /// a frame of it depends on nothing before it but the width its size is
    /// written at, which is settled on the caller's thread, in order. The
    /// stream is the same to the byte; the frames come out of
    /// [`Encoder::encode`] later, up to twice as many calls behind the input
    /// as there are threads. Nought, the default, codes them on the caller's
    /// thread; a pool that cannot be started codes them there too.
    ///
    /// With threads, a frame refused once it is coded — for the navigation
    /// word as for the peak rate — ends the stream, and the frames the
    /// threads had in hand after it are not sent. A frame [`Encoder::encode`]
    /// refuses before coding it moves nothing on, threads or not.
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.pool = (threads > 0)
            .then(|| Pool::new(threads, self.lossless.config()))
            .flatten();
        self
    }

    /// Take one frame — [`FRAME_SAMPLES`] samples of every channel,
    /// interleaved in WAV order as 32-bit samples whose bits below the
    /// configured resolution are zero — and append to `out` the stream's
    /// frames that are ready: the frame before this one, since the core runs
    /// a frame ahead (nothing on the first call), and, where the stream may
    /// smooth, only once the encoder has looked far enough past it — or,
    /// with threads ([`Encoder::with_threads`]), once they have coded it.
    /// [`Encoder::finish`] sends the rest.
    ///
    /// A refusal leaves in `out` nothing of the refused frame's own, and,
    /// with threads, the frames before it that the calls before would have
    /// handed back without them: the frames written before a refusal are the
    /// same either way. After [`Refused::PeakRate`] the stream has ended, and
    /// with threads after any refusal of a coded frame: every later call
    /// returns the same refusal.
    ///
    /// # Panics
    /// If `pcm` is not exactly one frame.
    pub fn encode(&mut self, pcm: &[i32], out: &mut Vec<u8>) -> Result<(), Refused> {
        assert_eq!(
            pcm.len(),
            self.pending.len(),
            "a frame is {FRAME_SAMPLES} samples of each channel"
        );
        if let Some(refused) = &self.ended {
            return Err(refused.clone());
        }
        // Refused before anything moves on, naming the input frame: the
        // lossless layer has coded the frames sent, not the one waiting.
        if let Err(refused) = self.lossless.check(pcm) {
            let refused = refused.of_frame(self.handed_out() + u64::from(self.waiting));
            if self.pool.is_some() {
                // Every frame handed out goes out first, as each would have
                // in the call after it.
                self.settle_back(out, Until::Settled(self.handed_out()))?;
            }
            return Err(refused);
        }
        self.core_frame.clear();
        self.core
            .encode(self.lossless.core_input(pcm), &mut self.core_frame);
        if !self.waiting {
            // What this core frame decodes to is the silence before the
            // input: it is not sent, and the stream's decoder starts its
            // subband prediction on the next from silence.
            self.core.restart_prediction();
        } else if self.pool.is_some() {
            self.hand_out();
            self.settle_back(out, Until::Room)?;
        } else {
            let mark = out.len();
            if let Err(refused) = self.send(out) {
                out.truncate(mark);
                return Err(refused);
            }
        }
        self.pending.copy_from_slice(pcm);
        self.waiting = true;
        Ok(())
    }

    /// Encode the frame still waiting, with a core made from silence, and
    /// send every frame not yet sent. A refusal leaves in `out`, as
    /// [`Encoder::encode`]'s does, the frames that would have gone out by the
    /// last call to it, and nothing of this one's own.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
        if let Some(refused) = &self.ended {
            return Err(refused.clone());
        }
        if self.waiting {
            let silence = vec![0; self.pending.len()];
            self.core_frame.clear();
            self.core
                .encode(self.lossless.core_input(&silence), &mut self.core_frame);
        }
        if self.pool.is_some() {
            // The frames before the last went out in the calls after each;
            // the last, and the rest of the stream, are this call's.
            let last = self.handed_out();
            if self.waiting {
                self.hand_out();
                self.waiting = false;
            }
            self.settle_back(out, Until::Settled(last))?;
            let mark = out.len();
            let all = self.handed_out();
            let finished = self
                .settle_back(out, Until::Settled(all))
                .and_then(|()| self.emit(out, true));
            if finished.is_err() {
                out.truncate(mark);
            }
            return finished;
        }
        let mark = out.len();
        let finished = if self.waiting {
            self.send(out).inspect(|()| self.waiting = false)
        } else {
            Ok(())
        }
        .and_then(|()| self.emit(out, true));
        if finished.is_err() {
            out.truncate(mark);
        }
        finished
    }

    /// Frames handed to the lossless layer so far: the index of the next.
    fn handed_out(&self) -> u64 {
        self.pool.as_ref().map_or(self.frames, |pool| pool.queued)
    }

    /// Whether the lossless frame `index` may code channels against the
    /// core.
    ///
    /// FFmpeg's decoder treats the first frame of a stream that codes
    /// channels against the core in more than one channel set as one to
    /// recover from, and presents it from the core alone (as it does after a
    /// seek). A first frame that codes every channel as itself has nothing
    /// to recover, so the stream decodes losslessly from its first sample.
    fn against_core_at(&self, index: u64) -> bool {
        index > 0 || self.lossless.config().layout == Layout::FivePointOne
    }

    /// Encode the stream's frame for the pending input — the core frame last
    /// encoded and the lossless frame — put it in the queue, and send what
    /// is ready.
    fn send(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
        self.decoder.decode(&self.core);
        let core = self.against_core_at(self.frames).then_some(&self.decoder);
        self.lossless.encode(&self.pending, core)?;
        let core_frame = std::mem::take(&mut self.core_frame);
        let queued = self.queue(&core_frame, out);
        self.core_frame = core_frame;
        queued
    }

    /// Hand the pending input to the pool to code, with the core frame last
    /// encoded and the core's reconstruction of the input.
    fn hand_out(&mut self) {
        self.decoder.decode(&self.core);
        let pool = self.pool.as_mut().expect("handing out to the pool");
        let index = pool.queued;
        let spare = pool.spare.pop();
        let mut job = spare.unwrap_or_else(|| Job {
            index: 0,
            pcm: Vec::with_capacity(self.pending.len()),
            core: Box::new(CoreDecoder::new()),
            against: false,
            core_frame: Vec::with_capacity(self.core_frame.len()),
            coded: self.lossless.coded(),
        });
        job.index = index;
        job.pcm.clear();
        job.pcm.extend_from_slice(&self.pending);
        job.against = self.against_core_at(index);
        if job.against {
            job.core.as_mut().clone_from(&self.decoder);
        }
        job.core_frame.clear();
        job.core_frame.extend_from_slice(&self.core_frame);
        let pool = self.pool.as_mut().expect("handing out to the pool");
        pool.jobs
            .as_ref()
            .expect("the pool is open while the encoder is")
            .send(job)
            .expect("the pool's threads are running");
        pool.queued += 1;
        pool.in_flight += 1;
    }

    /// Settle, in order, the frames the pool has coded, and send what is
    /// ready — waiting for the pool until `until`. A frame refused leaves
    /// `out` as the frames before it left it, and ends the stream.
    fn settle_back(&mut self, out: &mut Vec<u8>, until: Until) -> Result<(), Refused> {
        loop {
            let settled = self.frames;
            let pool = self.pool.as_mut().expect("settling from the pool");
            while let Ok(job) = pool.done.try_recv() {
                pool.place(job, settled);
            }
            while let Some(Some(_)) = self.pool.as_ref().and_then(|p| p.back.front()) {
                let pool = self.pool.as_mut().expect("settling from the pool");
                let mut job = pool
                    .back
                    .pop_front()
                    .flatten()
                    .expect("the next frame is back");
                pool.in_flight -= 1;
                let mark = out.len();
                let sent = self.settle(&mut job, out);
                self.pool
                    .as_mut()
                    .expect("settling from the pool")
                    .spare
                    .push(job);
                if let Err(refused) = sent {
                    out.truncate(mark);
                    self.ended = Some(refused.clone());
                    return Err(refused);
                }
            }
            let settled = self.frames;
            let pool = self.pool.as_mut().expect("settling from the pool");
            let enough = match until {
                Until::Room => pool.in_flight <= pool.most,
                Until::Settled(frames) => settled >= frames,
            };
            if enough {
                return Ok(());
            }
            let job = pool
                .done
                .recv()
                .expect("the pool's threads give back every frame");
            pool.place(job, settled);
        }
    }

    /// Settle a frame the pool coded, put it in the queue beside its core
    /// frame, and send what is ready.
    fn settle(&mut self, job: &mut Job, out: &mut Vec<u8>) -> Result<(), Refused> {
        debug_assert_eq!(job.index, self.frames, "frames are settled in order");
        self.lossless.settle_coded(&mut job.coded)?;
        self.queue(&job.core_frame, out)
    }

    /// Put the lossless frame last written in the queue beside `core_frame`,
    /// and send what is ready.
    fn queue(&mut self, core_frame: &[u8], out: &mut Vec<u8>) -> Result<(), Refused> {
        let frame = self.lossless.frame();
        let size = frame.len();
        self.lossless_stream.extend(frame);
        let word = self.lossless.extension().map(|navigation: Navigation| {
            let (offset, size) = self.reach.unwrap_or_default();
            self.reach = Some((offset.max(navigation.offset), size.max(navigation.size)));
            navigation
                .word()
                .expect("the lossless encoder refuses a block it cannot navigate")
        });
        self.words.push_back(word);
        self.cores.extend(core_frame);
        self.smoother.push(size);
        self.frames += 1;
        self.lossless_bytes += size as u64;
        self.against_core += u64::from(self.lossless.against_core());
        self.emit(out, false)
    }

    /// Send the frames whose substreams can be decided — all of them when
    /// `flush` — each as its core frame, the substream header and the bytes
    /// of the lossless stream the smoother gives it.
    fn emit(&mut self, out: &mut Vec<u8>, flush: bool) -> Result<(), Refused> {
        while self.smoother.ready() || (flush && self.smoother.waiting() > 0) {
            let carry = match self.smoother.carry().expect("a frame is waiting") {
                Ok(carry) => carry,
                Err((frame, cause)) => {
                    let refused = Refused::PeakRate { frame, cause };
                    self.ended = Some(refused.clone());
                    return Err(refused);
                }
            };
            let bytes = carry.bytes as u32;
            self.exss.asset_sizes[0] = bytes;
            if let Coding::Components { xll: Some(x), .. } = &mut self.exss.assets[0].coding {
                x.size = bytes;
                x.sync = carry.sync.map(|s| XllSync {
                    peak_buffer_code: self.buffer_code,
                    delay_bits: peak::delay_bits(s.delay),
                    delay_frames: s.delay,
                    sync_offset: s.offset,
                });
            }
            // The word, when the frame has one, describes the frame the
            // decoder takes out of its buffer with this substream.
            self.exss.assets[0].tail = match self.words.pop_front().flatten() {
                Some(word) => tail(word),
                None => Bits::new(),
            };
            let header = header(&mut self.exss, bytes);
            let start = out.len();
            let core = self.core.frame_size();
            out.extend(self.cores.drain(..core));
            out.extend_from_slice(&header);
            out.extend(self.lossless_stream.drain(..carry.bytes));
            let sent = out.len() - start;
            debug_assert!(
                sent <= self.rate.budget,
                "{sent} bytes over the {}-byte budget",
                self.rate.budget
            );
            self.rate.largest = self.rate.largest.max(sent);
        }
        self.rate.held = self.smoother.peak();
        self.rate.ahead = self.smoother.ahead();
        Ok(())
    }

    /// How the stream has used its ceiling so far.
    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// Bytes of the lossless frames encoded so far, headers included.
    pub fn lossless_bytes(&self) -> u64 {
        self.lossless_bytes
    }

    /// Channels of the frames sent so far that were coded against the core,
    /// summed over the frames.
    pub fn against_core(&self) -> u64 {
        self.against_core
    }

    /// For a 7.1.4, the furthest the navigation word has had to point and
    /// the largest block it has had to describe, in bytes: how close the
    /// stream has come to [`crate::navigation::MAX_OFFSET`] and
    /// [`crate::navigation::MAX_SIZE`].
    pub fn extension_reach(&self) -> Option<(usize, usize)> {
        self.reach
    }

    /// Bytes of the core in every frame.
    pub fn core_frame_size(&self) -> usize {
        self.core.frame_size()
    }
}

/// The lossless asset descriptor's tail when the asset carries the DTS:X
/// extension: two zero bits and the navigation word, as every frame of the
/// standard-profile reference streams has it. What the two bits are is not
/// established. [`ExssHeader::fit`] pads it to the descriptor's byte.
fn tail(word: u64) -> Bits {
    let mut w = hz_core::bits::BitWriter::new();
    w.put(2, 0);
    w.put(32, (word >> 32) as u32);
    w.put(32, word as u32);
    let bytes = w.padded();
    Bits::read(&mut hz_core::bits_read::BitReader::new(&bytes), 66).expect("66 bits were written")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{split, xll};

    /// Tones, so that the core is close and the difference from it cheap.
    fn tones(channels: usize, frames: usize) -> Vec<i32> {
        (0..frames * FRAME_SAMPLES * channels)
            .map(|i| {
                let (n, ch) = (i / channels, i % channels);
                let phase = std::f64::consts::TAU * 110.0 * (ch + 1) as f64 * n as f64 / 48000.0;
                ((phase.sin() * 0.4 * f64::from(1 << 23)) as i32) << 8
            })
            .collect()
    }

    /// The residual masks of every lossless frame's first channel set.
    fn residual_masks(stream: &[u8]) -> Vec<u16> {
        split::frames(stream)
            .map(|frame| {
                let exss = frame.unwrap().exss.unwrap();
                let header = ExssHeader::read(exss).unwrap();
                let at = header.header_size as usize;
                let xll = &exss[at..at + header.asset_sizes[0] as usize];
                xll::Headers::read(xll, true).unwrap().channel_sets[0].residual_encode
            })
            .collect()
    }

    #[test]
    fn a_frame_goes_out_per_frame_in_the_first_coded_alone_in_a_7_1() {
        for layout in [Layout::FivePointOne, Layout::SevenPointOne] {
            let channels = layout.channels();
            let mut encoder = Encoder::new(Config::reference(layout, 24)).unwrap();
            let mut stream = Vec::new();
            for (k, frame) in tones(channels, 5)
                .chunks(FRAME_SAMPLES * channels)
                .enumerate()
            {
                encoder.encode(frame, &mut stream).unwrap();
                assert_eq!(
                    stream.is_empty(),
                    k == 0,
                    "{layout:?}: the core runs a frame ahead"
                );
            }
            encoder.finish(&mut stream).unwrap();
            let masks = residual_masks(&stream);
            assert_eq!(masks.len(), 5, "{layout:?}");
            // FFmpeg would recover from a 7.1's first frame coded against the
            // core; a 5.1's it decodes as it is.
            assert_eq!(
                masks[0] == 0x3f,
                layout == Layout::SevenPointOne,
                "{layout:?}"
            );
            assert!(
                masks[1..].iter().all(|&m| m != 0x3f),
                "{layout:?}: {masks:x?}"
            );
            // Nothing is left to send.
            let sent = stream.len();
            encoder.finish(&mut stream).unwrap();
            assert_eq!(stream.len(), sent);
        }
    }

    #[test]
    fn full_coding_codes_no_channel_against_the_core() {
        let mut config = Config::reference(Layout::SevenPointOne, 24);
        config.lossless.residual = false;
        let mut encoder = Encoder::new(config).unwrap();
        let mut stream = Vec::new();
        for frame in tones(8, 3).chunks(FRAME_SAMPLES * 8) {
            encoder.encode(frame, &mut stream).unwrap();
        }
        encoder.finish(&mut stream).unwrap();
        assert_eq!(residual_masks(&stream), [0x3f; 3]);
        assert_eq!(encoder.against_core(), 0);
    }

    #[test]
    fn a_refused_frame_moves_nothing_on() {
        let mut encoder = Encoder::new(Config::reference(Layout::FivePointOne, 16)).unwrap();
        let mut stream = Vec::new();
        let mut pcm: Vec<i32> = tones(6, 1).iter().map(|s| s & !0xffff).collect();
        encoder.encode(&pcm, &mut stream).unwrap();
        pcm[7] |= 0x100;
        assert!(encoder.encode(&pcm, &mut stream).is_err());
        assert!(stream.is_empty());
        pcm[7] &= !0xffff;
        encoder.encode(&pcm, &mut stream).unwrap();
        encoder.finish(&mut stream).unwrap();
        assert_eq!(residual_masks(&stream).len(), 2);
    }

    /// A 7.1.4 is the standard DTS:X profile: the asset states the 7.1, its
    /// descriptor's tail is two zero bits, the navigation word and the
    /// padding to the byte, and the word points at the block that ends the
    /// lossless frame. Every substream header ends on a 32-bit word, as the
    /// reference streams' do.
    #[test]
    fn a_7_1_4_carries_the_word_that_locates_its_heights() {
        for layout in [
            Layout::FivePointOne,
            Layout::SevenPointOne,
            Layout::SevenPointOneFour,
        ] {
            let channels = layout.channels();
            let mut encoder = Encoder::new(Config::reference(layout, 24)).unwrap();
            let mut stream = Vec::new();
            let pcm: Vec<i32> = tones(channels, 4).iter().map(|&s| s >> 1 & !0xff).collect();
            for frame in pcm.chunks(FRAME_SAMPLES * channels) {
                encoder.encode(frame, &mut stream).unwrap();
            }
            encoder.finish(&mut stream).unwrap();
            let mut frames = 0;
            for frame in split::frames(&stream) {
                let exss = frame.unwrap().exss.unwrap();
                let header = ExssHeader::read(exss).unwrap();
                assert_eq!(header.header_size % 4, 0, "{layout:?}");
                let asset = &header.assets[0];
                let statics = asset.statics.as_ref().unwrap();
                assert_eq!(statics.channels, 8.min(channels as u16));
                let tail = &asset.tail;
                if layout != Layout::SevenPointOneFour {
                    // The padding to the descriptor's byte, and nothing else.
                    assert!(tail.len() < 8 && tail.is_zero(), "{layout:?}");
                    continue;
                }
                assert_eq!((tail.len(), tail.bytes()[0] >> 6), (69, 0));
                let nav = crate::navigation::Navigation::read(tail.word64(2).unwrap()).unwrap();
                let at = header.header_size as usize;
                let xll = &exss[at..at + header.asset_sizes[0] as usize];
                assert_eq!(nav.offset + nav.size, xll.len());
                assert_eq!(
                    u32::from_be_bytes(xll[nav.offset..nav.offset + 4].try_into().unwrap()),
                    crate::dtsx::SYNC
                );
                frames += 1;
            }
            if layout == Layout::SevenPointOneFour {
                assert_eq!(frames, 4);
                let (offset, size) = encoder.extension_reach().unwrap();
                assert!(offset <= crate::navigation::MAX_OFFSET && size > 22);
            }
        }
    }

    /// Tones with a burst of full-scale white noise in every channel from
    /// frame 10 to 13, all `shift` bits down, 24 bits.
    fn bursts(channels: usize, frames: usize, shift: u32) -> Vec<i32> {
        let mut state = 0x2545_f491u32;
        let tones = tones(channels, frames);
        tones
            .iter()
            .enumerate()
            .map(|(i, &tone)| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let frame = i / channels / FRAME_SAMPLES;
                if (10..14).contains(&frame) {
                    (state as i32) >> shift >> 8 << 8
                } else {
                    tone >> shift >> 8 << 8
                }
            })
            .collect()
    }

    fn encode(config: Config, pcm: &[i32]) -> (Vec<u8>, Result<(), Refused>, Rate) {
        let (stream, result, rate, _) = encode_on(config, pcm, 0);
        (stream, result, rate)
    }

    /// As [`encode`], the lossless frames on `threads` threads, with what
    /// the encoder counted: lossless bytes, channels against the core, and
    /// the extension's reach.
    #[allow(clippy::type_complexity)]
    fn encode_on(
        config: Config,
        pcm: &[i32],
        threads: usize,
    ) -> (
        Vec<u8>,
        Result<(), Refused>,
        Rate,
        (u64, u64, Option<(usize, usize)>),
    ) {
        let channels = config.lossless.layout.channels();
        let mut encoder = Encoder::new(config).unwrap().with_threads(threads);
        let counted = |encoder: &Encoder| {
            (
                encoder.lossless_bytes(),
                encoder.against_core(),
                encoder.extension_reach(),
            )
        };
        let mut stream = Vec::new();
        for frame in pcm.chunks(FRAME_SAMPLES * channels) {
            if let Err(e) = encoder.encode(frame, &mut stream) {
                return (stream, Err(e), encoder.rate(), counted(&encoder));
            }
        }
        let result = encoder.finish(&mut stream);
        (stream, result, encoder.rate(), counted(&encoder))
    }

    /// The lossless frames coded on threads and settled in order are the
    /// stream coded on one, to the byte — smoothed, refused, or neither —
    /// and a refusal leaves the same frames written before it.
    #[test]
    fn threads_write_the_stream_one_thread_writes() {
        let smoothed = |layout: Layout, ceiling: u32| {
            let mut config = Config::reference(layout, 24);
            config.peak.ceiling = ceiling;
            config.peak.buffer_code = 1;
            config.peak.lookahead = 4;
            config
        };
        let cases = [
            (Config::reference(Layout::FivePointOne, 24), tones(6, 25)),
            (Config::reference(Layout::FivePointOne, 16), {
                tones(6, 9).iter().map(|s| s & !0xffff).collect()
            }),
            (
                Config::reference(Layout::SevenPointOne, 24),
                bursts(8, 30, 2),
            ),
            (
                Config::reference(Layout::SevenPointOneFour, 24),
                bursts(12, 30, 10),
            ),
            // Refused for the peak rate, with frames sent ahead before it.
            (smoothed(Layout::SevenPointOne, 7_000_000), bursts(8, 30, 0)),
            // Refused for the peak rate once the input has ended, by the
            // flush.
            (smoothed(Layout::SevenPointOne, 7_000_000), bursts(8, 13, 0)),
            // Sent ahead, and not refused.
            (
                {
                    let mut config = Config::reference(Layout::SevenPointOne, 24);
                    config.peak.ceiling = 8_500_000;
                    config.peak.lookahead = 16;
                    config
                },
                bursts(8, 40, 0),
            ),
            // A corner past 24 bits with its height: refused by the check.
            (
                Config::reference(Layout::SevenPointOneFour, 24),
                bursts(12, 30, 0),
            ),
            // A bed too large for the navigation word to reach past.
            (
                Config::reference(Layout::SevenPointOneFour, 24),
                bursts(12, 30, 1),
            ),
        ];
        let mut refusals = Vec::new();
        let mut sent_ahead = false;
        for (case, (config, pcm)) in cases.into_iter().enumerate() {
            let alone = encode_on(config.clone(), &pcm, 0);
            match &alone.1 {
                Err(e) => refusals.push(std::mem::discriminant(e)),
                Ok(()) => sent_ahead |= alone.2.ahead > 0,
            }
            for threads in [1, 3, 8] {
                let spread = encode_on(config.clone(), &pcm, threads);
                assert!(
                    spread.0 == alone.0,
                    "case {case}, {threads} threads: the stream"
                );
                assert_eq!(spread.1, alone.1, "case {case}, {threads} threads");
                if alone.1.is_ok() {
                    assert_eq!(spread.2, alone.2, "case {case}, {threads} threads");
                    assert_eq!(spread.3, alone.3, "case {case}, {threads} threads");
                }
            }
        }
        assert!(sent_ahead, "no case sent a frame ahead and went on");
        // Every way a frame is refused was taken.
        for refused in [
            Refused::Fold {
                frame: 0,
                channel: 0,
                sample: 0,
                value: 0,
            },
            Refused::Navigation {
                frame: 0,
                cause: crate::navigation::OutOfRange::Offset(0),
            },
            Refused::PeakRate {
                frame: 0,
                cause: peak::Overrun::Buffer {
                    held: 0,
                    capacity: 0,
                },
            },
        ] {
            assert!(
                refusals.contains(&std::mem::discriminant(&refused)),
                "no case refused with {refused:?}"
            );
        }
    }

    /// Each frame's bytes, and its substream's lossless data, sync fields
    /// and descriptor tail.
    fn substreams(stream: &[u8]) -> Vec<(usize, Vec<u8>, Option<XllSync>, Bits)> {
        split::frames(stream)
            .map(|frame| {
                let frame = frame.unwrap();
                let exss = frame.exss.unwrap();
                let header = ExssHeader::read(exss).unwrap();
                let at = header.header_size as usize;
                let data = exss[at..at + header.asset_sizes[0] as usize].to_vec();
                let asset = &header.assets[0];
                let sync = asset.coding.xll().unwrap().sync.clone();
                let bytes = frame.core.unwrap().len() + exss.len();
                (bytes, data, sync, asset.tail.clone())
            })
            .collect()
    }

    /// Under a ceiling the noise does not fit, its frames are sent partly
    /// ahead, and the decoder's buffer gets back exactly the lossless frames
    /// a stream that never smooths carries, each with its own substream —
    /// and, for a 7.1.4, the navigation word of each substream points at
    /// the block of the frame decoded with it. No frame passes the budget,
    /// the stream states the buffer it uses and holds no more, and a
    /// decoder that starts at any substream with a sync takes out the frame
    /// its delay names.
    #[test]
    fn a_lowered_ceiling_sends_frames_ahead_and_every_frame_comes_back_whole() {
        for (layout, shift, ceiling) in [
            (Layout::SevenPointOne, 0, 8_500_000),
            (Layout::SevenPointOneFour, 10, 8_000_000),
        ] {
            let pcm = bursts(layout.channels(), 40, shift);
            let (plain, result, rate) = encode(Config::reference(layout, 24), &pcm);
            result.unwrap();
            assert_eq!((rate.buffer, rate.ahead), (0, 0), "{layout:?}");
            let frames: Vec<Vec<u8>> = substreams(&plain).into_iter().map(|s| s.1).collect();
            let largest = frames.iter().map(Vec::len).max().unwrap();

            let mut config = Config::reference(layout, 24);
            config.peak.ceiling = ceiling;
            config.peak.lookahead = 16;
            let (smoothed, result, rate) = encode(config, &pcm);
            result.unwrap_or_else(|e| panic!("{layout:?}: {e}"));
            let budget = peak::frame_budget(ceiling, 512, 48000);
            assert!(
                largest + 2012 > budget,
                "{layout:?}: the noise has to need it"
            );
            assert!(
                rate.ahead > 0 && rate.largest <= budget,
                "{layout:?}: {rate:?}"
            );
            assert_eq!(rate.buffer, 128 << 10);

            let substreams = substreams(&smoothed);
            assert_eq!(substreams.len(), frames.len());
            let mut buffer = peak::DecoderBuffer::new();
            for (n, (bytes, data, sync, tail)) in substreams.iter().enumerate() {
                assert!(*bytes <= budget, "{layout:?}: frame {n} is {bytes} bytes");
                if let Some(s) = sync {
                    assert_eq!(s.peak_buffer_code, 8);
                    assert_eq!(s.delay_bits, peak::delay_bits(s.delay_frames));
                }
                assert_eq!(
                    buffer.push(data, sync.as_ref()),
                    Ok(peak::Taken::Frame(frames[n].len())),
                    "{layout:?}: substream {n}"
                );
                assert_eq!(buffer.frame(), &frames[n][..], "{layout:?}: substream {n}");
                if layout == Layout::SevenPointOneFour {
                    let nav = crate::navigation::Navigation::read(tail.word64(2).unwrap()).unwrap();
                    assert_eq!(nav.offset + nav.size, frames[n].len());
                    assert_eq!(
                        &frames[n][nav.offset..nav.offset + 4],
                        &crate::dtsx::SYNC.to_be_bytes()
                    );
                }
            }
            assert!(buffer.peak() <= rate.buffer && buffer.peak() as u64 == rate.held);
            for (n, (_, data, sync, _)) in substreams.iter().enumerate() {
                let Some(s) = sync else { continue };
                let due = n + s.delay_frames as usize;
                let mut joining = peak::DecoderBuffer::new();
                let mut taken = joining.push(data, Some(s));
                for (_, data, sync, _) in &substreams[n + 1..=due] {
                    taken = joining.push(data, sync.as_ref());
                }
                assert_eq!(taken, Ok(peak::Taken::Frame(frames[due].len())));
                assert_eq!(
                    joining.frame(),
                    &frames[due][..],
                    "{layout:?}: joined at {n}"
                );
            }
        }
    }

    /// A frame that cannot reach the decoder in time is refused, named, and
    /// the stream ends there: what was sent stays, nothing more is.
    #[test]
    fn a_frame_past_the_ceiling_ends_the_stream() {
        let pcm = bursts(8, 30, 0);
        let mut config = Config::reference(Layout::SevenPointOne, 24);
        config.peak.ceiling = 7_000_000;
        config.peak.buffer_code = 1;
        config.peak.lookahead = 4;
        let mut encoder = Encoder::new(config).unwrap();
        let mut stream = Vec::new();
        let mut refused = None;
        for (k, frame) in pcm.chunks(FRAME_SAMPLES * 8).enumerate() {
            if let Err(e) = encoder.encode(frame, &mut stream) {
                refused = Some((k, e));
                break;
            }
        }
        let (k, e) = refused.expect("the noise does not fit");
        let Refused::PeakRate { frame, .. } = e else {
            panic!("{e}")
        };
        assert!(
            (10..14).contains(&frame) && k as u64 <= frame + 6,
            "{k}, {e}"
        );
        let sent = stream.len();
        assert!(matches!(
            encoder.encode(&pcm[..FRAME_SAMPLES * 8], &mut stream),
            Err(Refused::PeakRate { .. })
        ));
        assert!(encoder.finish(&mut stream).is_err());
        assert_eq!(stream.len(), sent);
        assert!((substreams(&stream).len() as u64) < frame);
    }

    /// At the default ceiling no frame the lossless layer can write is too
    /// large for its substream: the stream states no buffer, and every frame
    /// goes out as soon as it is encoded, as before.
    #[test]
    fn the_default_ceiling_needs_no_buffer() {
        for layout in [
            Layout::FivePointOne,
            Layout::SevenPointOne,
            Layout::SevenPointOneFour,
        ] {
            let encoder = Encoder::new(Config::reference(layout, 24)).unwrap();
            assert_eq!(encoder.rate().buffer, 0, "{layout:?}");
            assert!(encoder.lossless.frame_bound() + 2012 + 48 < encoder.rate().budget);
        }
        // No room beside the core — none at all, or not even for the
        // headers — is refused, not panicked on.
        for ceiling in [0, 1, 749, 750, 1_500_000, 1_520_000] {
            let mut config = Config::reference(Layout::SevenPointOneFour, 24);
            config.peak.ceiling = ceiling;
            assert!(Encoder::new(config).is_err(), "{ceiling} b/s");
        }
        // A ceiling past what a substream's size field describes caps the
        // room at what it does.
        for ceiling in [800_000_000, u32::MAX] {
            let mut config = Config::reference(Layout::SevenPointOne, 24);
            config.peak.ceiling = ceiling;
            let encoder = Encoder::new(config).expect("a ceiling past any frame");
            assert_eq!(encoder.rate().buffer, 0, "{ceiling} b/s");
        }
        let mut config = Config::reference(Layout::SevenPointOne, 24);
        config.peak.lookahead = usize::MAX;
        assert!(Encoder::new(config).is_err(), "an endless lookahead");
    }

    /// A corner its height takes past 24 bits is refused, naming the input
    /// frame it is in — not the frame the lossless layer last coded, which
    /// is the one before, still waiting for its core.
    #[test]
    fn a_fold_past_24_bits_names_its_input_frame() {
        let mut encoder = Encoder::new(Config::reference(Layout::SevenPointOneFour, 24)).unwrap();
        let mut stream = Vec::new();
        let mut pcm = vec![0i32; FRAME_SAMPLES * 12];
        for _ in 0..2 {
            encoder.encode(&pcm, &mut stream).unwrap();
        }
        // L at full scale and TFL at full scale: 1.707.
        pcm[7 * 12] = ((1 << 23) - 1) << 8;
        pcm[7 * 12 + 8] = ((1 << 23) - 1) << 8;
        match encoder.encode(&pcm, &mut stream) {
            Err(Refused::Fold {
                frame: 2,
                channel: 0,
                sample: 7,
                ..
            }) => {}
            other => panic!("{other:?}"),
        }
    }
}
