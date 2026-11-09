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
use hz_render::{Keyframe, Mode};
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
    /// An object on its path, and where the walk along it stopped. Folded
    /// as a free object, whatever the master says of how it renders.
    Moving(Path, usize),
    /// An object whose rendering class the fold keeps apart, as the TrueHD
    /// encode keeps it — see `hz_cluster::class` — so that the element
    /// carrying it renders as it does.
    Classed(Box<Classed>),
}

/// An object, its path and its updates: the next one due and the last in
/// force, which says how it renders.
pub struct Classed {
    path: Path,
    hint: usize,
    keyframes: Vec<Keyframe>,
    next: usize,
    current: Keyframe,
}

impl Member {
    /// An object whose class the fold keeps, from its updates.
    pub fn classed(keyframes: Vec<Keyframe>) -> Self {
        Self::Classed(Box::new(Classed {
            path: Path::new(&keyframes),
            hint: 0,
            current: keyframes.first().copied().unwrap_or_default(),
            keyframes,
            next: 0,
        }))
    }
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
    /// Where each member is, where it is pinned when it is, and how it
    /// renders.
    places: Vec<([f64; 3], Option<[f64; 3]>, Mode)>,
    /// The scene the look-ahead weighed it as, which is the scene it is
    /// folded on — see [`Folder::fold`].
    weighed: Vec<Object>,
}

impl<T> Block<T> {
    /// Member `index` of this block, as a scene takes it.
    fn source(&self, index: usize) -> SceneSource {
        let (position, pinned, mode) = self.places[index];
        SceneSource {
            position,
            pinned,
            mode: if pinned.is_some() {
                hz_cluster::class::BED
            } else {
                mode
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

/// What reads a fold's blocks: each member's samples with its gain on
/// them, where each is, and the scene the look-ahead weighs the block as —
/// its own scene, because a weighted scene's filter has state and this one
/// reads ahead of the fold's. A [`Folder`]'s own, or taken apart from it
/// ([`Folder::read_apart`]) to read on a thread of its own.
pub struct Reader {
    members: Vec<(usize, Member)>,
    ahead: Scene,
}

impl Reader {
    /// Read `block`, `frames` samples from the master's sample `at`, as
    /// [`Folder::read`] reads it, for [`Folder::take`].
    pub fn read<T>(
        &mut self,
        block: &mut Block<T>,
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
        } = block;
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
                    (*position, pinned.then_some(*position), Mode::default())
                }
                Member::Moving(path, hint) => {
                    let end = clock(at + frames as i64 - 1);
                    let position = gained(signal, path, hint, at, frames, end, |n| {
                        sample(carry, n, channel)
                    });
                    (position, None, Mode::default())
                }
                Member::Classed(classed) => {
                    let end = clock(at + frames as i64 - 1);
                    let Classed {
                        path,
                        hint,
                        keyframes,
                        next,
                        current,
                    } = &mut **classed;
                    let position = gained(signal, path, hint, at, frames, end, |n| {
                        sample(carry, n, channel)
                    });
                    crate::path::advance(keyframes, next, current, end);
                    (position, None, current.mode)
                }
            });
        }
        block.weigh(&mut self.ahead);
        block.weighed.clear();
        block.weighed.extend_from_slice(self.ahead.objects());
    }
}

