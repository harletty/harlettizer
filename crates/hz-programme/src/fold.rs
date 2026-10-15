//! Where a fold puts its elements, block after block.
//!
//! A fold places fewer elements than there are objects, and places them
//! again every block. The placement is the clusterer's, warm-started from the
//! block before, and steered by the energies of the window around the block
//! rather than by the block alone — see `hz_cluster::smooth` — so that an
//! element moving because a sound *appears* is already there when it does.
//! That needs the energies of the blocks ahead before a block is placed,
//! which a writer gets by reading ahead of what it folds and recording each
//! block's energies as it is read ([`Placer::record`]). The rest — the mix,
//! the limiter, the cost — is [`crate::mix::Mixer`]'s.
//!
//! [`Folder`] is the whole of it for a writer whose sources are mono tracks
//! of the master — bed channels and objects on their paths — and whose
//! elements are the fold's mix and nothing else: it reads a block, records
//! what it weighs, holds it until the blocks its window reaches ahead are in,
//! and folds it. IAMF and E-AC-3's joint object coding fold this way; the
//! TrueHD encode, which carries its fold beside overlays and pinned beds,
//! drives a [`Placer`] of its own.

use crate::mix::{Mixer, Mixing};
use crate::path::Path;
use hz_cluster::scene::{Scene, Source as SceneSource};
use hz_cluster::smooth::{Smoother, Smoothing};
use hz_cluster::{Clusterer, Clustering, Object, Weighting};
use hz_core::Result;
use std::collections::VecDeque;

/// The clusterer, the window it is steered by, and the block it last placed.
pub struct Placer {
    clusterer: Clusterer,
    smoother: Smoother,
    /// The scene as the placement sees it — the block's geometry carrying the
    /// energies of the window around it. Refilled rather than rebuilt.
    placing: Vec<Object>,
    /// The last block's clustering: what the weights ramp *from*, and what
    /// the next one warm-starts from.
    previous: Option<Clustering>,
    /// How many blocks the fit bounded an element's coherent peak in.
    bounded: u64,
}

impl Placer {
    pub fn new(clusterer: Clusterer, smoothing: Smoothing) -> Self {
        Self {
            clusterer,
            smoother: Smoother::new(smoothing),
            placing: Vec::new(),
            previous: None,
            bounded: 0,
        }
    }

    pub fn smoothing(&self) -> Smoothing {
        self.smoother.smoothing()
    }

    /// Blocks of energy the placement wants to see beyond the one it is
    /// placing.
    pub fn look_ahead(&self) -> usize {
        self.smoother.smoothing().ahead
    }

    /// The energies of the next block read, in the order its objects are
    /// placed.
    pub fn record(&mut self, energies: &[f64]) {
        self.smoother.record(energies);
    }

    /// Place the next block, whose finished scene is `scene`: its geometry,
    /// and the energies of the window around it. The answer is not kept until
    /// [`Placer::keep`] — the mix ramps from the one before.
    pub fn place(&mut self, scene: &[Object]) -> Result<Clustering> {
        self.smoother.place(scene, &mut self.placing);
        let clustering = self
            .clusterer
            .cluster(&self.placing, self.previous.as_ref())?;
        if clustering.bounded > 0 {
            self.bounded += 1;
        }
        Ok(clustering)
    }

    /// The last block kept.
    pub fn previous(&self) -> Option<&Clustering> {
        self.previous.as_ref()
    }

    /// Keep `clustering` as the block placed last.
    pub fn keep(&mut self, clustering: Clustering) {
        self.previous = Some(clustering);
    }

    /// Blocks in which the fit bounded an element's coherent peak.
    pub fn bounded(&self) -> u64 {
        self.bounded
    }
}

/// What a fold takes from the master: a bed channel at its speaker, or an
/// object on its path.
pub enum Member {
    /// A bed channel, which never moves. A **pinned** one keeps an element
    /// of its own there and is never clustered — see
    /// `hz_cluster::Object::pinned`; a free one is folded as any object is.
    Bed { position: [f64; 3], pinned: bool },
    /// An object on its path, and where the walk along it stopped.
    Moving(Path, usize),
}

