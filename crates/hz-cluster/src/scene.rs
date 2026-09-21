//! One block's scene, built once for everything that folds one.
//!
//! [`Object::energy`] is what the whole clustering is steered by: it decides
//! which objects earn a seed, how hard each one pulls its element, which of
//! them the metric's floor lets set a worst case, and which of them a report
//! counts at all. Two callers were computing it, and they were not computing
//! the same thing — the harness applied the master's `importance` and the
//! encoder did not, so `cargo xtask cluster` was reporting on a scene the
//! stream it claimed to describe had never folded.
//!
//! So it is built here, once, and both of them call it. A number that steers
//! everything is not a number to have two of.
//!
//! # What the energy is
//!
//! Under the plain rule, the block's power, times the square of the gain the
//! master asks for, times the importance it states. A power and not an
//! amplitude because that is what a weighted mean of directions wants; the
//! gain squared because a gain is an amplitude; the importance as it stands
//! because it is already a weight.
//!
//! # A physical power is not what a listener weighs
//!
//! The power above is a meter's answer, and the ear is not a meter. A rumble
//! at −10 dBFS outweighs a line of dialogue at −20 dBFS ten to one in it, and
//! it is the dialogue a listener localises — so the rumble takes the seed,
//! pulls the centre and sets the floor under the worst case, and the voice is
//! folded into whatever happens to share its azimuth.
//!
//! [`Loudness::KWeighted`] was the first answer to that: the block's power
//! taken through the K-weighting filter pair of ITU-R BS.1770, which is the
//! same one [`hz_analysis`] already measures loudness with. Measured on the
//! design at 48 kHz: −13.3 dB at 20 Hz, −8.3 dB at 30 Hz, −5.6 dB at 40 Hz,
//! −1.1 dB at 100 Hz, +0.7 dB at 1 kHz, +3.1 dB at 2 kHz and +4.0 dB from
//! 4 kHz up. It re-weighs a rumble against a voice and nothing else, and the
//! metric said it cost — see [`LOUDNESS`].
//!
//! [`Loudness::Perceptual`] is the second, and it answers a different
//! question. An object's weight is not how loud it is but **what its
//! disappearance would change** in what is heard. So the block's audio is
//! split into bands the ear would split it into ([`hz_analysis::bands`]), the
//! loudness of each band follows the compressive law of ISO 532-2
//! ([`hz_analysis::partial`]), and an object's importance is the loudness of
//! the scene with it in, less the loudness of the scene without it, band by
//! band. A voice in bands nothing else occupies keeps its whole loudness; the
//! same voice forty decibels under a rumble in the same bands adds almost
//! nothing, and is weighed accordingly.
//!
//! **Masking is spatial.** A masker at the same place hides more than one
//! across the room — the spatial release from masking — so the "scene
//! without it" is not the whole scene. With [`Masker::Local`], what masks
//! object `i` is every other object weighted by how much of its rendered gain
//! vector it shares with `i`'s over the delivery presentations, which is the
//! metric's own language: two objects the same way round overlap completely
//! and two on opposite walls hardly at all. [`Masker::Global`] weighs every
//! other object by one, and is kept so the two can be measured against each
//! other.
//!
//! Because an importance depends on the whole block's scene, a perceptual
//! scene is finished in two steps: every object is pushed, then [`Scene::finish`]
//! weighs them all together. The other rules weigh each object as it is
//! pushed, and `finish` has nothing left to do.
//!
//! # The filter has state
//!
//! The K-weighted rule filters the audio, so a scene is built by one
//! [`Scene`] held across the blocks of a stream, and every object is pushed
//! every block in the same order. A filter restarted each block would ring at
//! every boundary and measure its own transient. The perceptual rule's band
//! analysis is a transform of the block alone and carries nothing between
//! blocks, so it does not depend on the order; the scene is held across blocks
//! anyway, because that is where its buffers live.