/// A master's members folded into fewer elements block by block: each block
/// placed by the energies of the window around it ([`Placer`]), which needs
/// the blocks ahead read before it is folded — so blocks go in
/// ([`Folder::read`]) a window ahead of the ones that come out
/// ([`Folder::fold`]).
pub struct Folder<T> {
    /// What reads the blocks, unless it was taken apart
    /// ([`Folder::read_apart`]).
    reader: Option<Reader>,
    /// What the mixer was made from, for another made alike.
    members: usize,
    sample_rate: f64,
    mixing: Mixing,
    elements: usize,
    pub placer: Placer,
    /// The fold's mix and cost: after [`Folder::fold`], its `mixed` holds the
    /// block's elements, and its `cost` and `limiter` what every block folded
    /// so far came to. Its `scene` is never weighed: a block is placed and
    /// judged on the scene the reading weighed it as (see [`Folder::fold`]),
    /// so `mixer.scene` holds nothing.
    ///
    /// Once [`Folder::mix_apart`] has taken it, this is a mixer made alike
    /// that nothing mixes with — no cost, a limiter that never ran — and the
    /// fold's own is [`Mix::mixer`]. [`Folder::fold`] is not to be called
    /// then: it would mix with this one, from nothing.
    pub mixer: Mixer,
    read: VecDeque<Block<T>>,
    spare: Vec<Block<T>>,
    energies: Vec<f64>,
    /// Whether the mix was taken apart ([`Folder::mix_apart`]).
    apart: bool,
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
        let mixer = mixer_for(members.len(), elements, sample_rate, mixing)?;
        Ok(Self {
            members: members.len(),
            reader: Some(Reader {
                members,
                ahead: Scene::weighed(sample_rate, mixing.weighing)?,
            }),
            sample_rate,
            mixing: *mixing,
            elements,
            placer: Placer::new(
                Clusterer::new(elements, Weighting::Fitted)?
                    .flooring(hz_cluster::floor::Floor::for_dialnorm(mixing.dialnorm))
                    .bounding(mixing.headroom.bounds()),
                hz_cluster::smooth::SMOOTHING,
            ),
            mixer,
            read: VecDeque::new(),
            spare: Vec::new(),
            energies: Vec::new(),
            apart: false,
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
        self.reader
            .as_mut()
            .expect("the fold's reader, not taken apart")
            .read(&mut block, at, frames, sample);
        self.take(block);
    }

    /// A block read apart from the fold ([`Reader::read`]) into it: what it
    /// weighs recorded for the window, and the block held until it is folded
    /// — the rest of [`Folder::read`].
    pub fn take(&mut self, block: Block<T>) {
        self.energies.clear();
        self.energies.extend(block.weighed.iter().map(|o| o.energy));
        self.placer.record(&self.energies);
        self.read.push_back(block);
    }

    /// The reading, taken apart from the fold: [`Reader::read`] reads each
    /// block as [`Folder::read`] does, and [`Folder::take`] takes it in.
    pub fn read_apart(&mut self) -> Reader {
        self.reader
            .take()
            .expect("the fold's reader, taken apart once")
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
    ///
    /// The block is placed and judged on the scene the look-ahead weighed it
    /// as when it was read. That is the scene the mixer's own would weigh it
    /// as now: both are made alike, and each weighs every block, in the order
    /// read — which is the order folded — from the same samples, so each
    /// weighs block `n` from the same state, the one `n` blocks of the same
    /// samples leave, whatever a rule carries from one block to the next.
    ///
    /// Not once the mix is taken apart ([`Folder::mix_apart`]): then each
    /// block is placed with [`Folder::place`] and mixed with [`Mix::mix`].
    pub fn fold(&mut self, ceiling: f64) -> Result<Option<Block<T>>> {
        debug_assert!(
            !self.apart,
            "a fold whose mix was taken apart is placed, not folded"
        );
        let Some(mut block) = self.read.pop_front() else {
            return Ok(None);
        };
        let clustering = self.placer.place(&block.weighed)?;
        mix(
            &mut self.mixer,
            self.placer.previous(),
            &mut block,
            &clustering,
            ceiling,
        );
        self.placer.keep(clustering);
        Ok(Some(block))
    }

    /// The first half of [`Folder::fold`], for a caller that mixes apart
    /// from the placing ([`Folder::mix_apart`]): the oldest block read placed
    /// — the placement kept, for the next block to start from — and handed
    /// back with it, for [`Mix::mix`]. Nothing when no block is waiting.
    pub fn place(&mut self) -> Result<Option<(Block<T>, Clustering)>> {
        let Some(block) = self.read.pop_front() else {
            return Ok(None);
        };
        let clustering = self.placer.place(&block.weighed)?;
        self.placer.keep(clustering.clone());
        Ok(Some((block, clustering)))
    }

    /// The mix, taken apart from the placing: the fold's own mixer, and the
    /// last block placed, for [`Mix::mix`] to mix each block
    /// [`Folder::place`] places as [`Folder::fold`] would have; one made
    /// alike is left in its place, which nothing mixes with while the two
    /// are apart — see [`Folder::mixer`]. Once, and never folded with
    /// [`Folder::fold`] after.
    pub fn mix_apart(&mut self) -> Result<Mix> {
        debug_assert!(!self.apart, "a fold's mix is taken apart once");
        let alike = mixer_for(self.members, self.elements, self.sample_rate, &self.mixing)?;
        self.apart = true;
        Ok(Mix {
            mixer: std::mem::replace(&mut self.mixer, alike),
            previous: self.placer.previous().cloned(),
        })
    }

    /// A folded block back, for its buffers to be read into again.
    pub fn recycle(&mut self, block: Block<T>) {
        self.spare.push(block);
    }

    /// Where each element is at the end of the block folded last.
    pub fn positions(&self) -> &[[f64; 3]] {
        self.placer.previous().map_or(&[], |at| &at.positions[..])
    }

    /// How each element renders, as of the block folded last: the class of
    /// every member it carries.
    pub fn modes(&self) -> &[Mode] {
        self.placer.previous().map_or(&[], |at| &at.modes[..])
    }
}

/// The mix of a fold apart from its placing: each block [`Folder::place`]
/// placed, mixed and judged, in the order placed, as [`Folder::fold`] mixes
/// and judges it — see [`Folder::mix_apart`].
pub struct Mix {
    pub mixer: Mixer,
    /// The last block's placement, which the next one's weights ramp from.
    previous: Option<Clustering>,
}

impl Mix {
    /// Mix `block`, placed by `clustering`: its elements are in
    /// `mixer.mixed`, its cost in `mixer.cost`. The next block ramps from
    /// this one.
    pub fn mix<T>(&mut self, block: &mut Block<T>, clustering: Clustering, ceiling: f64) {
        mix(
            &mut self.mixer,
            self.previous.as_ref(),
            block,
            &clustering,
            ceiling,
        );
        self.previous = Some(clustering);
    }
}

/// The mixer a fold of `members` into `elements` mixes with.
fn mixer_for(members: usize, elements: usize, sample_rate: f64, mixing: &Mixing) -> Result<Mixer> {
    let mut mixer = Mixer::new(
        sample_rate,
        mixing.weighing,
        mixing.dialnorm,
        mixing.headroom.limits(),
        elements,
    )?;
    mixer.gains = vec![1.0; members];
    Ok(mixer)
}

/// The second half of a fold: `block`, placed by `clustering`, mixed with the
/// weights ramping from `previous`'s, limited under `ceiling`, and judged on
/// the scene it was placed on.
fn mix<T>(
    mixer: &mut Mixer,
    previous: Option<&Clustering>,
    block: &mut Block<T>,
    clustering: &Clustering,
    ceiling: f64,
) {
    std::mem::swap(&mut mixer.signals, &mut block.signals);
    mixer.fold(clustering, previous, block.frames, ceiling);
    mixer.judge_on(&block.weighed, clustering);
    std::mem::swap(&mut mixer.signals, &mut block.signals);
}

/// An object's `frames` samples from the master's sample `at`, `sample(n)`
/// each, with its gain on them, into `signal`; where it is at `end`.
fn gained(
    signal: &mut Vec<f32>,
    path: &Path,
    hint: &mut usize,
    at: i64,
    frames: usize,
    end: u64,
    sample: impl Fn(usize) -> f64,
) -> [f64; 3] {
    // Before the programme an object is where it starts; its samples there
    // are the writer's to make silent.
    let mut n = 0;
    while n < frames {
        let t = (at + n as i64).max(0) as u64;
        let segment = *path.segment(t, hint);
        if segment.end == u64::MAX || segment.from == segment.to {
            // Standing, its gain is the segment's `from` at every sample of
            // it — what `Segment::at` hands back for each — so it is taken
            // once for every sample up to the segment's end.
            let gain = segment.from.gain;
            let until = if segment.end == u64::MAX {
                frames
            } else {
                (i128::from(segment.end) - i128::from(at)).min(frames as i128) as usize
            };
            signal.extend((n..until).map(|n| (sample(n) * gain) as f32));
            n = until;
        } else {
            signal.push((sample(n) * segment.at(t).gain) as f32);
            n += 1;
        }
    }
    path.segment(end, hint).at(end).position
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An object's samples with its gain on them as they were made before a
    /// standstill's gain was taken once for all its samples: the path asked
    /// at every sample. The oracle [`gained`] is held to, to the bit.
    fn gained_as_it_was(
        signal: &mut Vec<f32>,
        path: &Path,
        hint: &mut usize,
        at: i64,
        frames: usize,
        end: u64,
        sample: impl Fn(usize) -> f64,
    ) -> [f64; 3] {
        signal.extend((0..frames).map(|n| {
            let gain = path.gain((at + n as i64).max(0) as u64, hint);
            (sample(n) * gain) as f32
        }));
        path.segment(end, hint).at(end).position
    }

    fn keyframe(sample_pos: u64, x: f64, gain: f64, ramp_samples: u32) -> Keyframe {
        Keyframe {
            sample_pos,
            position: [x, 1.0, 0.0],
            gain,
            ramp_samples,
            ..Keyframe::default()
        }
    }

    /// Holds and ramps, a ramp cut short, a gain that ramps while the place
    /// holds and the other way round, a negative zero, blocks from before the
    /// programme and past its last update, and a walk started over: the same
    /// samples, the same place, and the walk left where it was.
    #[test]
    fn a_standstills_gain_taken_once_is_the_one_taken_at_every_sample() {
        let paths = [
            vec![keyframe(0, 0.0, 1.0, 0)],
            vec![
                keyframe(100, -0.5, 0.5, 0),
                keyframe(700, -0.5, 0.25, 300),
                keyframe(850, 0.5, 0.25, 400),
                keyframe(2000, 0.5, -0.0, 0),
                keyframe(2600, 0.25, 0.75, 1000),
                keyframe(3000, 0.75, 0.75, 0),
            ],
            vec![keyframe(5, 1.0, 0.0, 10), keyframe(4096, -1.0, 1.0, 2048)],
        ];
        let sample = |n: usize| ((n * 7919) % 2001) as f64 / 1000.0 - 1.0;
        for keyframes in &paths {
            let path = Path::new(keyframes);
            for frames in [1, 64, 256, 1024] {
                let (mut new_hint, mut old_hint) = (0usize, 0usize);
                for start in (-1500i64..9000).step_by(frames + 37) {
                    // Now and then a walk that has to start over.
                    if start % 5 == 0 {
                        (new_hint, old_hint) = (0, 0);
                    }
                    let end = (start + frames as i64 - 1).max(0) as u64;
                    let (mut new, mut old) = (Vec::new(), Vec::new());
                    let new_at = gained(&mut new, &path, &mut new_hint, start, frames, end, sample);
                    let old_at = gained_as_it_was(
                        &mut old,
                        &path,
                        &mut old_hint,
                        start,
                        frames,
                        end,
                        sample,
                    );
                    assert_eq!(
                        new.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                        old.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                        "from {start}, {frames} frames"
                    );
                    assert_eq!(new_at.map(f64::to_bits), old_at.map(f64::to_bits));
                    assert_eq!(new_hint, old_hint);
                }
            }
        }
    }

    /// [`Folder::fold`] as it was before it folded on the look-ahead's scene:
    /// the block weighed again by the mixer's own. The oracle the fold, and
    /// the placing with the mix apart, are held to.
    fn fold_as_it_was<T: Default>(folder: &mut Folder<T>, ceiling: f64) -> Option<Block<T>> {
        let mut block = folder.read.pop_front()?;
        block.weigh(&mut folder.mixer.scene);
        let clustering = folder.placer.place(folder.mixer.scene.objects()).unwrap();
        std::mem::swap(&mut folder.mixer.signals, &mut block.signals);
        folder
            .mixer
            .fold(&clustering, folder.placer.previous(), block.frames, ceiling);
        folder.mixer.judge(&clustering);
        std::mem::swap(&mut folder.mixer.signals, &mut block.signals);
        folder.placer.keep(clustering);
        Some(block)
    }

    fn members() -> Vec<(usize, Member)> {
        (0..9)
            .map(|channel| {
                let x = channel as f64 / 4.0 - 1.0;
                let member = match channel % 3 {
                    0 => Member::Bed {
                        position: [x, 1.0, 0.0],
                        pinned: channel == 0,
                    },
                    1 => Member::Moving(
                        Path::new(&[
                            keyframe(0, x, 1.0, 0),
                            keyframe(900, -x, 0.5, 2000),
                            keyframe(4000, x * 0.5, 0.8, 300),
                        ]),
                        0,
                    ),
                    _ => {
                        Member::classed(vec![keyframe(0, -x, 0.9, 0), keyframe(1500, x, 0.9, 700)])
                    }
                };
                (channel, member)
            })
            .collect()
    }

    fn signal_bits(signals: &[Vec<f32>]) -> Vec<Vec<u32>> {
        signals
            .iter()
            .map(|s| s.iter().map(|x| x.to_bits()).collect())
            .collect()
    }

    /// Under every rule a block's power is weighed by, the filter's among
    /// them: the fold on the look-ahead's scene, and the placing with the mix
    /// apart, mix what weighing each block again did, place it where it did
    /// and cost what it did, block for block and to the bit.
    #[test]
    fn a_fold_on_the_look_aheads_scene_folds_as_weighing_it_again_did() {
        use hz_cluster::scene::{Loudness, PERCEPTION};
        const FRAMES: usize = 512;
        const CHANNELS: usize = 9;
        for weighing in [
            Loudness::Flat,
            Loudness::KWeighted,
            Loudness::Perceptual(PERCEPTION),
        ] {
            let mixing = Mixing {
                weighing,
                ..Mixing::default()
            };
            let mut old: Folder<usize> = Folder::new(members(), 4, 48_000.0, &mixing).unwrap();
            let mut new: Folder<usize> = Folder::new(members(), 4, 48_000.0, &mixing).unwrap();
            let mut apart: Folder<usize> = Folder::new(members(), 4, 48_000.0, &mixing).unwrap();
            let mut mix = apart.mix_apart().unwrap();
            let mut seed = 9u32;
            let mut noise = move || {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5
            };
            let blocks = 14;
            let mut folded = 0;
            for k in 0..=blocks {
                if k < blocks {
                    // Louder and quieter blocks, and a short last one.
                    let frames = if k + 1 == blocks { 300 } else { FRAMES };
                    let level = [1.0, 0.05, 2.0, 0.5][k % 4];
                    let samples: Vec<f64> =
                        (0..frames * CHANNELS).map(|_| noise() * level).collect();
                    for folder in [&mut old, &mut new, &mut apart] {
                        let mut block = folder.block();
                        block.carry = k;
                        folder.read(block, k as i64 * FRAMES as i64 - 100, frames, |_, n, c| {
                            samples[n * CHANNELS + c]
                        });
                    }
                }
                while old.ready() || (k == blocks && old.waiting() > 0) {
                    let a = fold_as_it_was(&mut old, 0.9).unwrap();
                    let b = new.fold(0.9).unwrap().unwrap();
                    let (mut c, placed) = apart.place().unwrap().unwrap();
                    mix.mix(&mut c, placed, 0.9);
                    assert_eq!((a.carry, b.carry, c.carry), (folded, folded, folded));
                    let mixed = signal_bits(&old.mixer.mixed);
                    assert_eq!(
                        signal_bits(&new.mixer.mixed),
                        mixed,
                        "{weighing:?} {folded}"
                    );
                    assert_eq!(
                        signal_bits(&mix.mixer.mixed),
                        mixed,
                        "{weighing:?} {folded}"
                    );
                    let at = |f: &Folder<usize>| -> Vec<[u64; 3]> {
                        f.positions().iter().map(|p| p.map(f64::to_bits)).collect()
                    };
                    assert_eq!(at(&new), at(&old));
                    assert_eq!(at(&apart), at(&old));
                    for cost in [new.mixer.cost, mix.mixer.cost] {
                        assert_eq!(cost.sum.to_bits(), old.mixer.cost.sum.to_bits());
                        assert_eq!(cost.worst.to_bits(), old.mixer.cost.worst.to_bits());
                        assert_eq!(cost.blocks, old.mixer.cost.blocks);
                        assert_eq!(cost.inaudible, old.mixer.cost.inaudible);
                    }
                    assert_eq!(
                        new.mixer
                            .limiter
                            .as_ref()
                            .map(|l| (l.limited, l.peak.to_bits())),
                        old.mixer
                            .limiter
                            .as_ref()
                            .map(|l| (l.limited, l.peak.to_bits()))
                    );
                    assert_eq!(
                        mix.mixer
                            .limiter
                            .as_ref()
                            .map(|l| (l.limited, l.peak.to_bits())),
                        old.mixer
                            .limiter
                            .as_ref()
                            .map(|l| (l.limited, l.peak.to_bits()))
                    );
                    old.recycle(a);
                    new.recycle(b);
                    apart.recycle(c);
                    folded += 1;
                }
            }
            assert_eq!(folded, blocks);
            assert!(old.mixer.cost.blocks > 0);
        }
    }

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