/// One block a [`Folder`] has read and not yet folded: each member's samples
/// with its gain on them, where it is at the block's end — what the
/// look-ahead weighed it by, kept so that the fold weighs the same thing —
/// and whatever the writer carries beside it.
#[derive(Default)]
pub struct Block<T> {
    /// The master's sample the block's first is: before the programme for
    /// a writer whose clock starts ahead of it.
    pub at: i64,
    pub frames: usize,
    /// The writer's own, read with the block and handed back when it is
    /// folded: the master's samples, or what a writer has to hold back as
    /// long as the fold looks ahead.
    pub carry: T,
    signals: Vec<Vec<f32>>,
    /// Where each member is, and where it is pinned when it is.
    places: Vec<([f64; 3], Option<[f64; 3]>)>,
}

impl<T> Block<T> {
    /// Member `index` of this block, as a scene takes it.
    fn source(&self, index: usize) -> SceneSource {
        let (position, pinned) = self.places[index];
        SceneSource {
            position,
            pinned,
            mode: if pinned.is_some() {
                hz_cluster::class::BED
            } else {
                hz_render::Mode::default()
            },
            ..SceneSource::default()
        }
    }

    /// Every member of the block into `scene`, started and finished.
    fn weigh(&self, scene: &mut Scene) {
        scene.start();
        for (index, signal) in self.signals.iter().enumerate() {
            scene.push(&self.source(index), signal.iter().map(|&x| f64::from(x)));
        }
        scene.finish();
    }
}

/// A master's members folded into fewer elements block by block: each block
/// placed by the energies of the window around it ([`Placer`]), which needs
/// the blocks ahead read before it is folded — so blocks go in
/// ([`Folder::read`]) a window ahead of the ones that come out
/// ([`Folder::fold`]).
pub struct Folder<T> {
    members: Vec<(usize, Member)>,
    pub placer: Placer,
    /// The fold's scene, mix and cost; after [`Folder::fold`], its `mixed`
    /// holds the block's elements.
    pub mixer: Mixer,
    /// What each block read weighs, for the look-ahead: its own scene,
    /// because a weighted scene's filter has state and this one reads ahead
    /// of the fold's.
    ahead: Scene,
    read: VecDeque<Block<T>>,
    spare: Vec<Block<T>>,
    energies: Vec<f64>,
}

impl<T: Default> Folder<T> {
    /// `members`, each with the master channel it is read from, folded into
    /// `elements` at `sample_rate` as `mixing` says: the dialnorm's floor,
    /// the weighing, and the headroom rule. What the mixed elements are
    /// rounded to is the writer's.
    pub fn new(
        members: Vec<(usize, Member)>,
        elements: usize,
        sample_rate: f64,
        mixing: &Mixing,
    ) -> Result<Self> {
        let mut mixer = Mixer::new(
            sample_rate,
            mixing.weighing,
            mixing.dialnorm,
            mixing.headroom.limits(),
            elements,
        )?;
        mixer.gains = vec![1.0; members.len()];
        Ok(Self {
            members,
            placer: Placer::new(
                Clusterer::new(elements, Weighting::Fitted)?
                    .flooring(hz_cluster::floor::Floor::for_dialnorm(mixing.dialnorm))
                    .bounding(mixing.headroom.bounds()),
                hz_cluster::smooth::SMOOTHING,
            ),
            mixer,
            ahead: Scene::weighed(sample_rate, mixing.weighing)?,
            read: VecDeque::new(),
            spare: Vec::new(),
            energies: Vec::new(),
        })
    }

    /// An empty block to read into, its carry as the last one folded left
    /// it: the writer fills the carry, then hands it to [`Folder::read`].
    pub fn block(&mut self) -> Block<T> {
        self.spare.pop().unwrap_or_default()
    }

    /// Read `block`, `frames` samples from the master's sample `at`:
    /// `sample(carry, n, channel)` is the master's sample `n` of the block on
    /// `channel`, as a fraction of full scale. Each member's samples take its
    /// gain, and the block's energies are recorded for the window.
    pub fn read(
        &mut self,
        mut block: Block<T>,
        at: i64,
        frames: usize,
        sample: impl Fn(&T, usize, usize) -> f64,
    ) {
        block.at = at;
        block.frames = frames;
        let Block {
            carry,
            signals,
            places,
            ..
        } = &mut block;
        signals.resize_with(self.members.len(), Vec::new);
        places.clear();
        // Before the programme an object is where it starts; its samples
        // there are the writer's to make silent.
        let clock = |t: i64| t.max(0) as u64;
        for (signal, (channel, member)) in signals.iter_mut().zip(&mut self.members) {
            signal.clear();
            let channel = *channel;
            places.push(match member {
                Member::Bed { position, pinned } => {
                    signal.extend((0..frames).map(|n| sample(carry, n, channel) as f32));
                    (*position, pinned.then_some(*position))
                }
                Member::Moving(path, hint) => {
                    signal.extend((0..frames).map(|n| {
                        let gain = path.gain(clock(at + n as i64), hint);
                        (sample(carry, n, channel) * gain) as f32
                    }));
                    let end = clock(at + frames as i64 - 1);
                    (path.segment(end, hint).at(end).position, None)
                }
            });
        }
        block.weigh(&mut self.ahead);
        self.energies.clear();
        self.energies
            .extend(self.ahead.objects().iter().map(|o| o.energy));
        self.placer.record(&self.energies);
        self.read.push_back(block);
    }