use crate::Object;
use crate::fit::Reference;
use hz_analysis::bands::Bands;
use hz_analysis::kweighting::{KWeighting, State};
use hz_analysis::partial;
use hz_core::Result;
use hz_render::Mode;

/// How a block's power is weighed before it becomes an energy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Loudness {
    /// The block's plain mean square: what a meter reads.
    #[default]
    Flat,
    /// Through the K-weighting filter pair of ITU-R BS.1770 — what a listener
    /// weighs, as one number. See the note at the top of this module.
    KWeighted,
    /// What the object adds to the loudness of the scene, band by band, with
    /// what is near it masking it. See the note at the top of this module.
    Perceptual(Perception),
}

/// The parameters of the perceptual rule.
///
/// Both are chosen by measurement and recorded in `docs/clustering.md`; see
/// [`PERCEPTION`] for the values in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perception {
    /// How many equal steps of the ERB-rate scale the audible range is split
    /// into. One is the K-weighted power under the compressive law and no
    /// masking between bands at all.
    pub bands: usize,
    /// What masks an object.
    pub masker: Masker,
}

/// What masks an object: the whole scene, or the part of it near the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Masker {
    /// Every other object, wherever it is.
    Global,
    /// Every other object, weighted by how much of its rendered gain vector it
    /// shares with this one's over the delivery presentations.
    Local,
}

/// The perceptual rule's parameters, as measured.
pub const PERCEPTION: Perception = Perception {
    bands: 6,
    masker: Masker::Local,
};

/// How every fold in this project weighs a block's power unless told
/// otherwise.
///
/// One constant, and the `--loudness` flag of either command defaults to it.
/// The harness's report is a report about the encoder's fold only while the
/// two are steered the same way, so the default lives in one place, and a
/// flag states a departure from it in a log rather than hiding one in a
/// build: the encoder takes it so that a rule the metric cannot rank can be
/// listened to on a programme.
///
/// # Why it is not the K-weighted one
///
/// Because the metric says so. On `crowd40` folded to twelve elements,
/// measured on a *fixed* yardstick so that the change in the rule is not
/// hidden by a change in the ruler:
///
/// | steered by | mean error | worst | flips a block |
/// |---|---|---|---|
/// | the plain power | **0.044** | **0.377** | 4.74 |
/// | the K-weighted power | 0.087 | 0.839 | **3.15** |
///
/// It is twice the error for a third off the flips, and the argument for it —
/// that the objects it gives up on are ones a listener was not localising
/// anyway — is exactly the kind of argument this project does not accept
/// without a measurement. `crowd40` is also the worst possible scene to settle
/// it on: forty objects, each a pure tone, spread over five octaves, so a
/// perceptual re-weighting moves their ranking by seventeen decibels where
/// broadband content would move it by two or three. **Neither yardstick can
/// decide this, and the metric cannot judge it by ear**, so the rule that
/// does not cost anything stays, and the other one stays compiled.
///
/// The perceptual rule is measured in `docs/clustering.md`, on both rulers
/// and on a broadband scene as well as on `crowd40`; what it says is there.
pub const LOUDNESS: Loudness = Loudness::Flat;

/// What a full-scale power is as an excitation of the loudness model.
///
/// The model counts excitation from the threshold of hearing up, and a stream
/// counts power from full scale down, so the two meet at how loud a room plays
/// full scale. A cinema is calibrated so that pink noise at −20 dBFS in one
/// channel reads 85 dB SPL (SMPTE RP 200), which puts full scale at 105 dB; in
/// the model's units, where a kilohertz tone at nought decibels excites about
/// one, that is `10^10.5`.
///
/// Stated rather than measured, and it moves almost nothing: it decides where
/// the threshold term of the law falls, which is a hundred decibels under full
/// scale, and every comparison made above that is between objects the law
/// treats as far above threshold. Deriving it from the programme's own
/// dialogue level is a separate lever — see `docs/clustering-next.md`.
pub const FULL_SCALE_EXCITATION: f64 = 3.1622776601683795e10;

