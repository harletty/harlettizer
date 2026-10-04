//! Mixing a programme's objects into elements, a block at a time.
//!
//! Two things mix: a **fold**, which places fewer elements than there are
//! objects and folds every object into them (`hz_cluster`), and an
//! **overlay**, which keeps the master's elements and pans its last few
//! objects onto them ([`crate::overlay`]). Whatever decides the weights, a
//! block goes the same way: the objects' samples and the scene they make,
//! the weights ramping from the block before, a limiter on what the sum
//! leaves, and the metric's account of what it cost on every presentation
//! the stream is played through. [`Mixer`] is that, and the buffers it
//! keeps across blocks; what a writer codes the elements as is the writer's.

use crate::overlay::{self, Overlaid};
use hz_cluster::Clustering;
use hz_cluster::floor::Floor;
use hz_cluster::mix::Limiter;
use hz_cluster::scene::{Loudness, Scene};
use hz_core::Result;

/// How a mix keeps its elements inside the codec's domain.
///
/// An element is a sum, and two coherent objects in one element pass full
/// scale. The fit can **bound** an element's coherent peak to the domain by
/// solving the objects it carries under an upper bound on their weights —
/// see `hz_cluster::headroom` — and a **limiter** with one gain for every
/// element can hold whatever is left, ahead of the peak. The limiter alone
/// by default, because the metric says so: the bound is on the coherent
/// worst case, which forty tones in nine contributions an element pass in
/// every block while the mix itself passes full scale in few, and it costs
/// a fifth of the fold's mean for half the clips; the limiter takes every
/// clip for nothing the metric sees, and what it costs — a gain that dips
/// at the peaks — the report states. See `docs/clustering.md`. Both, the
/// bound alone, or neither, for the measurement; with neither, the mix is
/// clamped and the clips counted, which is what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Headroom {
    Both,
    Bound,
    #[default]
    Limit,
    Off,
}

impl Headroom {
    /// Whether the fit bounds an element's coherent peak.
    pub fn bounds(self) -> bool {
        matches!(self, Self::Both | Self::Bound)
    }

    /// Whether a limiter holds what the sum leaves.
    pub fn limits(self) -> bool {
        matches!(self, Self::Both | Self::Limit)
    }
}

/// What a mixed element's samples are rounded to, in bits, unless asked
/// otherwise.
///
/// # Why a mix rounds at all
///
/// Sixteen objects do not go into eleven elements without loss — that is what
/// a fold *is* — and once a step has accepted losing the difference between
/// two directions, keeping the difference between two values 2^-24 apart is
/// not a principle, it is an oversight. A mix of objects fills all
/// twenty-four bits with a product of real weights, and a lossless coder then
/// has to carry every one of them: the low four are noise from the
/// multiplication and cost about a fifth of the stream.
///
/// Measured on a two-hour programme folded 17 → 12: the reference encoder's
/// elements have exactly four dead low bits everywhere, ours had none, and
/// the difference accounts for the whole of a 1.33× size gap — our coder is
/// 0.95× the reference on identical audio. See `docs/encode.md`.
///
/// Twenty bits puts the rounding noise at about −120 dBFS, under the
/// programme's own noise floor and under what any playback chain resolves.
/// `--full-depth` declines the trade.
pub const FOLD_BITS: u32 = 20;

/// What shapes a mix, whichever writer it is for: what its elements are
/// rounded to, what counts as audible, how they are kept inside the domain,
/// and how a block's power is weighed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mixing {
    /// What the mixed elements are rounded to, in bits — see [`FOLD_BITS`].
    pub fold_depth: u32,
    /// The programme's dialnorm in decibels, which sets the floor under what
    /// an object has to carry to be heard at the playback level — see
    /// `hz_cluster::floor`.
    pub dialnorm: f64,
    pub headroom: Headroom,
    /// How a block's power is weighed — see `hz_cluster::scene::Loudness`.
    pub weighing: Loudness,
}

impl Default for Mixing {
    fn default() -> Self {
        Self {
            fold_depth: FOLD_BITS,
            dialnorm: -31.0,
            headroom: Headroom::default(),
            weighing: Loudness::Flat,
        }
    }
}

/// What the mix has cost so far, over every presentation the stream will be
/// played through. A writer that folds a scene and does not say what it
/// cost is asking to be trusted.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    /// Blocks judged.
    pub blocks: u64,
    /// The blocks' mean error, summed: [`Cost::mean`] divides it.
    pub sum: f64,
    /// The worst any block fared.
    pub worst: f64,
    /// Objects under the room's floor, summed over the blocks.
    pub inaudible: u64,
}

impl Cost {
    /// The mean over the blocks judged; nought before any was.
    pub fn mean(&self) -> f64 {
        if self.blocks == 0 {
            0.0
        } else {
            self.sum / self.blocks as f64
        }
    }