    /// Whether the oldest block read can be folded: the blocks its window
    /// reaches ahead are in.
    pub fn ready(&self) -> bool {
        self.read.len() > self.placer.look_ahead()
    }

    /// Blocks read and not yet folded.
    pub fn waiting(&self) -> usize {
        self.read.len()
    }

    /// Fold the oldest block read — once [`Folder::ready`], or at the end,
    /// for what there is — and hand it back with its carry: its elements are
    /// in `mixer.mixed`, mixed from the weights of the block before to its
    /// own with the limiter, when there is one, holding them under `ceiling`,
    /// a fraction of full scale; where each is at the block's end is
    /// `placer.previous()`'s. Nothing when no block is waiting. Hand the
    /// block back with [`Folder::recycle`] once done with it.
    pub fn fold(&mut self, ceiling: f64) -> Result<Option<Block<T>>> {
        let Some(mut block) = self.read.pop_front() else {
            return Ok(None);
        };
        block.weigh(&mut self.mixer.scene);
        let clustering = self.placer.place(self.mixer.scene.objects())?;
        std::mem::swap(&mut self.mixer.signals, &mut block.signals);
        self.mixer
            .fold(&clustering, self.placer.previous(), block.frames, ceiling);
        self.mixer.judge(&clustering);
        std::mem::swap(&mut self.mixer.signals, &mut block.signals);
        self.placer.keep(clustering);
        Ok(Some(block))
    }

    /// A folded block back, for its buffers to be read into again.
    pub fn recycle(&mut self, block: Block<T>) {
        self.spare.push(block);
    }

    /// Where each element is at the end of the block folded last.
    pub fn positions(&self) -> &[[f64; 3]] {
        self.placer.previous().map_or(&[], |at| &at.positions[..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block comes out once the blocks its window reaches ahead are in,
    /// and in the order they went in, each with what was carried beside it;
    /// at the end the rest come out for what there is.
    #[test]
    fn a_block_is_folded_once_its_window_is_read() {
        const MEMBERS: usize = 6;
        const FRAMES: usize = 256;
        let members = (0..MEMBERS)
            .map(|channel| {
                let x = channel as f64 / (MEMBERS - 1) as f64 * 2.0 - 1.0;
                (
                    channel,
                    Member::Bed {
                        position: [x, 1.0, 0.0],
                        pinned: false,
                    },
                )
            })
            .collect();
        let mut folder: Folder<usize> =
            Folder::new(members, 3, 48_000.0, &Mixing::default()).unwrap();
        let ahead = folder.placer.look_ahead();
        let mut seed = 1u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5
        };
        let mut folded = Vec::new();
        for k in 0..ahead + 3 {
            let samples: Vec<f64> = (0..FRAMES * MEMBERS).map(|_| noise()).collect();
            let mut block = folder.block();
            block.carry = k;
            folder.read(block, (k * FRAMES) as i64, FRAMES, |_, n, channel| {
                samples[n * MEMBERS + channel]
            });
            assert_eq!(folder.ready(), k >= ahead, "{k}");
            while folder.ready() {
                let block = folder.fold(1.0).unwrap().expect("a block");
                assert_eq!(folder.mixer.mixed.len(), 3);
                assert!(folder.mixer.mixed.iter().all(|m| m.len() == FRAMES));
                assert_eq!(folder.positions().len(), 3);
                folded.push(block.carry);
                folder.recycle(block);
            }
        }
        while let Some(block) = folder.fold(1.0).unwrap() {
            folded.push(block.carry);
            folder.recycle(block);
        }
        assert_eq!(folded, (0..ahead + 3).collect::<Vec<_>>());
    }
}