/// What the caller knows about one object before its audio is read.
///
/// Everything here comes from the master's own statements. The audio decides
/// the rest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Source {
    /// ADM Cartesian, at the end of the block — the position the payload
    /// declares and a decoder ramps to.
    pub position: [f64; 3],
    /// Linear, not decibels: the gain the master asks for.
    pub gain: f64,
    /// The object's width, which shipped content never states.
    pub size: f64,
    /// What the master says this object is worth, 0 to 1.
    ///
    /// A mix says which of its objects matter through this as well as through
    /// their level, and an encoder that ignores it is folding a scene the mix
    /// did not describe. Absent means one: an object a master says nothing
    /// about is an object that matters.
    pub importance: f64,
    /// Where this object has to stay, if it is a bed channel and may not be
    /// clustered — see [`Object::pinned`].
    pub pinned: Option<[f64; 3]>,
    /// How the master says it is to be rendered, beyond where and how loud —
    /// its class. See [`Object::mode`].
    pub mode: Mode,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            position: [0.0, 1.0, 0.0],
            gain: 1.0,
            size: 0.0,
            importance: 1.0,
            pinned: None,
            mode: Mode::default(),
        }
    }
}

/// The scene of one block, and the filter state that carries between them.
///
/// Held across a stream, not built per block: see the note about the filter's
/// state at the top of this module. The object vector is reused, so a block
/// costs no allocation once the stream is running.
pub struct Scene {
    loudness: Loudness,
    sample_rate: f64,
    filter: KWeighting,
    /// One filter state per object, in the order they are pushed. Empty unless
    /// the weighted rule is in force.
    states: Vec<(State, State)>,
    objects: Vec<Object>,
    pushed: usize,
    /// The band analysis, built for the block length the stream turns out to
    /// have and rebuilt only if that changes — once at the start, and again
    /// for a last block that is shorter. Empty unless the perceptual rule is
    /// in force, as is everything below it.
    bands: Option<Bands>,
    /// Every object's band powers this block, `objects × bands`, with the
    /// master's gain already in.
    powers: Vec<f64>,
    /// What the master says each object is worth, applied once the loudness
    /// is known.
    stated: Vec<f64>,
    /// One object's block, gathered from the iterator for the transform.
    block: Vec<f64>,
    /// The delivery presentations, for the local masker.
    reference: Option<Reference>,
    /// What each object radiates over them, and the length of each.
    radiated: Vec<Vec<f64>>,
    lengths: Vec<f64>,
    /// One object's masker, per band.
    masker: Vec<f64>,
    finished: bool,
}

impl Scene {
    /// A scene builder for a stream at this rate, weighing its blocks the way
    /// [`LOUDNESS`] says every fold in this project does.
    pub fn new(sample_rate: f64) -> Result<Self> {
        Self::weighed(sample_rate, LOUDNESS)
    }

    /// A scene builder weighing its blocks a stated way, for the measurement
    /// that chooses between them.
    ///
    /// Fails only if the delivery presentations cannot be built, which the
    /// local masker needs and nothing else does.
    pub fn weighed(sample_rate: f64, loudness: Loudness) -> Result<Self> {
        let reference = match loudness {
            Loudness::Perceptual(Perception {
                masker: Masker::Local,
                ..
            }) => Some(Reference::new()?),
            _ => None,
        };
        Ok(Self {
            loudness,
            sample_rate,
            filter: KWeighting::new(sample_rate),
            states: Vec::new(),
            objects: Vec::new(),
            pushed: 0,
            bands: None,
            powers: Vec::new(),
            stated: Vec::new(),
            block: Vec::new(),
            reference,
            radiated: Vec::new(),
            lengths: Vec::new(),
            masker: Vec::new(),
            finished: false,
        })
    }

    /// How this scene weighs a block.
    pub fn loudness(&self) -> Loudness {
        self.loudness
    }