    fn judged(&mut self, report: &hz_cluster::metric::Report) {
        self.blocks += 1;
        self.sum += report.mean;
        self.worst = self.worst.max(report.worst);
        self.inaudible += report.inaudible as u64;
    }

    fn spent(&mut self, spent: overlay::Spent) {
        self.blocks += 1;
        if let Some((mean, worst)) = spent.cost {
            self.sum += mean;
            self.worst = self.worst.max(worst);
        }
    }
}

/// Everything a block-at-a-time mix keeps across blocks.
///
/// These were allocations a block in the hot path, and are reused buffers,
/// refilled once a block. The scene is held rather than built because the
/// energy it computes may be K-weighted and a filter has state — and because
/// it is the *same* scene `cargo xtask cluster` builds, which is the only way
/// the harness's report is a report about the stream. See
/// [`hz_cluster::scene`].
pub struct Mixer {
    /// The block's scene: every object pushed with where it is and its
    /// samples, and finished, before the weights are decided.
    pub scene: Scene,
    /// Every object's samples for the block, as the mix takes them.
    pub signals: Vec<Vec<f32>>,
    /// Each object's gain, applied before its weights.
    pub gains: Vec<f64>,
    /// Each element's mixed samples.
    pub mixed: Vec<Vec<f32>>,
    /// The previous block's weights, which this one ramps from.
    pub from: Vec<Vec<f64>>,
    /// The limiter on what the sum leaves, when there is one — see
    /// [`Headroom`].
    pub limiter: Option<Limiter>,
    /// What an object has to carry to be heard — see `hz_cluster::floor`.
    pub floor: Floor,
    pub renderers: Vec<Box<dyn hz_render::Renderer>>,
    pub cost: Cost,
}

impl Mixer {
    /// A mixer for `width` elements, its scene weighing a block's power
    /// `weighing`'s way at `sample_rate`, its floor `dialnorm`'s and a
    /// limiter when `limit` asks for one.
    pub fn new(
        sample_rate: f64,
        weighing: Loudness,
        dialnorm: f64,
        limit: bool,
        width: usize,
    ) -> Result<Self> {
        Ok(Self {
            scene: Scene::weighed(sample_rate, weighing)?,
            signals: Vec::new(),
            gains: Vec::new(),
            mixed: vec![Vec::new(); width],
            from: Vec::new(),
            limiter: limit.then(Limiter::new),
            floor: Floor::for_dialnorm(dialnorm),
            renderers: hz_cluster::metric::delivery()?,
            cost: Cost::default(),
        })
    }

    /// Mix the block through `clustering`: the weights ramping from
    /// `previous`'s — or from this block's own at the very start, where there
    /// is nothing to ramp from — and the limiter, when there is one, holding
    /// the sum under `ceiling`, a fraction of full scale. Returns whether it
    /// brought the elements down.
    pub fn fold(
        &mut self,
        clustering: &Clustering,
        previous: Option<&Clustering>,
        frames: usize,
        ceiling: f64,
    ) -> bool {
        self.from.clear();
        match previous {
            Some(previous) if previous.weights.len() == clustering.weights.len() => {
                self.from.extend(previous.weights.iter().cloned());
            }
            _ => self.from.extend(clustering.weights.iter().cloned()),
        }
        hz_cluster::mix::mix(
            &self.signals,
            &self.gains,
            &self.from,
            &clustering.weights,
            frames,
            &mut self.mixed,
        );
        self.limiter
            .as_mut()
            .is_some_and(|limiter| limiter.apply(&mut self.mixed, ceiling))
    }

    /// What `clustering` cost on the block's scene, into [`Mixer::cost`]. A
    /// block the metric cannot judge is left out of it.
    pub fn judge(&mut self, clustering: &Clustering) {
        if let Ok(report) = hz_cluster::metric::error_with(
            self.scene.objects(),
            clustering,
            &self.renderers,
            &self.floor,
        ) {
            self.cost.judged(&report);
        }
    }

    /// Decide one overlay block on the finished scene, and account for it —
    /// see [`Overlaid::span`].
    pub fn overlay(
        &mut self,
        overlaid: &mut Overlaid,
        block: overlay::Block,
        ceiling: f64,
    ) -> std::io::Result<()> {
        let spent = overlaid.span(
            block,
            overlay::Buffers {
                scene: &self.scene,
                signals: &self.signals,
                gains: &mut self.gains,
                mixed: &mut self.mixed,
                from: &mut self.from,
                limiter: self.limiter.as_mut(),
                ceiling,
                floor: &self.floor,
                renderers: &self.renderers,
            },
        )?;
        self.cost.spent(spent);
        Ok(())
    }
}