    /// Begin a block. The objects from the last one are dropped; the filter
    /// state is not.
    pub fn start(&mut self) {
        self.objects.clear();
        self.powers.clear();
        self.stated.clear();
        self.pushed = 0;
        self.finished = false;
    }

    /// Add one object and its block of samples.
    ///
    /// Objects are pushed in the same order every block, because that order is
    /// what the filter state is kept by — the same reason the clustering keeps
    /// element numbers rather than deriving them.
    ///
    /// The samples are read once, in one pass, and nothing is kept: what comes
    /// out is the block's weighted mean square, folded into the energy with
    /// the gain and the importance the master states. Under the perceptual
    /// rule the energy is not known until every object is in, and this
    /// leaves it at nought for [`Scene::finish`] to fill.
    pub fn push(&mut self, source: &Source, samples: impl Iterator<Item = f64>) {
        let index = self.pushed;
        self.pushed += 1;

        let mut power = 0.0f64;
        let mut counted = 0usize;
        // The loudest the signal gets, whatever the rule: what bounds the
        // peak of an element carrying it — see [`crate::headroom`].
        let mut peak = 0.0f64;
        match self.loudness {
            Loudness::Flat => {
                for sample in samples {
                    power += sample * sample;
                    peak = peak.max(sample.abs());
                    counted += 1;
                }
            }
            Loudness::KWeighted => {
                if self.states.len() <= index {
                    self.states
                        .resize(index + 1, (State::default(), State::default()));
                }
                let state = &mut self.states[index];
                for sample in samples {
                    let weighted = self.filter.step(state, sample);
                    power += weighted * weighted;
                    peak = peak.max(sample.abs());
                    counted += 1;
                }
            }
            Loudness::Perceptual(perception) => {
                self.block.clear();
                self.block.extend(samples);
                peak = self.block.iter().fold(0.0f64, |m, s| m.max(s.abs()));
                let bands = perception.bands.max(1);
                let frames = self.block.len();
                if self
                    .bands
                    .as_ref()
                    .is_none_or(|analysis| analysis.frames() != frames || analysis.bands() != bands)
                {
                    self.bands = Some(Bands::new(self.sample_rate, frames, bands));
                }
                let at = index * bands;
                self.powers.resize(at + bands, 0.0);
                if let Some(analysis) = self.bands.as_mut() {
                    analysis.analyse(&self.block, &mut self.powers[at..at + bands]);
                }
                let gain = source.gain * source.gain;
                for power in &mut self.powers[at..at + bands] {
                    *power *= gain;
                }
                if let Some(reference) = &self.reference {
                    if self.radiated.len() <= index {
                        self.radiated.resize_with(index + 1, Vec::new);
                        self.lengths.resize(index + 1, 0.0);
                    }
                    let radiated = &mut self.radiated[index];
                    reference.radiated(source.position, source.size, radiated);
                    self.lengths[index] = radiated.iter().map(|g| g * g).sum::<f64>().sqrt();
                }
                self.stated.push(source.importance.clamp(0.0, 1.0));
            }
        }
        let power = if counted == 0 {
            0.0
        } else {
            power / counted as f64
        };

        self.objects.push(Object {
            position: source.position,
            energy: power * source.gain * source.gain * source.importance.clamp(0.0, 1.0),
            size: source.size,
            pinned: source.pinned,
            mode: source.mode,
            peak: peak * source.gain.abs(),
        });
    }

    /// Weigh the block's objects against each other, and hand the scene over.
    ///
    /// The step that only the perceptual rule needs, since an object's
    /// importance there is what it adds to the rest of the scene. Under the
    /// other rules every object was weighed as it was pushed and this returns
    /// them as they are. Idempotent: a scene finished twice is the same scene.
    pub fn finish(&mut self) -> &[Object] {
        if !self.finished {
            self.finished = true;
            if let Loudness::Perceptual(perception) = self.loudness {
                self.weigh(perception);
            }
        }
        &self.objects
    }

    /// The block's scene, as the clusterer takes it.
    ///
    /// What [`Scene::finish`] returned, for a caller that needs it again.
    /// Before `finish`, a perceptual scene's energies are all nought.
    pub fn objects(&self) -> &[Object] {
        &self.objects
    }

    /// The perceptual rule: each object's energy is the loudness it adds to
    /// what masks it, band by band, times what the master says it is worth.
    fn weigh(&mut self, perception: Perception) {
        let bands = perception.bands.max(1);
        let count = self.objects.len();
        self.masker.resize(bands, 0.0);
        for object in 0..count {
            // What masks this object, per band: every other object, weighted
            // by how much of the room they share.
            self.masker.fill(0.0);
            for other in 0..count {
                if other == object {
                    continue;
                }
                let overlap = match perception.masker {
                    Masker::Global => 1.0,
                    Masker::Local => {
                        let lengths = self.lengths[object] * self.lengths[other];
                        if lengths > 1e-12 {
                            let dot: f64 = self.radiated[object]
                                .iter()
                                .zip(&self.radiated[other])
                                .map(|(a, b)| a * b)
                                .sum();
                            (dot / lengths).clamp(0.0, 1.0)
                        } else {
                            0.0
                        }
                    }
                };
                if overlap <= 0.0 {
                    continue;
                }
                let theirs = &self.powers[other * bands..(other + 1) * bands];
                for (masker, power) in self.masker.iter_mut().zip(theirs) {
                    *masker += overlap * power;
                }
            }
            let own = &self.powers[object * bands..(object + 1) * bands];
            let added: f64 = own
                .iter()
                .zip(&self.masker)
                .map(|(own, masker)| {
                    partial::added(own * FULL_SCALE_EXCITATION, masker * FULL_SCALE_EXCITATION)
                })
                .sum();
            self.objects[object].energy = added * self.stated[object];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block of a sine, as a channel of audio would arrive.
    fn tone(hz: f64, rate: f64, frames: usize, at: usize) -> Vec<f64> {
        (0..frames)
            .map(|n| {
                let t = (at + n) as f64 / rate;
                (std::f64::consts::TAU * hz * t).sin()
            })
            .collect()
    }

    /// The correction this exists for: a rumble louder than a voice in a meter
    /// is not louder than it to a listener, and the clustering is steered by
    /// which of them the number says is loud.
    ///
    /// A 30 Hz tone ten decibels above a 2 kHz one. Unweighted the rumble
    /// carries ten times the energy of the voice; weighted it carries less
    /// than a third of it, because the filter is 8.3 dB down at thirty hertz
    /// and 3.1 dB up at two kilohertz.
    #[test]
    fn a_rumble_does_not_outweigh_a_voice() {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 1280;
        let mut scene = Scene::weighed(RATE, Loudness::KWeighted).unwrap();

        let loud = 10f64.powf(10.0 / 20.0);
        // Long enough for the filter to have settled: the first blocks are its
        // own transient, and what is asserted is the steady state.
        let mut energies = (0.0, 0.0);
        for block in 0..20 {
            scene.start();
            scene.push(
                &Source::default(),
                tone(30.0, RATE, FRAMES, block * FRAMES)
                    .into_iter()
                    .map(|s| s * loud),
            );
            scene.push(
                &Source::default(),
                tone(2000.0, RATE, FRAMES, block * FRAMES).into_iter(),
            );
            energies = (scene.objects()[0].energy, scene.objects()[1].energy);
        }

        // Unweighted it would be ten to one the other way.
        assert!(
            energies.0 < energies.1,
            "the rumble came out at {:.5} and the voice at {:.5}",
            energies.0,
            energies.1
        );
    }

    /// The gain and the importance are both in it, and both squared or not as
    /// they should be: a gain is an amplitude and an importance is a weight.
    #[test]
    fn the_gain_is_squared_and_the_importance_is_not() {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 1280;

        let energy_of = |gain: f64, importance: f64| {
            let mut scene = Scene::new(RATE).unwrap();
            let mut last = 0.0;
            for block in 0..20 {
                scene.start();
                scene.push(
                    &Source {
                        gain,
                        importance,
                        ..Source::default()
                    },
                    tone(1000.0, RATE, FRAMES, block * FRAMES).into_iter(),
                );
                last = scene.finish()[0].energy;
            }
            last
        };

        let plain = energy_of(1.0, 1.0);
        assert!(
            (energy_of(2.0, 1.0) / plain - 4.0).abs() < 1e-6,
            "twice the gain is four times the energy"
        );
        assert!(
            (energy_of(1.0, 0.5) / plain - 0.5).abs() < 1e-6,
            "half the importance is half the energy"
        );
    }

    /// The filter state carries between blocks, and every object keeps its
    /// own. Two objects fed the same signal come out the same; one fed a block
    /// at a time comes out as one fed the lot.
    #[test]
    fn the_filter_state_belongs_to_the_object_and_survives_the_block() {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 256;
        let mut scene = Scene::weighed(RATE, Loudness::KWeighted).unwrap();

        for block in 0..8 {
            scene.start();
            // A second object with a different signal in between, so that a
            // state kept by anything but the push order would be crossed.
            scene.push(
                &Source::default(),
                tone(1000.0, RATE, FRAMES, block * FRAMES).into_iter(),
            );
            scene.push(
                &Source::default(),
                tone(60.0, RATE, FRAMES, block * FRAMES).into_iter(),
            );
            scene.push(
                &Source::default(),
                tone(1000.0, RATE, FRAMES, block * FRAMES).into_iter(),
            );
        }
        let objects = scene.objects();
        assert!(
            (objects[0].energy - objects[2].energy).abs() < 1e-12,
            "two objects with the same signal came out at {} and {}",
            objects[0].energy,
            objects[2].energy
        );
        assert!(
            objects[1].energy < objects[0].energy,
            "sixty hertz should weigh less than a kilohertz"
        );
    }

    /// The rule that ships is the plain one, and the weighted one is a
    /// different answer rather than the same answer spelled differently.
    ///
    /// Asserted because the constant is what keeps the harness and the encoder
    /// describing the same fold: a change to it is a change to both, and it
    /// should not be possible to make it by accident.
    #[test]
    fn the_rule_that_ships_is_the_plain_one() {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 1280;
        assert_eq!(LOUDNESS, Loudness::Flat);

        let energy_of = |loudness| {
            let mut scene = Scene::weighed(RATE, loudness).unwrap();
            let mut last = 0.0;
            for block in 0..20 {
                scene.start();
                scene.push(
                    &Source::default(),
                    tone(60.0, RATE, FRAMES, block * FRAMES).into_iter(),
                );
                last = scene.finish()[0].energy;
            }
            last
        };
        // The plain rule is the block's own mean square and nothing else.
        let block = tone(60.0, RATE, FRAMES, 19 * FRAMES);
        let plain = block.iter().map(|s| s * s).sum::<f64>() / FRAMES as f64;
        assert!((energy_of(Loudness::Flat) - plain).abs() < 1e-12);
        // And sixty hertz weighs 2.9 dB less to a listener than to a meter.
        assert!(energy_of(Loudness::KWeighted) < 0.6 * plain);
    }

    /// A block with no samples in it is silent rather than a division by zero.
    #[test]
    fn a_block_with_no_samples_is_silent() {
        let mut scene = Scene::new(48_000.0).unwrap();
        scene.start();
        scene.push(&Source::default(), std::iter::empty());
        assert_eq!(scene.finish()[0].energy, 0.0);

        let mut scene = Scene::weighed(48_000.0, Loudness::Perceptual(PERCEPTION)).unwrap();
        scene.start();
        scene.push(&Source::default(), std::iter::empty());
        assert_eq!(scene.finish()[0].energy, 0.0);
    }

    /// A perceptual scene, with the objects placed where the test says.
    fn perceptual(masker: Masker, placed: &[([f64; 3], Vec<f64>)], importance: &[f64]) -> Vec<f64> {
        let mut scene = Scene::weighed(
            48_000.0,
            Loudness::Perceptual(Perception { bands: 12, masker }),
        )
        .unwrap();
        scene.start();
        for (index, (position, samples)) in placed.iter().enumerate() {
            scene.push(
                &Source {
                    position: *position,
                    importance: importance.get(index).copied().unwrap_or(1.0),
                    ..Source::default()
                },
                samples.iter().copied(),
            );
        }
        scene.finish().iter().map(|object| object.energy).collect()
    }

    /// What the perceptual rule is for: a quiet object in bands of its own
    /// keeps its importance, and the same object under something loud in the
    /// same bands loses it — the metric's floor then lets the first set a
    /// worst case and not the second, which is the ceiling object that was
    /// folded to the floor and the one that should have been.
    #[test]
    fn masking_is_by_band() {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 1280;
        let front = [0.0, 1.0, 0.0];
        let quiet = 10f64.powf(-38.0 / 20.0);
        // Both pairs sit well inside their bands: a tone within a couple of
        // bins of a band's edge leaks its window into the next band, which
        // would measure the window and not the masking.
        let loud = tone(1000.0, RATE, FRAMES, 0);
        let same_band: Vec<f64> = tone(1100.0, RATE, FRAMES, 0)
            .into_iter()
            .map(|s| s * quiet)
            .collect();
        let own_band: Vec<f64> = tone(3500.0, RATE, FRAMES, 0)
            .into_iter()
            .map(|s| s * quiet)
            .collect();

        let masked = perceptual(
            Masker::Global,
            &[(front, loud.clone()), (front, same_band)],
            &[],
        );
        let clear = perceptual(Masker::Global, &[(front, loud), (front, own_band)], &[]);

        let floor = crate::metric::AUDIBLE_FLOOR;
        assert!(
            masked[1] < masked[0] * floor,
            "under the loud object's band it weighs {:.2e} of it, which is above the floor",
            masked[1] / masked[0]
        );
        assert!(
            clear[1] > clear[0] * floor,
            "in a band of its own it weighs {:.2e} of it, which is under the floor",
            clear[1] / clear[0]
        );
        // The loud object outweighs the quiet one either way. It is not
        // unmoved: what it adds is the scene's loudness less the quiet one's
        // own, and under the compressive law a sound thirty-eight decibels
        // down still has a sixth of the loudness — so the loud object weighs
        // a sixth less with the quiet one in its band than with it elsewhere.
        // That is the definition, and it is worth knowing.
        assert!(masked[0] > 100.0 * masked[1] && clear[0] > clear[1]);
        assert!(
            masked[0] < clear[0] && masked[0] > 0.8 * clear[0],
            "{} with the quiet one in its band, {} with it elsewhere",
            masked[0],
            clear[0]
        );
    }

    /// Masking is spatial: the same masker across the room hides less than
    /// one at the same place, and the global masker cannot tell the two apart.
    #[test]
    fn a_masker_across_the_room_hides_less() {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 1280;
        let quiet = 10f64.powf(-30.0 / 20.0);
        let loud = tone(1000.0, RATE, FRAMES, 0);
        let soft: Vec<f64> = tone(1100.0, RATE, FRAMES, 0)
            .into_iter()
            .map(|s| s * quiet)
            .collect();
        let front = [0.0, 1.0, 0.0];
        let back = [0.0, -1.0, 0.0];

        let local_near = perceptual(
            Masker::Local,
            &[(front, loud.clone()), (front, soft.clone())],
            &[],
        );
        let local_far = perceptual(
            Masker::Local,
            &[(front, loud.clone()), (back, soft.clone())],
            &[],
        );
        let global_near = perceptual(
            Masker::Global,
            &[(front, loud.clone()), (front, soft.clone())],
            &[],
        );
        let global_far = perceptual(Masker::Global, &[(front, loud), (back, soft)], &[]);

        assert!(
            local_far[1] > 3.0 * local_near[1],
            "across the room the soft object weighs {:.3e}, next to the masker {:.3e}",
            local_far[1],
            local_near[1]
        );
        assert!((global_far[1] - global_near[1]).abs() < 1e-12 * global_near[1]);
        // Alone in the room it would weigh more still.
        let alone = perceptual(Masker::Local, &[(front, soft_alone(quiet))], &[]);
        assert!(alone[0] > local_far[1]);

        fn soft_alone(quiet: f64) -> Vec<f64> {
            tone(1100.0, 48_000.0, 1280, 0)
                .into_iter()
                .map(|s| s * quiet)
                .collect()
        }
    }

    /// The master's importance is a weight on the answer, and the gain is in
    /// the power the answer is made from; a scene finished twice is the same
    /// scene; and the perceptual rule does not care what order the objects
    /// come in.
    #[test]
    fn the_perceptual_rule_is_a_function_of_the_block() {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 1280;
        let front = [0.0, 1.0, 0.0];
        let left = [-1.0, 0.0, 0.0];
        let a = tone(500.0, RATE, FRAMES, 0);
        let b: Vec<f64> = tone(3000.0, RATE, FRAMES, 0)
            .into_iter()
            .map(|s| s * 0.3)
            .collect();

        let forward = perceptual(Masker::Local, &[(front, a.clone()), (left, b.clone())], &[]);
        let backward = perceptual(Masker::Local, &[(left, b.clone()), (front, a.clone())], &[]);
        assert!((forward[0] - backward[1]).abs() < 1e-12);
        assert!((forward[1] - backward[0]).abs() < 1e-12);

        let halved = perceptual(
            Masker::Local,
            &[(front, a.clone()), (left, b.clone())],
            &[0.5],
        );
        assert!((halved[0] - 0.5 * forward[0]).abs() < 1e-12);
        assert!((halved[1] - forward[1]).abs() < 1e-12);

        let mut scene = Scene::weighed(RATE, Loudness::Perceptual(PERCEPTION)).unwrap();
        scene.start();
        scene.push(
            &Source {
                position: front,
                ..Source::default()
            },
            a.iter().copied(),
        );
        scene.push(
            &Source {
                position: left,
                ..Source::default()
            },
            b.iter().copied(),
        );
        assert_eq!(scene.objects()[0].energy, 0.0, "not weighed until finished");
        let once: Vec<f64> = scene.finish().iter().map(|o| o.energy).collect();
        let twice: Vec<f64> = scene.finish().iter().map(|o| o.energy).collect();
        assert_eq!(once, twice);
        assert!(once[0] > 0.0 && once[1] > 0.0);
    }

    /// The compression is in it: ten decibels more power is well under twice
    /// the importance, where the plain rule makes it ten times.
    #[test]
    fn the_importance_is_compressive() {
        const RATE: f64 = 48_000.0;
        const FRAMES: usize = 1280;
        let front = [0.0, 1.0, 0.0];
        let soft: Vec<f64> = tone(1000.0, RATE, FRAMES, 0)
            .into_iter()
            .map(|s| s * 0.1)
            .collect();
        let loud: Vec<f64> = soft.iter().map(|s| s * 10f64.powf(0.5)).collect();
        let one = perceptual(Masker::Global, &[(front, soft)], &[]);
        let ten = perceptual(Masker::Global, &[(front, loud)], &[]);
        let ratio = ten[0] / one[0];
        assert!(
            ratio > 1.4 && ratio < 1.8,
            "ten decibels came out as {ratio}"
        );
    }
}
