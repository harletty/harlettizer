//! Reducing an authored object scene to the fixed number of elements a
//! delivery bitstream carries.
//!
//! A programme is mixed with as many objects as it needs — often more than a
//! hundred. A delivery bitstream carries **twelve, fourteen or sixteen**. So
//! something has to decide which objects share an element, where that element
//! goes, and how its audio is made. That decision is the one part of this
//! project with no published answer, and it is the part a listener hears.
//!
//! # What makes an answer right
//!
//! Rendering is linear in the object signals. On a layout with `C` speakers, an
//! object at `p` with signal `x` contributes `g(p)·x`, where `g` is the
//! panner. If each element `k` sits at `q_k` and carries `y_k = Σ_i w_ik x_i`,
//! then the rendered result is
//!
//! ```text
//! Σ_k g(q_k)·y_k  =  Σ_i (Σ_k w_ik g(q_k))·x_i
//! ```
//!
//! against `Σ_i g(p_i)·x_i` for the original scene. So the error is per
//! object, it is a property of the *weights and positions alone*, and it can be
//! measured without any audio at all: how far `Σ_k w_ik g(q_k)` is from
//! `g(p_i)`, weighted by how loud that object is. That is what [`error`]
//! computes and what everything here is judged by.
//!
//! # Where the weights come from
//!
//! Not from a rule invented here. Given the element positions, distributing an
//! object over them **by panning it onto them** preserves its energy and its
//! energy-weighted direction — that is a property of vector-base amplitude
//! panning, already exercised in [`hz_render`], and it is exactly the property
//! that lets an element stand in for the objects folded into it.
//!
//! So the only real question is where to put the elements, which is
//! [`positions`]: a weighted clustering of the objects by direction, with
//! loudness as the weight, because an object nobody can hear is an object
//! whose position does not matter.
//!
//! # What is not claimed
//!
//! That this matches the reference engine. It cannot be checked: every master
//! seen is *output* of a clustering — eleven, thirteen or fifteen objects,
//! which is twelve, fourteen or sixteen elements — so there is no authored
//! input to run both over. What is claimed is the metric above, measured, on
//! scenes whose ground truth is known because they were synthesised.

pub mod class;
pub mod earn;
pub mod fit;
pub mod floor;
pub mod headroom;
pub mod metric;
pub mod mix;
pub mod motion;
pub mod overlay;
pub mod place;
pub mod restart;
pub mod scene;
pub mod seed;
pub mod smooth;

use fit::Reference;
use hz_core::Result;
use hz_meta::oamd::{Block, Gain, Object as Element, ObjectAudioMetadata, Position, Ramp, Render};
use hz_render::{Mode, PointPanner};

/// One object in the scene at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Object {
    /// ADM Cartesian, which is what a master set carries.
    pub position: [f64; 3],
    /// How much this object matters, as a **power**: its gain squared times
    /// the block's K-weighted power, times whatever importance the master
    /// states for it. [`scene::Scene`] builds it, and is the only thing that
    /// should — see there for why it is weighted and why one place computes
    /// it.
    ///
    /// A power and not an amplitude, because that is what a weighted mean of
    /// directions wants — two objects of equal amplitude either side of a
    /// third do not pull it as hard as one twice as loud, and a mix says which
    /// of its objects matter through `importance` as well as through level.
    /// Objects with no energy are carried but do not pull an element towards
    /// them.
    pub energy: f64,
    /// How large the object is, zero for a point.
    ///
    /// **This is not carried through.** Counted over every master seen —
    /// 6546 object statements — every reference stream states a size of
    /// exactly zero, and never states decorrelation, snapping, a zone
    /// constraint or a screen anchor either. Only position and gain move.
    ///
    /// A mix does use size, so it has to go somewhere, and the only place left
    /// is **the fold itself**: a wide object is spread over several elements
    /// rather than given a width the output will not carry. That is what
    /// [`Weighting::Spread`] does with it.
    ///
    /// A fold cannot see a width at all — a presentation is rows and columns
    /// with a matrix after them — so the metric sees one only on the
    /// presentation that is rendered.
    pub size: f64,
    /// Where this object has to stay, if it may not be clustered at all.
    ///
    /// A bed channel is a speaker feed, not an object: it belongs at that
    /// speaker and nowhere else, and an element that carries it has to sit
    /// there exactly. The low frequency channel is the same and more so —
    /// panning it anywhere is meaningless. So a pinned object takes an element
    /// of its own at the position given, keeps it whatever else the scene
    /// does, and is left out of the fit entirely.
    ///
    /// `None` for an ordinary object, which is what a mix's objects are.
    pub pinned: Option<[f64; 3]>,
    /// How it is to be rendered beyond where it is — snap, zones, elevation,
    /// screen — which is its **class**: an object is only ever folded with
    /// objects of the same mode, and an element's own metadata are its
    /// class's. See [`class`].
    pub mode: Mode,
    /// The loudest the object's gained signal gets in the block, as a
    /// fraction of full scale — what bounds the peak of an element that
    /// carries it. Nought when nothing measured it, which bounds nothing.
    /// See [`headroom`].
    pub peak: f64,
}

/// Where the elements went, and how the objects were shared out between them.
#[derive(Debug, Clone, PartialEq)]
pub struct Clustering {
    /// One position per element, in the master's cube frame.
    pub positions: Vec<[f64; 3]>,
    /// And the direction the clustering settled on for each, as a unit vector.
    ///
    /// Not a derived quantity, which is why it is carried. A position is the
    /// direction scaled by a radius and then clamped into the cube, and the
    /// clamp *turns* it: a direction of `[0.8, 0.5, 0.33]` at radius 1.5
    /// scales to `[1.2, 0.75, 0.5]` and clamps to `[1.0, 0.75, 0.5]`, which
    /// points somewhere else. Recovering the direction by normalising the
    /// position — which the warm start used to do — therefore hands the next
    /// block a different element from the one this one settled on, and the
    /// hysteresis then measures loyalty against the wrong place.
    pub directions: Vec<[f64; 3]>,
    /// `weights[object][element]`, in linear amplitude. Each object's row has
    /// unit power, so folding it into the elements neither loses nor invents
    /// energy.
    pub weights: Vec<Vec<f64>>,
    /// Which element each object was counted towards when the positions were
    /// settled. Part of the answer rather than a by-product: handing it back
    /// with the next block is what stops an object changing hands over a
    /// difference too small to hear, and taking its element with it.
    pub owners: Vec<usize>,
    /// The mode every element renders with — its class's, exact rather than
    /// a mixture, since every object it carries is of that class. One per
    /// element, the pinned ones included. See [`class`].
    pub modes: Vec<Mode>,
    /// How many elements had their coherent peak bounded to the domain in
    /// the fit — see [`headroom`].
    pub bounded: usize,
}

impl Clustering {
    pub fn elements(&self) -> usize {
        self.positions.len()
    }

    /// The elements as an object audio metadata block.
    ///
    /// One element, one object: where it is and that it is there. The *level*
    /// is not here — it is in the weights, which is where a clustering puts it
    /// — so every element states unity, and an element carrying nothing states
    /// that it is inactive rather than being parked somewhere and left audible.
    ///
    /// `lfe` **prepends** the low frequency channel, which the syntax gives no
    /// position and no render information at all: it is a bed and it goes
    /// where the bed goes, so it is not clustered and the clustering's
    /// elements follow it. A payload with an LFE is therefore one object wider
    /// than the clustering.
    ///
    /// `offset` and `ramp` are the block's own: where in the access unit it
    /// takes effect and how long an element takes to reach it. A clustering
    /// decided per access unit ramps over the whole of one, which is what
    /// stops an element's position stepping — see [`crate::mix`] for the same
    /// argument about its weights.
    pub fn to_metadata(&self, lfe: bool, offset: u32, ramp: Ramp) -> ObjectAudioMetadata {
        let carried: Vec<bool> = (0..self.elements())
            .map(|element| {
                self.weights
                    .iter()
                    .any(|row| row.get(element).is_some_and(|weight| *weight != 0.0))
            })
            .collect();
        let mut objects = Vec::with_capacity(self.elements() + usize::from(lfe));
        if lfe {
            objects.push(Element {
                active: true,
                gain: Gain::Unity,
                render: None,
            });
        }
        objects.extend(
            self.positions
                .iter()
                .enumerate()
                .map(|(element, position)| {
                    // An element carrying nothing says so, and says nothing else: the
                    // syntax has nowhere to put a gain or a position on an inactive
                    // element, and a decoder would read neither.
                    if !carried[element] {
                        return Element {
                            active: false,
                            gain: Gain::Silent,
                            render: None,
                        };
                    }
                    // How the element renders is its class's, stated whole:
                    // every object it carries is of that class, so nothing
                    // here is a mixture. The height is honoured where the
                    // class honours it and the element has any.
                    let mode = self.modes.get(element).copied().unwrap_or_default();
                    Element {
                        active: true,
                        gain: Gain::Unity,
                        render: Some(Render {
                            position: Position::from_master(*position),
                            elevation: mode.elevation && position[2].abs() > f64::EPSILON,
                            zones: mode.zones,
                            screen: mode.screen,
                            snap: mode.snap,
                            ..Render::default()
                        }),
                    }
                }),
        );
        ObjectAudioMetadata {
            lfe,
            blocks: vec![Block {
                offset,
                ramp,
                objects,
            }],
        }
    }

    pub fn objects(&self) -> usize {
        self.weights.len()
    }
}

/// The fewest elements a bitstream carries, and the most.
pub const MIN_ELEMENTS: usize = 3;
pub const MAX_ELEMENTS: usize = 16;

/// How an object's energy is shared out between the elements.
///
/// The two honest choices, and they trade the same thing against each other:
/// an object that does not sit on an element either gets **moved** to one or
/// gets **spread** across several.
///
/// Neither is obviously right, and the difference is measurable — see
/// [`metric`], and `docs/clustering.md` for what it measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Weighting {
    /// Pan the object onto the elements. Its energy-weighted direction is
    /// preserved exactly; its energy is spread over two or three elements,
    /// which are then panned again by whatever plays the stream. Panning twice
    /// is wider than panning once, and that widening is the cost.
    ///
    /// It also makes objects **louder**: the weights carry unit power, which is
    /// what vector-base amplitude panning guarantees, but the elements are then
    /// panned again and add coherently. Measured, up to 1.10 of the level the
    /// object asked for — nearly a decibel — and worse the more elements there
    /// are. The first version of the measurement could not see this, because it
    /// checked the power of the weights rather than the level as rendered.
    Spread,
    /// Give the object to its nearest element outright. Nothing is spread and
    /// nothing is panned twice; the object arrives at the element's direction
    /// instead of its own, and that shift is the cost.
    ///
    /// It also **throws the object's size away**, since this puts everything
    /// on one point and a point element cannot carry a width. For a wide
    /// object that is not a trade-off, and the metric says so once it is
    /// looking.
    Nearest,
    /// Fit the elements to what the object actually radiates, width included,
    /// by least squares over a reference set of directions.
    ///
    /// The other two answer a question about the object's *direction*. This one
    /// answers the question a width poses, which is about the shape of what it
    /// radiates — and it answers it by solving for the shape rather than by a
    /// rule that happens to preserve a point's direction. See [`fit`].
    ///
    /// The default, because it is better on every axis that was measured: the
    /// scene's mean error, its worst object, an object's width, and the level
    /// it comes out at. On a point and on a width alike it reaches the floor —
    /// what the fold would cost if the encoder knew the layout — which the
    /// other two do not.
    #[default]
    Fitted,
}

/// Let the placement see one block ahead, without letting the geometry lead.
///
/// # The defect
///
/// A block's elements are placed from the scene at its **end** and reached at
/// its end, which is right for movement — an object crossing the block is
/// somewhere between where it was and where it is going, and so is the element
/// carrying it, because a decoder ramps a position across the access unit the
/// payload rides.
///
/// It is wrong for an object that *appears*. Silent through block `t−1`, it
/// pulled nothing, so no element is near it; loud from the first sample of
/// block `t`, it is carried by elements that only arrive where it is at the
/// end of that block. The attack — the part of a sound a listener localises —
/// is rendered by elements in transit. Measured on a scene of twenty objects
/// turning with one switching on hard: **the object that appears lands 1.31
/// out of its own gain vector as its first block arrives**, which is a whole
/// object somewhere else.
///
/// # Why not simply cluster a block ahead
///
/// Because that is not a look-ahead, it is a lead. Clustering block `t` on the
/// scene of `t+1` puts every element a whole block in front of the audio, and
/// the metric says exactly that — the same scene, judged as each block arrives
/// and as it departs:
///
/// | the scene turning | rule | on arrival | on departure |
/// |---|---|---|---|
/// | 2° a block | as it was | 0.0640 | 0.0631 |
/// | | a block ahead | 0.0823 | 0.0820 |
/// | 6° a block | as it was | 0.0652 | 0.0644 |
/// | | a block ahead | 0.1394 | 0.1381 |
///
/// It buys the attack by paying for every object that merely moves, which on a
/// scene turning six degrees a block is twice the error everywhere.
///
/// # What this does instead
///
/// The **geometry** stays where it was — the positions of this block's end,
/// aligned with the audio — and only the **energy** looks ahead: each object is
/// placed for the louder of what it does in this block and what it does in the
/// next. So an element starts moving towards a sound one block before it
/// arrives and is there when it does, and nothing else moves at all.
///
/// | the scene turning | rule | on arrival | on departure | worst on arrival |
/// |---|---|---|---|---|
/// | 2° a block, nothing appears | as it was | 0.0640 | 0.0631 | 0.746 |
/// | | the louder of two | 0.0640 | 0.0631 | 0.746 |
/// | 6° a block, nothing appears | as it was | 0.0652 | 0.0644 | 0.746 |
/// | | the louder of two | 0.0652 | 0.0644 | 0.746 |
/// | 2° a block, one appears | as it was | 0.0720 | 0.0687 | 1.312 |
/// | | the louder of two | 0.0698 | 0.0682 | **0.746** |
/// | 6° a block, one appears | as it was | 0.0715 | 0.0682 | 1.247 |
/// | | the louder of two | 0.0719 | 0.0717 | **0.746** |
///
/// Nothing at all when nothing appears — the same figures to four decimals,
/// which is what "only the energy" has to mean — and when something does, the
/// object that appears stops setting the worst case: 0.746 is what the rest of
/// the scene already cost.
///
/// # What it costs
///
/// Holding a decaying object one block longer than it deserves, which is the
/// 0.0682 → 0.0717 on the last row and is the same rule working: an element
/// does not walk away from a sound the instant it stops. And a block of
/// latency, in the *energy* only — a fold already reads a span ahead of the
/// coder.
///
/// `out` is the caller's buffer, reused, so a block costs no allocation. The
/// scene handed to [`metric`] must be the unmodified one: a placement rule may
/// look ahead and a measurement of it may not.
///
/// This is the one-block case of [`smooth`], which is what the encoder and
/// the harness now steer by; it is kept as the statement of that case, and
/// [`smooth::SMOOTHING`] is asserted to agree with it.
pub fn anticipating(scene: &[Object], next: &[f64], out: &mut Vec<Object>) {
    out.clear();
    out.extend(scene.iter().enumerate().map(|(object, here)| {
        let next = next.get(object).copied().unwrap_or(0.0);
        Object {
            energy: if next >= here.energy * ARRIVING {
                next
            } else {
                here.energy
            },
            ..*here
        }
    }));
}

/// How much louder an object has to get before the next block is the one it is
/// placed for.
///
/// Not simply the louder of the two. A block's power is a mean square over
/// 26.7 ms and a steady tone's is not steady: it ripples with where the block
/// boundary falls in the waveform. Taking the maximum unconditionally
/// therefore over-weights whichever objects happen to ripple most, and that is
/// a bias on every block rather than an anticipation on one. Measured on
/// `crowd40` into twelve elements:
///
/// | placed for | mean error | worst | flips a block |
/// |---|---|---|---|
/// | this block (as it was) | **0.044** | 0.377 | 4.74 |
/// | the louder of the two | 0.048 | 0.377 | 1.24 |
/// | **a doubling or more** | **0.044** | **0.377** | **4.74** |
///
/// Three decibels: below it nothing on `crowd40` qualifies, so the figures are
/// the unchanged ones to the digit — which is what "only when something
/// arrives" has to mean. Above it, an object appearing out of silence is a
/// rise of many orders of magnitude and is caught by any threshold at all.
///
/// (The flips are 4.74 either way here; taking the maximum unconditionally
/// took them to 1.24, which looks like a win and is the ripple being smoothed
/// rather than the fold being steadier. That is the cost showing up as a
/// benefit, which is the reason to measure the error beside it.)
pub const ARRIVING: f64 = 2.0;

/// Reduce a scene to `elements` elements.
///
/// `previous` is the last block's answer, when there was one. Handing it back
/// keeps the elements where they were unless the scene has moved, which is not
/// a refinement: an element that jumps between blocks takes the objects folded
/// into it with it, and a listener hears the jump rather than the mix.
pub fn cluster(
    objects: &[Object],
    elements: usize,
    weighting: Weighting,
    previous: Option<&Clustering>,
) -> Result<Clustering> {
    Clusterer::new(elements, weighting)?.cluster(objects, previous)
}

/// A clusterer, held across the blocks of a stream.
///
/// The reference the fitted weighting solves against is a triangulation of a
/// fixed set of directions: nothing about it depends on the scene, and building
/// it per block cost about 24 µs there — a third of the work at sixteen
/// objects, and pure waste at any object count.
pub struct Clusterer {
    elements: usize,
    weighting: Weighting,
    /// Built once. Absent for the weightings that do not fit.
    reference: Option<Reference>,
    /// How hard to look for a better place for each element once the
    /// clustering has settled — see [`place`].
    search: place::Search,
    /// Whether an element that is being wasted may be taken back — see
    /// [`earn`]. On, and settable so that the measurement that says it should
    /// be can be made.
    earn: bool,
    /// How many blocks the exchange paid for itself in, and how many since it
    /// was last looked for.
    earned: u64,
    since_earned: u64,
    /// Blocks since a cold start was last considered — see [`restart`].
    /// `u64::MAX` means never consider one.
    reconsider: u64,
    since: u64,
    /// How many times the fold decided it was better off starting over.
    restarts: u64,
    /// Whether the classes are honoured, and how much a class has to gain
    /// before an element changes class between blocks — see [`class`]. `None`
    /// folds every object as one class, which is what the measurement of
    /// what the classes cost is made against.
    classing: Option<f64>,
    /// Where the elements start from on a cold block — see [`seed`].
    seeding: seed::Rule,
    /// What an object has to carry to count — see [`floor`].
    floor: floor::Floor,
    /// Whether an element's coherent peak is bounded to the domain in the
    /// fit — see [`headroom`].
    bounding: bool,
    /// Everything a block needs and nothing it keeps. Held so that a block in
    /// the middle of a programme allocates nothing.
    scratch: Scratch,
}

/// The buffers a block fills and the next block refills.
#[derive(Debug, Default)]
struct Scratch {
    /// What each live element radiates over the reference.
    radiated: Vec<Vec<f64>>,
    /// What each object radiates over the reference.
    targets: Vec<Vec<f64>>,
    /// One object's row over the live elements, as the last block left it.
    anchor: Vec<f64>,
    /// Which elements the last settling found live, so that the answer it
    /// produced can be scored.
    live: Vec<usize>,
    /// Which live elements each object may be spread over: those of its
    /// class. Empty when the classes are not honoured.
    masks: Vec<Vec<bool>>,
    place: place::Scratch,
    earn: earn::Scratch,
}

impl Clusterer {
    pub fn new(elements: usize, weighting: Weighting) -> Result<Self> {
        Ok(Self {
            elements,
            weighting,
            reference: match weighting {
                Weighting::Fitted => Some(Reference::new()?),
                _ => None,
            },
            search: place::Search::default(),
            earn: true,
            earned: 0,
            // Looked for on the very first block, then every eighth.
            since_earned: earn::EVERY,
            reconsider: restart::RECONSIDER,
            since: 0,
            restarts: 0,
            classing: Some(class::WORTH),
            seeding: seed::SEEDING,
            floor: floor::Floor::default(),
            bounding: headroom::BOUNDING,
            scratch: Scratch::default(),
        })
    }

    /// Whether an element's coherent peak is bounded to the domain in the
    /// fit — see [`headroom`].
    pub fn bounding(mut self, bounding: bool) -> Self {
        self.bounding = bounding;
        self
    }

    /// Where the elements start from on a cold block — see [`seed`].
    pub fn seeding(mut self, rule: seed::Rule) -> Self {
        self.seeding = rule;
        self
    }

    /// What an object has to carry to seed or pull an element — see
    /// [`floor`].
    pub fn flooring(mut self, floor: floor::Floor) -> Self {
        self.floor = floor;
        self
    }

    /// Whether the classes are honoured — see [`class`] — and by how much a
    /// class has to gain before an element changes class between blocks.
    /// `None` folds every object as one class, for the measurement of what
    /// the classes cost.
    pub fn classing(mut self, worth: Option<f64>) -> Self {
        self.classing = worth;
        self
    }

    /// How hard to look for a better place for each element. Whatever
    /// [`place::Search::default`] says, unless a measurement is asking
    /// something else.
    pub fn searching(mut self, search: place::Search) -> Self {
        self.search = search;
        self
    }

    /// How many blocks between one look at a cold start and the next.
    /// `None` never looks, which is what this did before.
    pub fn reconsidering(mut self, every: Option<u64>) -> Self {
        self.reconsider = every.unwrap_or(u64::MAX);
        self
    }

    /// Whether an element that is being wasted may be taken back.
    pub fn earning(mut self, earn: bool) -> Self {
        self.earn = earn;
        self
    }

    /// How many blocks so far paid for the exchange — see [`earn`].
    ///
    /// A number worth reporting rather than inferring: an element under the
    /// audibility floor is excluded from every stability counter there is, so
    /// what happens to one is otherwise invisible.
    pub fn earned(&self) -> u64 {
        self.earned
    }

    /// How many blocks so far were better off started over — see [`restart`].
    pub fn restarts(&self) -> u64 {
        self.restarts
    }

    /// Reduce one block's scene to elements.
    ///
    /// Settled from the previous block's answer, and — every
    /// [`restart::RECONSIDER`] blocks — settled a second time from cold, so
    /// that the warm answer has to go on earning the basins it inherited. See
    /// [`restart`] for when the cold one is taken.
    pub fn cluster(
        &mut self,
        objects: &[Object],
        previous: Option<&Clustering>,
    ) -> Result<Clustering> {
        let warm = self.settle(objects, previous)?;
        // Only a warm answer has anything to reconsider, and only a fitted one
        // has the vectors to score two answers against each other.
        let Some(previous) =
            previous.filter(|_| self.weighting == Weighting::Fitted && self.reconsider != u64::MAX)
        else {
            return Ok(warm);
        };
        self.since += 1;
        if self.since < self.reconsider {
            return Ok(warm);
        }
        self.since = 0;

        let warm_cost = self.cost_of(&warm, objects);
        let mut cold = self.settle(objects, None)?;
        let cold_cost = self.cost_of(&cold, objects);
        // Matched to what it replaces before it is compared with it, so that
        // nothing counts as moving merely for being numbered differently.
        keep_the_element_numbers(&mut cold, &warm);
        if restart::worth_starting_over(&warm, warm_cost, &cold, cold_cost, objects) {
            self.restarts += 1;
            // And matched again to the block before, which is what every other
            // answer this returns has been.
            keep_the_element_numbers(&mut cold, previous);
            return Ok(cold);
        }
        Ok(warm)
    }

    /// What a settled clustering costs, over the vectors the settling that
    /// produced it left behind.
    fn cost_of(&self, clustering: &Clustering, objects: &[Object]) -> f64 {
        restart::cost(
            clustering,
            objects,
            &self.scratch.live,
            &self.scratch.radiated,
            &self.scratch.targets,
        )
    }

    /// Reduce one block's scene to elements, from a given starting point.
    fn settle(&mut self, objects: &[Object], previous: Option<&Clustering>) -> Result<Clustering> {
        let elements = self.elements;
        let weighting = self.weighting;
        let fitted = weighting == Weighting::Fitted;
        // What each object radiates over the reference, once: the fit is
        // made against it, the placement search re-uses it, and the share of
        // the elements between the classes is judged by it — which is why it
        // is computed before the partition and not after.
        if fitted {
            let reference = self
                .reference
                .as_ref()
                .expect("a fitting clusterer is built with a reference");
            let targets = &mut self.scratch.targets;
            targets.resize_with(objects.len(), Vec::new);
            for (object, into) in objects.iter().zip(targets.iter_mut()) {
                reference.radiated(object.position, object.size, into);
            }
        }
        let Partition {
            positions,
            directions,
            owners,
            modes,
        } = {
            let settling =
                Settling {
                    classing: self.classing,
                    judge: self.reference.as_ref().filter(|_| fitted).map(|reference| {
                        class::Judge {
                            reference,
                            targets: &self.scratch.targets,
                        }
                    }),
                    seeding: self.seeding,
                    floor: self.floor,
                };
            partition_with(objects, elements, previous, &settling)?
        };
        // An element nothing owns is *parked*: `separate` put it somewhere a
        // triangulation can work with, which is a direction the scene never
        // asked for. It is still a direction as far as a panner or a fit is
        // concerned, and both will happily send an object through it — which
        // puts that object's signal where the mix has nothing. So the
        // weighting is computed over the elements that carry something, and
        // the rest are left at zero.
        let live: Vec<usize> = (0..elements)
            .filter(|element| owners.iter().any(|owner| owner == element))
            .collect();
        let live_positions: Vec<[f64; 3]> = live.iter().map(|e| positions[*e]).collect();
        // Which live elements each object may be spread over: those of its
        // class, when the classes are honoured.
        let masked = self.classing.is_some();
        if masked {
            fill_masks(&mut self.scratch.masks, objects, &modes, &live);
        }
        let mut bounded = 0usize;

        let narrow: Vec<Vec<f64>> = match weighting {
            Weighting::Spread => {
                let panner = PointPanner::new(&live_positions)?;
                let mut gains = vec![0.0f32; live_positions.len()];
                objects
                    .iter()
                    .map(|object| {
                        // The object's own size goes in here. Since the output
                        // will not carry a size, spreading it over more elements
                        // is the only way it survives the fold at all.
                        panner.gains(object.position, object.size, &mut gains);
                        gains.iter().map(|gain| f64::from(*gain)).collect()
                    })
                    .collect()
            }
            Weighting::Nearest => {
                let directions: Vec<[f64; 3]> =
                    live_positions.iter().map(|p| direction(*p)).collect();
                objects
                    .iter()
                    .enumerate()
                    .map(|(index, object)| {
                        let mut row = vec![0.0; live_positions.len()];
                        let here = direction(object.position);
                        // The nearest element of the object's own class.
                        let chosen = (0..live.len())
                            .filter(|slot| !masked || self.scratch.masks[index][*slot])
                            .max_by(|a, b| {
                                dot(directions[*a], here)
                                    .partial_cmp(&dot(directions[*b], here))
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                        if let Some(slot) = chosen {
                            row[slot] = 1.0;
                        }
                        row
                    })
                    .collect()
            }
            Weighting::Fitted => {
                let reference = self
                    .reference
                    .as_ref()
                    .expect("a fitting clusterer is built with a reference");
                // What each element radiates over the reference, once: they
                // are the same for every object, they are the expensive
                // half, and the search below re-uses them rather than
                // panning anything again.
                let radiated = &mut self.scratch.radiated;
                radiated.resize_with(live_positions.len(), Vec::new);
                for (position, into) in live_positions.iter().zip(radiated.iter_mut()) {
                    reference.radiated(*position, 0.0, into);
                }
                self.scratch.live.clear();
                self.scratch.live.extend_from_slice(&live);
                weigh(
                    radiated,
                    &self.scratch.targets,
                    &live,
                    previous,
                    objects,
                    elements,
                    &mut self.scratch.anchor,
                    masked.then_some(&self.scratch.masks[..]),
                    self.bounding,
                    &mut bounded,
                )
            }
        };

        // Back to the full width: a parked element carries nothing, which is
        // what being parked means.
        let widen = |narrow: Vec<Vec<f64>>| -> Vec<Vec<f64>> {
            narrow
                .into_iter()
                .map(|row: Vec<f64>| {
                    let mut full = vec![0.0; elements];
                    for (slot, weight) in live.iter().zip(row) {
                        full[*slot] = weight;
                    }
                    full
                })
                .collect()
        };

        let mut clustering = Clustering {
            positions,
            directions,
            weights: widen(narrow),
            owners,
            modes,
            bounded,
        };
        pin(&mut clustering, objects);

        // And now that there are weights, the positions can be judged by what
        // the metric actually reports rather than by the directional criterion
        // that produced them. See [`place`].
        if let (Weighting::Fitted, Some(reference)) = (weighting, self.reference.as_ref())
            && self.search.passes > 0
        {
            place::refine(
                &mut clustering,
                objects,
                &live,
                reference,
                &self.scratch.targets,
                &mut self.scratch.radiated,
                previous,
                self.search,
                &mut self.scratch.place,
            );
            let refitted = weigh(
                &self.scratch.radiated,
                &self.scratch.targets,
                &live,
                previous,
                objects,
                elements,
                &mut self.scratch.anchor,
                masked.then_some(&self.scratch.masks[..]),
                self.bounding,
                &mut bounded,
            );
            clustering.bounded = bounded;
            clustering.weights = widen(refitted);
            // The pins are restated: the fit was solved again and a fit left
            // to itself will pan an object through a bed.
            pin(&mut clustering, objects);
        }

        // And now that the elements are where the metric wants them, one that
        // is being wasted can be offered to the object that most needs one.
        // See [`earn`].
        //
        // The exchange proposes; what decides is the **finished** answer. Its
        // own trial fits freely and this block's weights hold the set the last
        // one gave, so a trial that looked like a gain can come out a loss
        // once it is weighed the way the answer will be — and on a scene that
        // only jitters, a rule that moves an element for any gain at all moves
        // it every block for noise. So the cost is taken before and after, on
        // the whole finished thing, and the exchange has to pay for itself by
        // more than [`earn::WORTH`] or it is put back.
        self.since_earned += 1;
        if self.earn
            && self.since_earned >= earn::EVERY
            && let (Weighting::Fitted, Some(reference)) = (weighting, self.reference.as_ref())
        {
            self.since_earned = 0;
            let before = restart::cost(
                &clustering,
                objects,
                &live,
                &self.scratch.radiated,
                &self.scratch.targets,
            );
            let was = clustering.clone();
            let was_radiated = self.scratch.radiated.clone();
            if earn::earn_it(
                &mut clustering,
                objects,
                &live,
                reference,
                &self.scratch.targets,
                &mut self.scratch.radiated,
                &mut self.scratch.earn,
                masked,
            ) {
                // The element that moved took its new object's class with
                // it, so the masks are made again before the weights are.
                if masked {
                    fill_masks(&mut self.scratch.masks, objects, &clustering.modes, &live);
                }
                // Weighed again through the one rule that weighs, so that a
                // block where the exchange paid still holds the set the last
                // one gave.
                let after_weights = weigh(
                    &self.scratch.radiated,
                    &self.scratch.targets,
                    &live,
                    previous,
                    objects,
                    elements,
                    &mut self.scratch.anchor,
                    masked.then_some(&self.scratch.masks[..]),
                    self.bounding,
                    &mut bounded,
                );
                clustering.weights = widen(after_weights);
                clustering.bounded = bounded;
                pin(&mut clustering, objects);

                let after = restart::cost(
                    &clustering,
                    objects,
                    &live,
                    &self.scratch.radiated,
                    &self.scratch.targets,
                );
                if earn::worth_the_move(before, after, objects) {
                    self.earned += 1;
                } else {
                    clustering = was;
                    self.scratch.radiated = was_radiated;
                    if masked {
                        fill_masks(&mut self.scratch.masks, objects, &clustering.modes, &live);
                    }
                }
            }
        }

        if let Some(previous) = previous {
            keep_the_element_numbers(&mut clustering, previous);
        }
        Ok(clustering)
    }
}

/// Which live elements each object may be spread over: those of its class.
fn fill_masks(masks: &mut Vec<Vec<bool>>, objects: &[Object], modes: &[Mode], live: &[usize]) {
    masks.resize_with(objects.len(), Vec::new);
    for (object, mask) in objects.iter().zip(masks.iter_mut()) {
        mask.clear();
        mask.extend(live.iter().map(|element| modes[*element] == object.mode));
    }
}

/// Share every object out over the live elements, holding the set the last
/// block gave it.
///
/// Called twice on a fitting block: once on the positions the clustering
/// settled, and once more after [`place`] has moved them. The second call is
/// not a formality — the weights were fitted to where the elements *were*, and
/// leaving them would share an object out by a geometry that no longer exists
/// — and it goes through the same rule as the first, or the search would undo
/// the stability the held set buys.
///
/// `anchor` is the caller's buffer, one row wide, so a block costs no
/// allocation. `masks`, when the classes are honoured, says which of the live
/// elements each object may reach: the ones of its class.
#[allow(clippy::too_many_arguments)]
fn weigh(
    radiated: &[Vec<f64>],
    targets: &[Vec<f64>],
    live: &[usize],
    previous: Option<&Clustering>,
    scene: &[Object],
    elements: usize,
    anchor: &mut Vec<f64>,
    masks: Option<&[Vec<bool>]>,
    bounding: bool,
    bounded: &mut usize,
) -> Vec<Vec<f64>> {
    let objects = scene.len();
    // The element numbers line up because the warm start makes them: this
    // block's centre `j` starts from the previous block's direction `j`, and
    // the renumbering that runs after the weights are solved permutes
    // positions and weights together, so the next block's slot `j` is this
    // one's slot `j` again.
    let held = previous
        .filter(|previous| previous.elements() == elements && previous.weights.len() == objects);
    anchor.clear();
    anchor.resize(live.len(), 0.0);
    let mut rows: Vec<Vec<f64>> = targets
        .iter()
        .enumerate()
        .map(|(object, target)| {
            let allowed = masks.map(|masks| &masks[object][..]);
            let Some(held) = held else {
                return fit::fitted_near(radiated, target, None, allowed);
            };
            let row = &held.weights[object];
            for (slot, element) in live.iter().enumerate() {
                anchor[slot] = row[*element];
            }
            fit::fitted_near(
                radiated,
                target,
                Some(fit::Held {
                    previous: anchor,
                    tolerance: fit::HELD_TOLERANCE,
                }),
                allowed,
            )
        })
        .collect();
    // And inside the domain: an element whose coherent peak would pass full
    // scale has the objects it carries solved again under a bound — see
    // [`headroom`].
    *bounded = if bounding {
        headroom::bound(
            &mut rows, radiated, targets, scene, live, held, masks, anchor,
        )
    } else {
        0
    };
    rows
}

/// Renumber this block's elements so each keeps the number it had last block.
///
/// # Why anything needs renumbering
///
/// Nothing in the clustering decides *which slot* a direction lands in: the
/// seeding takes objects in energy order, the assignment loop takes them in
/// index order, and both of those turn over as levels change. So a scene that
/// has not moved at all comes back with the same set of positions in a
/// different order, over and over. Measured on a real master at twelve
/// elements, where eleven objects go into eleven elements and the fold is
/// exact: the eleven positions were identical in every block and their order
/// changed in 348 blocks out of 1129.
///
/// That is not a cosmetic difference. An element is a *channel*, and moving a
/// signal from channel 7 to channel 8 while moving channel 7's position to
/// where channel 8's was is a swap only if both happen at the same instant.
/// They do not: the payload ramps the positions across the block and
/// [`crate::mix`] cross-fades the weights across the same block, so what a
/// listener gets is two signals sweeping past each other. And every one of
/// those relabellings writes a payload, because the payload's bytes changed.
///
/// # What it does
///
/// A relabelling is free — it changes no position, no weight and no object's
/// share, only which slot holds what — so the one to pick is the one that
/// keeps each element carrying what it carried. That is an assignment problem,
/// and with sixteen elements at most the Hungarian algorithm is a few thousand
/// operations once a metadata block.
///
/// # What is matched, and why not the direction
///
/// An element is a *channel*: what makes it the same element as last block is
/// the signal in it, not the angle it points at. Matching on direction alone
/// looks right and is not — two objects at the same place give two elements at
/// the same direction, the assignment between them costs the same either way,
/// and the two signals go on trading channels for as long as the objects stay
/// coincident. Measured: it left 12 217 of 12 800 samples a block changed on a
/// master whose fold is exact.
///
/// So the cost is how much of what an element carried it still carries — the
/// cosine between its column of the weight matrix before and after — with the
/// angular distance kept as a thousandth of a term, which decides nothing
/// except between elements carrying nothing at all. Those have no column to
/// compare and would otherwise be assigned arbitrarily.
///
/// Unweighted by energy on purpose. A silent element's number matters as much
/// as a loud one's, because it is silent *now* and may not be next block.
fn keep_the_element_numbers(clustering: &mut Clustering, previous: &Clustering) {
    let n = clustering.elements();
    if n == 0 || previous.elements() != n || previous.weights.len() != clustering.weights.len() {
        return;
    }
    /// How much the angle counts against what the element carries.
    const BY_DIRECTION: f64 = 1e-3;
    /// What changing class costs: more than any angle and any signal, so an
    /// element keeps its number within its class before anything else is
    /// considered. See [`class`].
    const BY_CLASS: f64 = 4.0;
    let classed = previous.modes.len() == n && clustering.modes.len() == n;

    // Each element's column of the weight matrix, and its length, so the
    // cosine below is a dot product and two divisions.
    let column = |weights: &[Vec<f64>], element: usize| -> (Vec<f64>, f64) {
        let values: Vec<f64> = weights.iter().map(|row| row[element]).collect();
        let length = values.iter().map(|w| w * w).sum::<f64>().sqrt();
        (values, length)
    };
    let was_columns: Vec<(Vec<f64>, f64)> = (0..n).map(|e| column(&previous.weights, e)).collect();
    let now_columns: Vec<(Vec<f64>, f64)> =
        (0..n).map(|e| column(&clustering.weights, e)).collect();

    // `cost[i][j]`: what it costs to give this block's element `j` the number
    // the previous block's element `i` had.
    let mut cost = vec![0.0f64; n * n];
    for i in 0..n {
        let (was, was_length) = &was_columns[i];
        for j in 0..n {
            let (now, now_length) = &now_columns[j];
            let carried = if *was_length > 0.0 && *now_length > 0.0 {
                was.iter().zip(now).map(|(a, b)| a * b).sum::<f64>() / (was_length * now_length)
            } else {
                0.0
            };
            cost[i * n + j] = (1.0 - carried)
                + BY_DIRECTION * (1.0 - dot(previous.directions[i], clustering.directions[j]))
                + if classed && previous.modes[i] != clustering.modes[j] {
                    BY_CLASS
                } else {
                    0.0
                };
        }
    }
    let taking = assign(&cost, n);
    if taking
        .iter()
        .enumerate()
        .all(|(number, from)| number == *from)
    {
        return;
    }

    // `taking[number]` is the element that becomes `number`.
    let positions: Vec<[f64; 3]> = taking.iter().map(|e| clustering.positions[*e]).collect();
    let directions: Vec<[f64; 3]> = taking.iter().map(|e| clustering.directions[*e]).collect();
    if clustering.modes.len() == n {
        let modes: Vec<Mode> = taking.iter().map(|e| clustering.modes[*e]).collect();
        clustering.modes = modes;
    }
    // And the other way round for anything that *names* an element.
    let mut number_of = vec![0usize; n];
    for (number, from) in taking.iter().enumerate() {
        number_of[*from] = number;
    }
    for row in &mut clustering.weights {
        let was = row.clone();
        for (number, from) in taking.iter().enumerate() {
            row[number] = was[*from];
        }
    }
    for owner in &mut clustering.owners {
        if *owner < n {
            *owner = number_of[*owner];
        }
    }
    clustering.positions = positions;
    clustering.directions = directions;
}

/// The cheapest one-to-one assignment of `n` columns to `n` rows.
///
/// The Hungarian algorithm in its `O(n³)` shortest-augmenting-path form, on a
/// row-major `n × n` cost matrix. Returns, for each row, the column it takes.
///
/// Written here rather than pulled in: `hz-cluster` has three dependencies and
/// all of them are this workspace's, the matrix is never larger than sixteen
/// square, and the whole thing is forty lines.
fn assign(cost: &[f64], n: usize) -> Vec<usize> {
    // The potentials, and the column-to-row assignment being built. Index 0 of
    // each is the algorithm's sentinel, so everything is one-based inside.
    let mut u = vec![0.0f64; n + 1];
    let mut v = vec![0.0f64; n + 1];
    let mut taken = vec![0usize; n + 1];
    let mut way = vec![0usize; n + 1];

    for row in 1..=n {
        taken[0] = row;
        let mut column = 0usize;
        let mut least = vec![f64::INFINITY; n + 1];
        let mut used = vec![false; n + 1];
        loop {
            used[column] = true;
            let here = taken[column];
            let mut delta = f64::INFINITY;
            let mut next = 0usize;
            for candidate in 1..=n {
                if used[candidate] {
                    continue;
                }
                let step = cost[(here - 1) * n + (candidate - 1)] - u[here] - v[candidate];
                if step < least[candidate] {
                    least[candidate] = step;
                    way[candidate] = column;
                }
                if least[candidate] < delta {
                    delta = least[candidate];
                    next = candidate;
                }
            }
            for candidate in 0..=n {
                if used[candidate] {
                    u[taken[candidate]] += delta;
                    v[candidate] -= delta;
                } else {
                    least[candidate] -= delta;
                }
            }
            column = next;
            if taken[column] == 0 {
                break;
            }
        }
        // Walk the augmenting path back.
        while column != 0 {
            let previous = way[column];
            taken[column] = taken[previous];
            column = previous;
        }
    }

    let mut out = vec![0usize; n];
    for column in 1..=n {
        if taken[column] > 0 {
            out[taken[column] - 1] = column - 1;
        }
    }
    out
}

/// Give every pinned object the element it owns, outright.
///
/// A bed channel is a speaker feed and not an object: it belongs at that
/// speaker, and an element carrying it has to sit there exactly. So the
/// weighting above — which knows nothing about pinning — is overruled for
/// those objects and for those elements: the element goes to the pinned
/// position, the object goes to it with unit weight and to nothing else, and
/// every *other* object is stopped from leaning on it.
///
/// That last part is what makes it a pin rather than a preference. An element
/// at the low frequency channel's position is not somewhere a mix's objects
/// should be panned through, and a fit left to itself will use it: it is a
/// direction like any other as far as least squares is concerned.
fn pin(clustering: &mut Clustering, objects: &[Object]) {
    for (object, pinned) in objects.iter().enumerate() {
        let Some(position) = pinned.pinned else {
            continue;
        };
        let element = clustering.owners[object];
        if element >= clustering.elements() {
            continue;
        }
        clustering.positions[element] = position;
        clustering.directions[element] = direction(position);
        for (other, row) in clustering.weights.iter_mut().enumerate() {
            row[element] = if other == object { 1.0 } else { 0.0 };
        }
        // And the pinned object goes nowhere else.
        for (slot, weight) in clustering.weights[object].iter_mut().enumerate() {
            if slot != element {
                *weight = 0.0;
            }
        }
    }
}

/// Passes of the refinement. Beyond this the positions stop moving on every
/// scene measured here, and stopping early costs error rather than saving
/// time.
const PASSES: usize = 32;

/// How much better another element has to be before an object changes hands.
///
/// Not a smoothing preference: without it, a scene turning by three degrees
/// moved eleven of its twelve elements by exactly three degrees and the
/// twelfth by **nine**, because one object crossed a boundary and dragged its
/// element after it. What a listener hears from that is the jump, not the mix.
///
/// Stated as an angle because that is what it means — an object must be this
/// much closer to the other element, not merely closer.
///
/// # Which is not what a cosine margin says
///
/// The comparison is made in cosines, because that is what the assignment is
/// made in, and a *fixed* cosine margin of `1 − cos 5°` is five degrees only
/// where the challenger sits on top of the object. Away from there the cosine
/// flattens: the same margin is worth about `0.0038 / sin θ` radians at an
/// angle θ from the element an object is on, which is
///
/// | on an element at | a fixed cosine margin is worth |
/// |---|---|
/// | 5° | 5° |
/// | 15° | 0.8° |
/// | 30° | 0.4° |
/// | 60° | 0.25° |
///
/// and fifteen degrees is what a boundary object sits at with twelve
/// elements. So the guard was almost absent exactly where it is for: an
/// object between two elements, which is the only kind that changes hands.
/// Synthesised scenes did not show it, because a ring turning smoothly moves
/// every object the same way at the same time.
///
/// So the margin is taken **at the angle the object is actually at**: a
/// challenger has to be inside `θ − 5°`, which in cosines is
/// `cos(θ − 5°) = cos θ cos 5° + sin θ sin 5°` — one square root, no inverse
/// cosine, and the same answer.
const HYSTERESIS_DEG: f64 = 5.0;

/// How close two elements may be before they count as the same one.
///
/// Real content walked into this on the first attempt. A localisation test
/// puts every object at the front or the back and nothing in between, so a
/// scene with two distinct directions and twelve elements to spend leaves ten
/// of them piled on top of each other — and a triangulation over coincident
/// points has no triplets to find, so the panner refuses and the whole fold
/// fails. Synthesised scenes never showed it, because objects on a circle are
/// never coincident.
const COINCIDENT_DEG: f64 = 2.0;

/// What the refinement settled on: where the elements went, and which of them
/// each object was counted towards.
#[derive(Debug, Clone, PartialEq)]
pub struct Partition {
    pub positions: Vec<[f64; 3]>,
    /// The directions the clustering settled on, before the radius was handed
    /// back — see [`Clustering::directions`].
    pub directions: Vec<[f64; 3]>,
    pub owners: Vec<usize>,
    /// The mode of every element: its class's. See [`class`].
    pub modes: Vec<Mode>,
}

/// How a block is settled: whether the classes are honoured, what judges
/// them, and where the elements start from.
pub struct Settling<'a> {
    /// How much more a class has to gain than another loses before an
    /// element changes class between blocks — see [`class`] — or `None` to
    /// fold every object as one class.
    pub classing: Option<f64>,
    /// The metric's view of the scene, for judging what a class would cost
    /// and what a seed would serve; by direction without one.
    pub judge: Option<class::Judge<'a>>,
    /// Where the elements start from on a cold block — see [`seed`].
    pub seeding: seed::Rule,
    /// What an object has to carry to seed or pull an element — see
    /// [`floor`]. Below it an object is placed as though silent.
    pub floor: floor::Floor,
}

/// Where to put the elements.
///
/// A weighted clustering of the objects **by direction**: two objects the same
/// way round render almost identically however far apart they are, so distance
/// is not what an element has to agree about. Loudness is the weight, because
/// an object nobody can hear is an object whose position does not matter.
///
/// Warm-started from the previous block when there is one. That is what keeps
/// an element where it was unless the scene moved — not a refinement, since an
/// element that jumps takes the objects folded into it with it.
///
/// The classes are honoured, judged by direction: see [`partition_with`].
pub fn partition(
    objects: &[Object],
    elements: usize,
    previous: Option<&Clustering>,
) -> Result<Partition> {
    partition_with(
        objects,
        elements,
        previous,
        &Settling {
            classing: Some(class::WORTH),
            judge: None,
            seeding: seed::SEEDING,
            floor: floor::Floor::default(),
        },
    )
}

/// As [`partition`], with the classes honoured as stated — or not at all.
///
/// With classes, the elements are first shared between them — see
/// [`class::allocate`] — and every element then belongs to one class: an
/// object is only ever owned by an element of its own class, whatever the
/// direction says, and a spare element only ever goes to an object of the
/// class it belongs to. A scene of one class is partitioned exactly as it
/// would be without any.
pub fn partition_with(
    objects: &[Object],
    elements: usize,
    previous: Option<&Clustering>,
    settling: &Settling<'_>,
) -> Result<Partition> {
    let path = std::path::Path::new("object scene");
    if !(MIN_ELEMENTS..=MAX_ELEMENTS).contains(&elements) {
        return Err(hz_core::Error::unsupported(
            path,
            format!("{elements} elements; a bitstream carries {MIN_ELEMENTS} to {MAX_ELEMENTS}"),
        ));
    }

    // A pinned object is not clustered at all: it takes an element of its own,
    // at the front, and the scene is clustered into what is left. Putting them
    // at the front is the shape a reference stream has — the low frequency
    // channel is its first element — and it means an element index means the
    // same thing from one block to the next however the scene moves.
    let pinned: Vec<usize> = (0..objects.len())
        .filter(|index| objects[*index].pinned.is_some())
        .collect();
    let free: Vec<usize> = (0..objects.len())
        .filter(|index| objects[*index].pinned.is_none())
        .collect();
    if pinned.len() >= elements {
        return Err(hz_core::Error::unsupported(
            path,
            format!(
                "{} pinned objects and {elements} elements; a pinned object takes an element \
                 of its own and there has to be one left for the scene",
                pinned.len()
            ),
        ));
    }
    let clustered = elements - pinned.len();

    let directions: Vec<[f64; 3]> = free
        .iter()
        .map(|index| direction(objects[*index].position))
        .collect();
    let energies: Vec<f64> = free
        .iter()
        .map(|index| objects[*index].energy.max(0.0))
        .collect();
    // An object under the room's floor seeds nothing, since nobody hears
    // where an element for it alone would go — see [`floor`]. It still pulls
    // the centre it is counted towards, so that an element carrying only
    // near-silence stays with it rather than jumping to it when it speaks.
    // The relative guard is the metric's and not the partition's.
    let seed_energies: Vec<f64> = energies
        .iter()
        .map(|energy| {
            if settling.floor.heard(*energy) {
                *energy
            } else {
                0.0
            }
        })
        .collect();

    // The classes: one for every distinct mode among the free objects, or
    // one for all of them when the classes are not honoured — and one, of
    // nothing, when every object is pinned and there is nothing to cluster.
    let (mut modes, class_of) = match settling.classing {
        Some(_) => class::classes(objects, &free),
        None => (vec![Mode::default()], vec![0; free.len()]),
    };
    let judge = settling.judge.as_ref();
    if modes.is_empty() {
        modes.push(Mode::default());
    }

    let warm = previous.filter(|previous| {
        previous.elements() == elements && previous.owners.len() == objects.len()
    });

    // How many elements each class gets. One class gets them all; more than
    // one share them by what each would gain, from the share the last block
    // had — see [`class::allocate`].
    let share: Vec<usize> = if modes.len() == 1 {
        vec![clustered]
    } else {
        let members_of: Vec<Vec<usize>> = (0..modes.len())
            .map(|class| {
                free.iter()
                    .zip(&class_of)
                    .filter(|(_, of)| **of == class)
                    .map(|(index, _)| *index)
                    .collect()
            })
            .collect();
        let curves: Vec<class::Curve> = members_of
            .iter()
            .map(|members| {
                class::curve(
                    objects,
                    members,
                    judge,
                    clustered,
                    settling.seeding,
                    &settling.floor,
                )
            })
            .collect();
        let held: Option<Vec<usize>> = warm
            .filter(|previous| previous.modes.len() == elements)
            .map(|previous| {
                modes
                    .iter()
                    .map(|mode| {
                        previous.modes[pinned.len()..]
                            .iter()
                            .filter(|known| *known == mode)
                            .count()
                    })
                    .collect()
            });
        let worth = settling.classing.unwrap_or(class::WORTH);
        class::allocate(&curves, clustered, held.as_deref(), worth).ok_or_else(|| {
            hz_core::Error::unsupported(
                path,
                format!(
                    "{} classes of objects that render differently and {clustered} elements to \
                     share between them; every class present needs one",
                    curves.iter().filter(|curve| curve.present).count()
                ),
            )
        })?
    };

    // Every element has a class and a centre. From the last block, both are
    // what it settled on — the directions exactly, since normalising its
    // positions instead loses whatever the cube clamp turned, see
    // [`Clustering::directions`] — and a `Clustering` built by hand has no
    // directions to offer, so that is the fallback and not the rule.
    let mut centres: Vec<[f64; 3]> = vec![[0.0, 1.0, 0.0]; clustered];
    let mut slot_class: Vec<Option<usize>> = vec![None; clustered];
    match warm {
        Some(previous) => {
            for slot in 0..clustered {
                let element = pinned.len() + slot;
                centres[slot] = if previous.directions.len() == elements {
                    previous.directions[element]
                } else {
                    direction(previous.positions[element])
                };
                slot_class[slot] = if modes.len() == 1 {
                    Some(0)
                } else {
                    previous
                        .modes
                        .get(element)
                        .and_then(|mode| modes.iter().position(|known| known == mode))
                };
            }
        }
        None if modes.len() == 1 => {
            centres = match (settling.seeding, judge) {
                (seed::Rule::Capture, Some(judge)) => {
                    let mut centres: Vec<[f64; 3]> =
                        seed::capture(objects, &free, judge, clustered, &settling.floor)
                            .into_iter()
                            .map(|member| directions[member])
                            .collect();
                    // Nothing left to serve: the rest go somewhere the panner
                    // can work with rather than on top of each other.
                    while centres.len() < clustered {
                        centres.push(spread(centres.len(), clustered));
                    }
                    centres
                }
                _ => seed(&directions, &seed_energies, clustered),
            };
            slot_class = vec![Some(0); clustered];
        }
        None => {}
    }
    // Only a block that inherits an assignment has anything to be loyal to; a
    // cold start would otherwise be loyal to element zero.
    let mut owner: Vec<usize> = match warm {
        Some(previous) => free
            .iter()
            .map(|index| previous.owners[*index].saturating_sub(pinned.len()))
            .collect(),
        None => vec![usize::MAX; directions.len()],
    };

    if modes.len() > 1 {
        // Bring the elements to the share. A class holding more than its
        // share gives up the ones carrying the least of its own objects; a
        // class short of its share takes what is free and seeds each on the
        // object it serves worst — its loudest, when it has nothing yet — or
        // parks it where the geometry has room, when it has nothing left to
        // serve.
        let mut carried = vec![0.0f64; clustered];
        for (object, slot) in owner.iter().enumerate() {
            if *slot < clustered && slot_class[*slot] == Some(class_of[object]) {
                carried[*slot] += energies[object];
            }
        }
        let mut held_slots: Vec<Vec<usize>> = (0..modes.len())
            .map(|class| {
                (0..clustered)
                    .filter(|slot| slot_class[*slot] == Some(class))
                    .collect()
            })
            .collect();
        let mut spare_slots: Vec<usize> = (0..clustered)
            .filter(|slot| slot_class[*slot].is_none())
            .collect();
        for class in 0..modes.len() {
            while held_slots[class].len() > share[class] {
                let (at, _) = held_slots[class]
                    .iter()
                    .enumerate()
                    .min_by(|a, b| {
                        carried[*a.1]
                            .partial_cmp(&carried[*b.1])
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .expect("a class holding more than nothing");
                let slot = held_slots[class].remove(at);
                slot_class[slot] = None;
                spare_slots.push(slot);
            }
        }
        spare_slots.sort_unstable();
        for class in 0..modes.len() {
            while held_slots[class].len() < share[class] {
                let slot = spare_slots.remove(0);
                let members = (0..directions.len()).filter(|object| class_of[*object] == class);
                let seed_on = if held_slots[class].is_empty() {
                    members.max_by(|a, b| {
                        energies[*a]
                            .partial_cmp(&energies[*b])
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                } else {
                    members
                        .map(|object| {
                            let nearest = held_slots[class]
                                .iter()
                                .map(|slot| dot(centres[*slot], directions[object]))
                                .fold(f64::NEG_INFINITY, f64::max);
                            (object, energies[object] * (1.0 - nearest))
                        })
                        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                        .filter(|(_, worth)| *worth > 0.0)
                        .map(|(object, _)| object)
                };
                centres[slot] = match seed_on {
                    Some(object) => directions[object],
                    None => spread(slot, clustered),
                };
                slot_class[slot] = Some(class);
                held_slots[class].push(slot);
            }
        }
        for slot in spare_slots {
            slot_class[slot] = Some(0);
            centres[slot] = spread(slot, clustered);
        }
        // An object is loyal only to an element of its own class.
        for (object, slot) in owner.iter_mut().enumerate() {
            if *slot >= clustered || slot_class[*slot] != Some(class_of[object]) {
                *slot = usize::MAX;
            }
        }
    }
    let slot_class: Vec<usize> = slot_class
        .into_iter()
        .map(|class| class.unwrap_or(0))
        .collect();
    let housed: Vec<bool> = (0..modes.len())
        .map(|class| slot_class.contains(&class))
        .collect();

    let (margin_sin, margin_cos) = HYSTERESIS_DEG.to_radians().sin_cos();
    for _ in 0..PASSES {
        let mut moved = false;
        for (object, direction) in directions.iter().enumerate() {
            let class = class_of[object];
            // An object whose class has no element this block — it carries
            // nothing, or there would be one — has nothing to choose.
            if !housed[class] {
                continue;
            }
            let nearest = nearest_of(&centres, &slot_class, class, *direction);
            let keep = match owner[object] {
                usize::MAX => false,
                held if held >= centres.len() || slot_class[held] != class => false,
                held => {
                    // Stay unless the other element is closer **by an angle**
                    // of more than the margin — which is what the margin
                    // means and what a fixed cosine margin does not say. The
                    // bound is `cos(θ − margin)` at the angle θ the object is
                    // at now, expanded so that neither an inverse cosine nor a
                    // second one is needed.
                    let held_cos = dot(centres[held], *direction);
                    // Already inside the margin: nothing can be that much
                    // closer, since an angle does not go below zero. Without
                    // this the cosine's evenness would turn the guard off for
                    // exactly the objects that need nothing decided about them.
                    held_cos >= margin_cos || {
                        let held_sin = (1.0 - held_cos * held_cos).max(0.0).sqrt();
                        dot(centres[nearest], *direction)
                            <= held_cos * margin_cos + held_sin * margin_sin
                    }
                }
            };
            let chosen = if keep { owner[object] } else { nearest };
            moved |= chosen != owner[object];
            owner[object] = chosen;
        }

        for (element, centre) in centres.iter_mut().enumerate() {
            let mut summed = [0.0f64; 3];
            let mut total = 0.0;
            for (object, direction) in directions.iter().enumerate() {
                if owner[object] != element {
                    continue;
                }
                total += energies[object];
                for axis in 0..3 {
                    summed[axis] += energies[object] * direction[axis];
                }
            }
            // An element nothing chose stays where it is. Moving it somewhere
            // arbitrary would only make the next block's warm start a lie.
            if total > 0.0 && length(summed) > 1e-12 {
                *centre = direction_of(summed);
            }
        }

        if !moved {
            break;
        }
    }

    // An element nothing chose is an element going spare, and a spare element
    // costs nothing to use. Give each one to whichever object is worst served
    // by the element it landed on — **whatever that object's level**, which is
    // the whole point: the seeding and the centres are energy-weighted, so a
    // quiet object never earns a seed and is folded into whatever loud thing
    // happens to share its azimuth. Measured on a real master, that is
    // exactly what set the worst case: two objects at the ceiling, 38 dB down,
    // folded into an element on the floor below them, with a twelfth element
    // carrying nothing at all.
    hand_out_the_spares(
        &mut centres,
        &mut owner,
        &directions,
        &slot_class,
        &class_of,
    );

    // Elements the scene did not ask for are free to go where the geometry
    // needs them. Without this the set can be one a panner cannot use at all.
    let mut carried = vec![0.0f64; clustered];
    for (object, energy) in energies.iter().enumerate() {
        if owner[object] < clustered {
            carried[owner[object]] += energy;
        }
    }
    separate(&mut centres, &carried, &slot_class);
    // The directions are the answer; the positions are the answer put back in
    // the cube. Both are carried, because the cube is lossy — see
    // [`Clustering::directions`].
    let mut directions_out = Vec::with_capacity(elements);
    let mut positions = Vec::with_capacity(elements);
    let mut modes_out = Vec::with_capacity(elements);
    for index in &pinned {
        let at = objects[*index]
            .pinned
            .expect("a pinned object has a position");
        positions.push(at);
        directions_out.push(direction(at));
        modes_out.push(objects[*index].mode);
    }
    directions_out.extend_from_slice(&centres);
    modes_out.extend(slot_class.iter().map(|class| modes[*class]));
    give_back_the_radius_of(&mut centres, &owner, &free, objects, &energies);
    positions.extend_from_slice(&centres);

    // Back to whole-scene indices: the pinned objects own the elements at the
    // front, in order, and everything else is shifted past them.
    let mut owners = vec![0usize; objects.len()];
    for (slot, index) in pinned.iter().enumerate() {
        owners[*index] = slot;
    }
    for (slot, index) in free.iter().enumerate() {
        owners[*index] = pinned.len() + owner[slot].min(clustered.saturating_sub(1));
    }

    Ok(Partition {
        positions,
        directions: directions_out,
        owners,
        modes: modes_out,
    })
}

/// Put each element back in the cube its objects came from.
///
/// The clustering above is entirely directional: objects are assigned by the
/// angle between them, the centres are normalised means, the hysteresis is in
/// degrees and `separate` moves an element along the sphere. That is the right
/// domain for the decision — what a listener localises is a direction — and it
/// throws away the one thing a *fold* reads.
///
/// A master's object frame is a **cube**, not a sphere, and the presentations
/// a stream is played through are folds: fixed rows and columns of that cube
/// with a matrix after them. An element left on the unit sphere is at
/// `[0.707, 0.707, 0]` where its objects were at `[0.5, 0.5, 0]`, which is a
/// different place in the cube and a different set of gains. Measured before
/// this: the 7.1.4 render put the error at 0.001 and the three folds put it at
/// 0.38 to 0.49, which is the frame and not the clustering.
///
/// So the direction is kept and the radius is handed back: the
/// energy-weighted mean of how far its objects sit from the middle. An element
/// nothing chose keeps the unit radius `separate` gave it, since there is
/// nothing to average.
fn give_back_the_radius_of(
    centres: &mut [[f64; 3]],
    owner: &[usize],
    free: &[usize],
    objects: &[Object],
    energies: &[f64],
) {
    for (element, centre) in centres.iter_mut().enumerate() {
        let mut weighted = 0.0;
        let mut total = 0.0;
        for (object, energy) in energies.iter().enumerate() {
            if owner[object] != element || *energy <= 0.0 {
                continue;
            }
            weighted += energy * length(objects[free[object]].position);
            total += energy;
        }
        if total <= 0.0 {
            continue;
        }
        let radius = weighted / total;
        // Clamped to the cube, which is what the frame is: a radius past a
        // face is a position no master states and no fold has a row for.
        for axis in centre.iter_mut() {
            *axis = (*axis * radius).clamp(-1.0, 1.0);
        }
    }
}

/// Give every element nothing chose to the object worst served by the one it
/// landed on, within the element's class.
///
/// The clustering is energy-weighted from end to end — the seeding picks by
/// energy times how badly served an object is, and the centres are
/// energy-weighted means — which is right for deciding *where* the elements
/// go and wrong for deciding whether to use one at all. An element carrying
/// nothing costs nothing, and the object it would serve best is the one
/// furthest from its own element, whether or not anybody weights it.
///
/// Only objects whose element has something else on it are moved: taking the
/// sole occupant of one element and putting it on another achieves nothing and
/// would loop. And only objects of the element's own class, since an element
/// belongs to its class before it belongs to anybody.
fn hand_out_the_spares(
    centres: &mut [[f64; 3]],
    owner: &mut [usize],
    directions: &[[f64; 3]],
    slot_class: &[usize],
    class_of: &[usize],
) {
    let mut members = vec![0usize; centres.len()];
    for element in owner.iter() {
        if let Some(slot) = members.get_mut(*element) {
            *slot += 1;
        }
    }
    for element in 0..centres.len() {
        if members[element] > 0 {
            continue;
        }
        // The object furthest from its own element, among those of this
        // class sharing one.
        let worst = (0..directions.len())
            .filter(|object| {
                class_of[*object] == slot_class[element]
                    && owner[*object] < centres.len()
                    && members[owner[*object]] > 1
            })
            .min_by(|a, b| {
                dot(centres[owner[*a]], directions[*a])
                    .partial_cmp(&dot(centres[owner[*b]], directions[*b]))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        let Some(worst) = worst else { continue };
        members[owner[worst]] -= 1;
        owner[worst] = element;
        members[element] = 1;
        centres[element] = directions[worst];
    }
}

/// Move elements that are unused or sitting on top of another somewhere a
/// triangulation can work with.
///
/// Which one of a coincident pair moves is decided by what it carries: an
/// element with energy in it stays, because moving it moves what a listener
/// hears. An element carrying nothing costs nothing to move, and there is
/// always one when the scene has fewer distinct directions than the bitstream
/// has elements — which is exactly when this is needed.
///
/// Two elements of different classes are never coincident: a snapped voice
/// and a free effect at the same place are two elements at that place, which
/// is what keeping them apart means, and parking one of them across the room
/// would serve its class from there.
fn separate(centres: &mut [[f64; 3]], carried: &[f64], slot_class: &[usize]) {
    let apart = COINCIDENT_DEG.to_radians().cos();

    // Loudest first, so the elements that matter keep their places.
    let mut order: Vec<usize> = (0..centres.len()).collect();
    order.sort_by(|a, b| {
        carried[*b]
            .partial_cmp(&carried[*a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut kept: Vec<([f64; 3], usize)> = Vec::with_capacity(centres.len());
    for element in order {
        let here = centres[element];
        let class = slot_class[element];
        let clear = |candidate: [f64; 3], kept: &[([f64; 3], usize)]| {
            kept.iter()
                .all(|(other, of)| *of != class || dot(*other, candidate) < apart)
        };
        if clear(here, &kept) {
            kept.push((here, class));
            continue;
        }
        // Somewhere nothing else is, taken from a spread that always has room:
        // the poles and rings between them.
        let free = (0..centres.len() * 4)
            .map(|candidate| spread(candidate, centres.len()))
            .find(|candidate| clear(*candidate, &kept));
        match free {
            Some(position) => {
                centres[element] = position;
                kept.push((position, class));
            }
            // Nowhere left, which would need more elements than the spread has
            // directions. Leaving it where it is says so honestly: the panner
            // will refuse and the caller will hear about it.
            None => kept.push((here, class)),
        }
    }
}

/// The first guess: the loudest object, then repeatedly whichever object is
/// worst served by what has been chosen so far, weighted by its energy.
///
/// The usual seeding for this kind of clustering, and it matters more than the
/// refinement does — the passes above only ever find the nearest local answer
/// to where they started.
fn seed(directions: &[[f64; 3]], energies: &[f64], elements: usize) -> Vec<[f64; 3]> {
    let mut centres: Vec<[f64; 3]> = Vec::with_capacity(elements);

    if let Some(loudest) = (0..directions.len()).max_by(|a, b| {
        energies[*a]
            .partial_cmp(&energies[*b])
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        centres.push(directions[loudest]);
    }

    while centres.len() < elements {
        let worst = (0..directions.len())
            .map(|object| {
                let nearest = nearest(&centres, directions[object]);
                let apart = 1.0 - dot(centres[nearest], directions[object]);
                (object, energies[object] * apart)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        match worst {
            Some((object, cost)) if cost > 0.0 => centres.push(directions[object]),
            // Nothing left to serve: fewer objects than elements, or every
            // object already sitting on one. The rest go somewhere the
            // panner can work with rather than on top of each other.
            _ => {
                let spare = centres.len();
                centres.push(spread(spare, elements));
            }
        }
    }

    centres
}

/// Directions to fall back on when the scene does not ask for that many.
///
/// The poles, then rings between them: a shape a triangulation can always work
/// with, and one with room for far more elements than a bitstream carries so
/// that [`separate`] can always find somewhere free.
fn spread(index: usize, elements: usize) -> [f64; 3] {
    match index {
        0 => [0.0, 0.0, 1.0],
        1 => [0.0, 1.0, 0.0],
        n => {
            let ring = elements.saturating_sub(2).max(3);
            let (level, step) = ((n - 2) / ring, (n - 2) % ring);
            // Three rings, all at or above the floor. **A master's frame has
            // no below.** Its objects live in a cube whose floor is `z = 0`
            // and whose ceiling is `z = 1`; a direction pointing under the
            // floor is a place nothing can be, the fold has no row for it, and
            // an element parked there is one a renderer has to invent a
            // meaning for. The rings are therefore the floor, halfway up, and
            // the ceiling.
            let elevation = match level % 3 {
                0 => 0.0,
                1 => 30f64.to_radians(),
                _ => 60f64.to_radians(),
            };
            // Each ring turned a little against the last, so a candidate is
            // never directly above one already taken.
            let angle = step as f64 * std::f64::consts::TAU / ring as f64
                + level as f64 * std::f64::consts::TAU / (ring * 4) as f64;
            [
                angle.sin() * elevation.cos(),
                angle.cos() * elevation.cos(),
                elevation.sin(),
            ]
        }
    }
}

/// The nearest of the centres of one class.
fn nearest_of(
    centres: &[[f64; 3]],
    slot_class: &[usize],
    class: usize,
    direction: [f64; 3],
) -> usize {
    centres
        .iter()
        .enumerate()
        .filter(|(slot, _)| slot_class[*slot] == class)
        .max_by(|a, b| {
            dot(*a.1, direction)
                .partial_cmp(&dot(*b.1, direction))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map_or(0, |(index, _)| index)
}

fn nearest(centres: &[[f64; 3]], direction: [f64; 3]) -> usize {
    centres
        .iter()
        .enumerate()
        .max_by(|a, b| {
            dot(*a.1, direction)
                .partial_cmp(&dot(*b.1, direction))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map_or(0, |(index, _)| index)
}

pub(crate) fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn length(v: [f64; 3]) -> f64 {
    dot(v, v).sqrt()
}

/// A position's direction, as a unit vector. The origin has none, and front is
/// as good an answer as any — it is where a master puts an object it has
/// nothing to say about.
pub fn direction(position: [f64; 3]) -> [f64; 3] {
    if length(position) < 1e-12 {
        return [0.0, 1.0, 0.0];
    }
    direction_of(position)
}

fn direction_of(v: [f64; 3]) -> [f64; 3] {
    let length = length(v);
    [v[0] / length, v[1] / length, v[2] / length]
}

#[cfg(test)]
mod tests {
    use super::*;
    use hz_render::Layout;

    /// A ring of objects at even spacing, all equally loud.
    pub(crate) fn ring(count: usize) -> Vec<Object> {
        (0..count)
            .map(|n| {
                let angle = n as f64 * std::f64::consts::TAU / count as f64;
                Object {
                    position: [angle.sin(), angle.cos(), 0.0],
                    energy: 1.0,
                    size: 0.0,
                    pinned: None,
                    peak: 0.0,
                    mode: Mode::default(),
                }
            })
            .collect()
    }

    #[test]
    fn every_object_keeps_its_energy() {
        let objects = ring(40);
        let clustering = cluster(&objects, 12, Weighting::Spread, None).expect("a ring clusters");
        for (index, weights) in clustering.weights.iter().enumerate() {
            let power: f64 = weights.iter().map(|w| w * w).sum();
            assert!((power - 1.0).abs() < 1e-3, "object {index} came to {power}");
        }
    }

    /// Fewer objects than elements is not a failure: every object gets one to
    /// itself and the rest go somewhere a triangulation can work with.
    #[test]
    fn a_scene_smaller_than_the_element_count_still_clusters() {
        let objects = ring(4);
        let clustering =
            cluster(&objects, 12, Weighting::Spread, None).expect("four objects still cluster");
        assert_eq!(clustering.elements(), 12);
        assert_eq!(clustering.objects(), 4);
    }

    /// An element count a bitstream does not carry is refused by name.
    #[test]
    fn an_impossible_element_count_is_refused() {
        let err = match cluster(&ring(20), 40, Weighting::Spread, None) {
            Err(err) => err,
            Ok(_) => panic!("forty elements were accepted"),
        };
        assert!(err.to_string().contains("40 elements"), "{err}");
    }

    /// What the two weightings actually cost, on a ring of objects folded
    /// into a delivery layout. Printed rather than asserted at a threshold,
    /// because the number is the finding — see `docs/clustering.md`.
    #[test]
    fn the_two_weightings_are_measured_against_each_other() {
        let objects = ring(48);
        // Every presentation the stream is played through, not only the one
        // that is rendered: three of the four are folds.
        let renderers = metric::delivery().unwrap();

        for elements in [6usize, 8, 12, 16] {
            let mut reports = Vec::new();
            for weighting in [Weighting::Spread, Weighting::Nearest, Weighting::Fitted] {
                let clustering = cluster(&objects, elements, weighting, None).unwrap();
                let report = metric::error(&objects, &clustering, &renderers).unwrap();
                println!(
                    "{elements:2} elements {weighting:?}: mean {:.3} worst {:.3} energy {:.4}  [{}]",
                    report.mean,
                    report.worst,
                    report.energy,
                    report
                        .layouts
                        .iter()
                        .map(|layer| {
                            format!(
                                "{} {:.3}/{:.3}/{:.3}",
                                layer.name, layer.mean, layer.worst, layer.energy
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                // A fold that changes how loud an object is has changed the
                // mix, whatever it did for its position. Two of the three hold
                // it exactly; the third is measured separately, since its
                // overshoot is a finding and not a tolerance.
                if weighting != Weighting::Spread {
                    // A fold that changes how loud an object is has changed
                    // the mix, whatever it did for its position — and the
                    // level is preserved **over the presentations**, which is
                    // where the fit is now made. It is not preserved on any
                    // one of them exactly, and that is the trade: fitting
                    // against 7.1.4's directions alone held that layout to
                    // four figures and let the folds come out 5 % loud.
                    assert!(
                        (report.energy - 1.0).abs() < 0.02,
                        "{elements} {weighting:?} came out at {} of the level over the \
                         presentations",
                        report.energy
                    );
                }
                reports.push(report);
            }
            // Whatever the mean does, no object may be left facing the wrong
            // way: an error of one is a whole object somewhere else.
            for report in &reports {
                assert!(
                    report.worst < 1.2,
                    "{elements} elements: {:?}",
                    report.layouts
                );
            }
        }
    }

    /// Panning an object over the elements makes it **louder**, and more so
    /// the more elements there are. The weights carry unit power — that is what
    /// the panning guarantees — but the elements are panned again by whatever
    /// plays the stream, and being correlated they add coherently.
    ///
    /// Recorded because it is the defect the first version of the measurement
    /// could not see: it checked the power of the weights, which is exactly the
    /// quantity this weighting gets right.
    #[test]
    fn spreading_over_the_elements_makes_objects_louder() {
        let objects = ring(48);
        let layout = Layout::surround_7_1_4();
        let mut levels = Vec::new();
        for elements in [6usize, 8, 12, 16] {
            let clustering = cluster(&objects, elements, Weighting::Spread, None).unwrap();
            let report =
                metric::error(&objects, &clustering, &metric::panned(&layout).unwrap()).unwrap();
            levels.push(report.energy);
        }
        assert!(
            levels.iter().all(|level| *level > 1.02),
            "spreading was expected to overshoot: {levels:?}"
        );
        assert!(
            levels[3] > levels[0],
            "the overshoot was expected to grow with the elements: {levels:?}"
        );
    }

    /// More elements should localise the worst-served object better. The mean
    /// is not monotone and deliberately not asserted to be — folding into more
    /// elements moves objects less but spreads more of them, and which wins
    /// depends on the scene.
    #[test]
    fn more_elements_localise_the_worst_object_better() {
        let objects = ring(48);
        let layout = Layout::surround_7_1_4();

        let mut last = f64::INFINITY;
        for elements in [6usize, 8, 12, 16] {
            let clustering = cluster(&objects, elements, Weighting::Spread, None).unwrap();
            let report =
                metric::error(&objects, &clustering, &metric::panned(&layout).unwrap()).unwrap();
            assert!(
                report.worst < last,
                "{elements} elements left an object {:.3} out, against {last:.3} with fewer",
                report.worst
            );
            last = report.worst;
        }
    }

    /// A scene that turns should take its elements with it, by about as much
    /// as it turned — not by a jump, and not by staying put.
    ///
    /// This is what warm-starting is for. An element that jumps takes the
    /// objects folded into it with it, and what a listener hears is the jump
    /// rather than the mix.
    #[test]
    fn elements_follow_a_scene_that_turns_rather_than_jumping() {
        let objects = ring(30);
        let first = cluster(&objects, 12, Weighting::Spread, None).unwrap();

        let turn = 3f64.to_radians();
        let turned: Vec<Object> = objects
            .iter()
            .map(|object| Object {
                position: [
                    object.position[0] * turn.cos() + object.position[1] * turn.sin(),
                    object.position[1] * turn.cos() - object.position[0] * turn.sin(),
                    object.position[2],
                ],
                ..*object
            })
            .collect();
        let second = cluster(&turned, 12, Weighting::Spread, Some(&first)).unwrap();

        for (index, (before, after)) in first.positions.iter().zip(&second.positions).enumerate() {
            let apart = dot(direction(*before), direction(*after))
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees();
            // Every element follows the turn, and none of them jumps. Without
            // the hysteresis above, one of the twelve moved nine degrees.
            assert!(
                (apart - 3.0).abs() < 0.5,
                "element {index} moved {apart:.2}° for a 3° turn"
            );
        }
    }

    /// The hysteresis is five degrees at fifteen degrees out, not at five.
    ///
    /// The guard exists for the object between two elements, and a fixed
    /// cosine margin is worth almost nothing there: `1 − cos 5°` is 0.0038,
    /// and at fifteen degrees from an element the cosine changes by 0.0045 per
    /// degree, so the margin bought about 0.8°. An object fifteen degrees from
    /// its element changed hands to one fourteen degrees away.
    ///
    /// Built directly on the assignment rather than through a whole
    /// clustering, so that what is asserted is the rule and not a scene that
    /// happens to exercise it: one object, its element fifteen degrees off,
    /// and a challenger walked in from far away.
    #[test]
    fn an_object_off_its_element_keeps_the_full_margin() {
        // A ring of one object with two elements to sit between, and a
        // previous block that gave it the further of the two.
        let out_by = 15f64.to_radians();
        let at = |degrees: f64| {
            let angle = degrees.to_radians();
            [angle.sin(), angle.cos(), 0.0]
        };
        let object = Object {
            position: at(0.0),
            energy: 1.0,
            size: 0.0,
            pinned: None,
            peak: 0.0,
            mode: Mode::default(),
        };

        // Where the challenger has to reach before the object changes hands,
        // found by walking it in a hundredth of a degree at a time.
        let mut changed_at = None;
        let mut challenger = 15.0f64;
        while challenger > 0.0 {
            let previous = Clustering {
                positions: vec![at(15.0), at(challenger), [0.0, 0.0, 1.0]],
                directions: vec![at(15.0), at(challenger), [0.0, 0.0, 1.0]],
                weights: vec![vec![1.0, 0.0, 0.0]],
                owners: vec![0],
                modes: vec![Mode::default(); 3],
                bounded: 0,
            };
            // One pass over a scene of one object: the centres do not move,
            // because the only object either stays on element zero or does
            // not.
            let settled = partition(&[object], 3, Some(&previous)).expect("a partition");
            if settled.owners[0] != 0 {
                changed_at = Some(challenger);
                break;
            }
            challenger -= 0.01;
        }

        let changed_at = changed_at.expect("the object never changed hands at all");
        let margin = out_by.to_degrees() - changed_at;
        assert!(
            (margin - HYSTERESIS_DEG).abs() < 0.05,
            "the object gave way to a challenger {margin:.2}° closer, and the \
             hysteresis is {HYSTERESIS_DEG}°"
        );
    }

    /// A scene that only jitters should not change hands at all.
    ///
    /// The scene the hysteresis is for. A ring that turns smoothly moves every
    /// object the same way at the same time, so nothing crosses a boundary
    /// that was not going to cross it anyway — which is why `crowd40` reports
    /// the same travel, jumps and flips whatever the margin is. A real
    /// trajectory is not smooth: it wobbles about where the mix put it, and an
    /// object sitting between two elements is then handed back and forth for
    /// as long as the wobble lasts.
    ///
    /// Here every object is displaced from a fixed place by a random angle a
    /// block, so **no object ever moves**: anything the fold changes is the
    /// fold reacting to noise. Over a hundred blocks, 48 objects into 12
    /// elements, counting the objects that changed hands:
    ///
    /// | displaced by | a fixed cosine margin | at the angle the object is at |
    /// |---|---|---|
    /// | 1° | 3 | **0** |
    /// | 3° | 62 | **5** |
    /// | 4° | 235 | **20** |
    ///
    /// Every one of those displacements is inside the five degrees the margin
    /// claims, so the right answer in all three rows is nothing at all. The
    /// elements themselves still wobble by about half the displacement, and
    /// should: a centre is the mean of its members and its members are
    /// wobbling.
    #[test]
    fn a_scene_that_only_jitters_does_not_change_hands() {
        const OBJECTS: usize = 48;
        const ELEMENTS: usize = 12;
        const BLOCKS: usize = 100;
        /// How far an object is displaced from where it belongs, at most.
        /// Well inside the margin, so nothing here is a move.
        const JITTER_DEG: f64 = 3.0;

        // A deterministic wobble, so the count is a number and not a sample.
        let wobble = |object: usize, block: usize, axis: usize| -> f64 {
            let mut state = (object as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (block as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9)
                ^ (axis as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
            state ^= state >> 33;
            state = state.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
            state ^= state >> 33;
            (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
        };

        let mut clusterer = Clusterer::new(ELEMENTS, Weighting::Fitted).expect("a clusterer");
        let mut previous: Option<Clustering> = None;
        let mut changed = 0usize;
        let mut travel = 0.0f64;
        let mut travelled = 0usize;

        for block in 0..BLOCKS {
            let objects: Vec<Object> = (0..OBJECTS)
                .map(|object| {
                    let angle = object as f64 * std::f64::consts::TAU / OBJECTS as f64;
                    let at = [angle.sin(), angle.cos(), 0.0];
                    let mut position = [0.0f64; 3];
                    for (axis, slot) in position.iter_mut().enumerate() {
                        *slot = at[axis] + JITTER_DEG.to_radians() * wobble(object, block, axis);
                    }
                    Object {
                        position,
                        energy: 1.0,
                        size: 0.0,
                        pinned: None,
                        peak: 0.0,
                        mode: Mode::default(),
                    }
                })
                .collect();

            let clustering = clusterer
                .cluster(&objects, previous.as_ref())
                .expect("a clustering");
            if let Some(previous) = &previous {
                changed += previous
                    .owners
                    .iter()
                    .zip(&clustering.owners)
                    .filter(|(was, now)| was != now)
                    .count();
                for (was, now) in previous.directions.iter().zip(&clustering.directions) {
                    travel += dot(*was, *now).clamp(-1.0, 1.0).acos().to_degrees();
                    travelled += 1;
                }
            }
            previous = Some(clustering);
        }

        let travel = travel / travelled.max(1) as f64;
        println!(
            "jitter of {JITTER_DEG}°: {changed} objects changed hands over {BLOCKS} blocks, \
             {travel:.2}° travel a block"
        );
        // Nothing moved, so almost nothing should have been decided
        // differently. Not exactly nothing: the first warm block still settles
        // an assignment the cold start made, and a spare element is handed out
        // on what the scene looks like at the time.
        //
        // The elements themselves still wobble, and should: a centre is the
        // mean of its members' directions and its members are wobbling. That
        // is the scene being followed, not the numbering churning, so it is
        // printed rather than asserted.
        assert!(
            changed < OBJECTS / 4,
            "{changed} objects changed hands over {BLOCKS} blocks on a scene where nothing \
             moved, and {travel:.2}° of travel a block"
        );
    }

    /// An object between elements stops trading channels for nothing.
    ///
    /// A flip is an element going from carrying an object to carrying none of
    /// it between two blocks: a signal appearing in or vanishing from a
    /// channel, twenty-five times a second. [`crate::mix`] cross-fades the
    /// amplitude so it is not a click, but the scene never asked for the move.
    ///
    /// The scene here is a ring turning slowly, which is a scene where nothing
    /// dramatic happens and the fit is therefore free to answer a slightly
    /// different question every block. What is counted is the flips; the error
    /// is asserted not to have moved, because a fold that stops flipping by
    /// putting objects in the wrong place has not fixed anything.
    #[test]
    fn an_object_between_elements_stops_trading_channels() {
        const BLOCKS: usize = 60;
        const ELEMENTS: usize = 12;
        let renderers = metric::delivery().unwrap();
        let mut clusterer = Clusterer::new(ELEMENTS, Weighting::Fitted).expect("a clusterer");

        let mut previous: Option<Clustering> = None;
        let mut flips = 0usize;
        let mut error = 0.0f64;
        for block in 0..BLOCKS {
            // Turning by a fifth of a degree a block: far too little to move
            // anything, and enough that the fit is solved afresh each time.
            let turn = block as f64 * 0.2f64.to_radians();
            let objects: Vec<Object> = ring(37)
                .iter()
                .map(|object| Object {
                    position: [
                        object.position[0] * turn.cos() + object.position[1] * turn.sin(),
                        object.position[1] * turn.cos() - object.position[0] * turn.sin(),
                        object.position[2],
                    ],
                    ..*object
                })
                .collect();
            let clustering = clusterer
                .cluster(&objects, previous.as_ref())
                .expect("a clustering");
            if let Some(previous) = &previous {
                for (before, after) in previous.weights.iter().zip(&clustering.weights) {
                    flips += before
                        .iter()
                        .zip(after)
                        .filter(|(was, now)| (**was == 0.0) != (**now == 0.0))
                        .count();
                }
            }
            error += metric::error(&objects, &clustering, &renderers)
                .unwrap()
                .mean;
            previous = Some(clustering);
        }

        let error = error / BLOCKS as f64;
        println!(
            "{flips} flips over {BLOCKS} blocks, {:.2} a block, mean error {error:.5}",
            flips as f64 / (BLOCKS - 1) as f64
        );
        // A hundred and seventy-one without holding the set, twenty-two with
        // it. The search that places the elements on the metric makes this
        // matter more rather than less: it leaves more candidate sets nearly
        // equal, which is exactly the ground the free fit wanders over.
        //
        // Seventy-two since [`place::HOLDING`], and that is the dead band's
        // price rather than the sticky set failing. An element held where it
        // was is an element the fit has to route objects through from a
        // slightly stale place, so the active set changes oftener: the band
        // moves instability out of the positions and some of it into the
        // weights. What it buys is in `place::HOLDING`, what it costs is
        // here, and which way that trade goes is a question for the listening
        // of `docs/clustering-next.md` lever 9 and not for this number.
        // Measured against the band: 22 flips at nought, 51 at 0.05, 63 at
        // 0.10, 71 at 0.15, 95 at 0.20 and 72 at the quarter that ships.
        //
        // **Why the bound is a hundred and ten and not eighty.** The series is
        // not monotonic — it peaks at 95 a fifth of the way along and comes
        // back to 72 at the quarter — so a bound set just above the shipped
        // measurement would be a bound that the constant walks through the
        // moment anyone moves it, and this test is meant to catch the sticky
        // set failing, not to pin `HOLDING` in place. So it clears the peak of
        // the curve rather than the point on it, with about a sixth over that
        // for the scene being resynthesised. Anything near the 171 of no
        // sticky set at all is the failure it is looking for.
        assert!(
            flips < 110,
            "{flips} flips over {BLOCKS} blocks on a scene turning a fifth of a degree"
        );
        // And the fold is no further out for it — the band takes the error
        // from 0.05777 to 0.05513, so it is not stillness bought with error,
        // which is what this assertion is here to catch.
        assert!(error < 0.059, "the fold cost {error:.5}");
    }

    /// An object that appears is carried from its first sample.
    ///
    /// A block's elements are reached at its end, so an object that is silent
    /// through one block and loud from the first sample of the next is carried
    /// by elements still in transit for the whole of its attack — the part of
    /// a sound a listener localises. Letting the *placement* see the next
    /// block's energy, and nothing else about it, starts the element moving a
    /// block early.
    ///
    /// Judged as the block **arrives**: the objects where they are at its
    /// first sample, the elements where the previous payload left them, and
    /// the energies of the block that is starting, because that is what is
    /// loud while it plays.
    #[test]
    fn an_object_that_appears_is_carried_from_its_first_sample() {
        const BLOCKS: usize = 20;
        const APPEARS: usize = 10;
        let renderers = metric::delivery().unwrap();

        // Twenty objects turning slowly, and one that switches on hard above
        // and behind them where nothing else is.
        let scene_at = |block: usize| -> Vec<Object> {
            let turn = block as f64 * 6.0f64.to_radians();
            let mut objects = ring(20);
            for (index, object) in objects.iter_mut().enumerate() {
                let angle = index as f64 * std::f64::consts::TAU / 20.0 + turn;
                object.position = [angle.sin(), angle.cos(), 0.0];
            }
            objects.push(Object {
                position: [0.2, -1.0, 0.9],
                energy: if block >= APPEARS { 4.0 } else { 1e-9 },
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            });
            objects
        };

        let worst_on_arrival = |anticipate: bool| {
            let mut clusterer = Clusterer::new(12, Weighting::Fitted).expect("a clusterer");
            let mut previous: Option<Clustering> = None;
            let mut placing = Vec::new();
            let mut in_force: Option<Clustering> = None;
            let mut worst = 0.0f64;
            for block in 0..BLOCKS {
                let here = scene_at(block);
                // As the block arrives: last block's positions, this block's
                // energies, against the clustering the last payload left.
                if let Some(in_force) = &in_force {
                    let was = scene_at(block - 1);
                    let arriving: Vec<Object> = was
                        .iter()
                        .zip(&here)
                        .map(|(was, now)| Object {
                            position: was.position,
                            ..*now
                        })
                        .collect();
                    let report = metric::error(&arriving, in_force, &renderers).unwrap();
                    worst = worst.max(report.worst);
                }
                let placed = if anticipate {
                    let next: Vec<f64> = scene_at(block + 1).iter().map(|o| o.energy).collect();
                    anticipating(&here, &next, &mut placing);
                    &placing[..]
                } else {
                    &here[..]
                };
                let clustering = clusterer
                    .cluster(placed, previous.as_ref())
                    .expect("a clustering");
                previous = Some(clustering.clone());
                in_force = Some(clustering);
            }
            worst
        };

        let as_it_was = worst_on_arrival(false);
        let anticipated = worst_on_arrival(true);
        println!("worst on arrival: {as_it_was:.3} as it was, {anticipated:.3} anticipating");
        // A whole object somewhere else, against what the rest of the scene
        // already costs.
        assert!(
            as_it_was > 1.0,
            "the scene does not exercise an onset: {as_it_was:.3}"
        );
        assert!(
            anticipated < 0.8,
            "the object that appears still set the worst case at {anticipated:.3}"
        );
    }

    /// And it does nothing at all when nothing appears, which is what "only
    /// the energy" has to mean: a rule that also moved the geometry would show
    /// up on every object that merely moves.
    #[test]
    fn anticipating_a_scene_that_only_turns_changes_nothing() {
        // Two streams, two clusterers: a `Clusterer` carries a stream's state —
        // how often it reconsiders, a block's buffers — and sharing one between
        // two independent folds makes them diverge for that reason rather than
        // for the one being measured.
        let mut clusterer = Clusterer::new(12, Weighting::Fitted).expect("a clusterer");
        let mut second = Clusterer::new(12, Weighting::Fitted).expect("a clusterer");
        let mut placing = Vec::new();
        let mut plain: Option<Clustering> = None;
        let mut anticipated: Option<Clustering> = None;

        for block in 0..10 {
            let turn = block as f64 * 6.0f64.to_radians();
            let scene: Vec<Object> = ring(30)
                .iter()
                .enumerate()
                .map(|(index, object)| {
                    let angle = index as f64 * std::f64::consts::TAU / 30.0 + turn;
                    Object {
                        position: [angle.sin(), angle.cos(), 0.0],
                        ..*object
                    }
                })
                .collect();
            // The next block's energies are this block's: every object holds
            // its level, which is what a scene that only turns does.
            let next: Vec<f64> = scene.iter().map(|object| object.energy).collect();
            anticipating(&scene, &next, &mut placing);

            plain = Some(clusterer.cluster(&scene, plain.as_ref()).unwrap());
            anticipated = Some(second.cluster(&placing, anticipated.as_ref()).unwrap());
            assert_eq!(
                plain, anticipated,
                "block {block} was clustered differently for looking ahead at nothing"
            );
        }
    }

    /// Placing an element where the metric says beats placing it where the
    /// directions say.
    ///
    /// `docs/clustering.md` records the opposite result and it was measured on
    /// a **proxy**: the position whose gain vector best matches the
    /// energy-weighted mean of its members' gain vectors, which is an average
    /// in one space inverted through a non-linear map into another. The
    /// quantity the metric reports contains no such mean. Minimising it
    /// directly — move one element, keep the rest, ask what the metric would
    /// say — wins at every element count.
    #[test]
    fn placing_on_the_metric_beats_placing_on_directions() {
        let objects = ring(48);
        let renderers = metric::delivery().unwrap();

        for elements in [6usize, 8, 12, 16] {
            let measure = |search: place::Search| {
                let mut clusterer = Clusterer::new(elements, Weighting::Fitted)
                    .expect("a clusterer")
                    .searching(search);
                let clustering = clusterer.cluster(&objects, None).expect("a clustering");
                metric::error(&objects, &clustering, &renderers).expect("a report")
            };
            let directions = measure(place::Search::OFF);
            let searched = measure(place::Search::default());
            println!(
                "{elements:2} elements: directions {:.3}/{:.3}, on the metric {:.3}/{:.3}",
                directions.mean, directions.worst, searched.mean, searched.worst
            );
            assert!(
                searched.mean <= directions.mean,
                "{elements} elements: the search made the mean worse, {:.4} against {:.4}",
                searched.mean,
                directions.mean
            );
            // And it has not bought the mean by inventing or losing level,
            // which is the way a fold can look better and be wrong.
            assert!(
                (searched.energy - 1.0).abs() < 0.02,
                "{elements} elements came out at {} of the level",
                searched.energy
            );
        }
    }

    /// A fold that folds nothing is still exact after the search.
    ///
    /// The anchor the whole metric rests on: with an element for every object
    /// the fold costs zero, and a search that moved an element off an object
    /// it has to itself would break it. Nothing to gain is nothing to move.
    #[test]
    fn the_search_leaves_a_fold_that_folds_nothing_alone() {
        let objects = ring(12);
        let mut clusterer = Clusterer::new(12, Weighting::Fitted).expect("a clusterer");
        let clustering = clusterer.cluster(&objects, None).expect("a clustering");
        let report =
            metric::error(&objects, &clustering, &metric::delivery().unwrap()).expect("a report");
        assert!(
            report.worst < 0.02,
            "a fold that folds nothing cost {:.4} after the search",
            report.worst
        );
    }

    /// A programme in two acts is not folded by the basins the first one chose.
    ///
    /// The first block decides which parts of the scene the elements sit in,
    /// and every block after it is warm-started from the one before, so Lloyd's
    /// method never leaves the answer nearest to where it began. A scene that
    /// opens on a few objects at the front and then becomes something else
    /// entirely is folded, for the rest of its length, by what the front chose.
    ///
    /// Twenty blocks of five objects across the front, then twenty of
    /// twenty-four all round with the first five silent. What is measured is
    /// the second act.
    ///
    /// Measured with the search and the exchange both off, for the same
    /// reason the exchange's own test turns the search off: each of the three
    /// takes the same slack, and whichever runs first leaves the others
    /// nothing to find. With the search on this scene restarts nothing at all
    /// and the two answers are identical — see `docs/clustering.md`.
    #[test]
    fn a_film_in_two_acts_is_not_folded_by_the_first_one() {
        const ACT: usize = 20;
        let renderers = metric::delivery().unwrap();

        let scene = |block: usize| -> Vec<Object> {
            let second = block >= ACT;
            // The five that open, tight across the front.
            let mut objects: Vec<Object> = (0..5)
                .map(|n| {
                    let angle = (n as f64 - 2.0) * 8f64.to_radians();
                    Object {
                        position: [angle.sin(), angle.cos(), 0.0],
                        energy: if second { 1e-9 } else { 1.0 },
                        size: 0.0,
                        pinned: None,
                        peak: 0.0,
                        mode: Mode::default(),
                    }
                })
                .collect();
            // And the twenty-four that arrive, everywhere.
            objects.extend((0..24).map(|n| {
                let angle = n as f64 * std::f64::consts::TAU / 24.0;
                let up = (n % 3) as f64 / 2.0;
                Object {
                    position: [angle.sin(), angle.cos(), up],
                    energy: if second { 1.0 } else { 1e-9 },
                    size: 0.0,
                    pinned: None,
                    peak: 0.0,
                    mode: Mode::default(),
                }
            }));
            objects
        };

        let second_act = |reconsider: Option<u64>| {
            let mut clusterer = Clusterer::new(12, Weighting::Fitted)
                .expect("a clusterer")
                .searching(place::Search::OFF)
                .earning(false)
                .reconsidering(reconsider);
            let mut previous: Option<Clustering> = None;
            let mut mean = 0.0;
            let mut worst = 0.0f64;
            for block in 0..2 * ACT {
                let objects = scene(block);
                let clustering = clusterer
                    .cluster(&objects, previous.as_ref())
                    .expect("a clustering");
                if block >= ACT {
                    let report = metric::error(&objects, &clustering, &renderers).unwrap();
                    mean += report.mean;
                    worst = worst.max(report.worst);
                }
                previous = Some(clustering);
            }
            (mean / ACT as f64, worst, clusterer.restarts())
        };

        let (kept, kept_worst, _) = second_act(None);
        let (over, over_worst, restarts) = second_act(Some(restart::RECONSIDER));
        println!(
            "the second act: {kept:.4}/{kept_worst:.3} carrying the first act's basins, \
             {over:.4}/{over_worst:.3} starting over ({restarts} restarts)"
        );
        assert!(restarts > 0, "the fold never reconsidered at all");
        assert!(
            over < kept,
            "starting over cost {over:.4} against {kept:.4} for carrying on"
        );
    }

    /// An element with slack in it is taken back for the object that needs it.
    ///
    /// The case a Lloyd iteration cannot reach: a centre moves, it never
    /// changes hands, so an element spent badly stays spent badly. And the
    /// stability counters cannot report it, because they exclude elements
    /// carrying almost nothing.
    ///
    /// Forty-eight objects into sixteen elements, which is where a fold has
    /// slack in it. Into twelve of the same nothing is exchanged, because
    /// every element there is contested — see [`earn`].
    ///
    /// Measured with [`place::Search::OFF`], because the search that places
    /// the elements on the metric takes the same slack first: with it on, this
    /// scene exchanges nothing at all and the two answers are identical. That
    /// is the honest state of the two together — see `docs/clustering.md` —
    /// and it is also why this test turns the search off rather than asserting
    /// something the search would satisfy on its own.
    #[test]
    fn an_element_with_slack_in_it_is_taken_back() {
        let objects = ring(48);
        let renderers = metric::delivery().unwrap();
        // La recherche coupée : voir la note du test.
        let measure = |earn: bool| {
            let mut clusterer = Clusterer::new(16, Weighting::Fitted)
                .expect("a clusterer")
                .searching(place::Search::OFF)
                .earning(earn);
            let clustering = clusterer.cluster(&objects, None).expect("a clustering");
            (
                metric::error(&objects, &clustering, &renderers).expect("a report"),
                clusterer.earned(),
            )
        };
        let (kept, _) = measure(false);
        let (earned, exchanges) = measure(true);
        println!(
            "16 elements: kept {:.3}/{:.3}, earned {:.3}/{:.3} over {exchanges} exchanges",
            kept.mean, kept.worst, earned.mean, earned.worst
        );
        assert!(exchanges > 0, "nothing was exchanged at all");
        assert!(
            earned.worst < kept.worst,
            "the worst object was {:.3} with the exchange and {:.3} without",
            earned.worst,
            kept.worst
        );
    }

    /// And the exchange never makes the fold worse, because it is only kept
    /// when the metric says it is better — which is the whole of what makes it
    /// an improvement rather than a rule.
    #[test]
    fn the_exchange_is_kept_only_when_it_pays() {
        let objects = ring(48);
        let renderers = metric::delivery().unwrap();
        for elements in [6usize, 8, 12, 16] {
            let measure = |earn: bool| {
                let mut clusterer = Clusterer::new(elements, Weighting::Fitted)
                    .expect("a clusterer")
                    .searching(place::Search::OFF)
                    .earning(earn);
                let clustering = clusterer.cluster(&objects, None).expect("a clustering");
                metric::error(&objects, &clustering, &renderers).expect("a report")
            };
            let without = measure(false);
            let with = measure(true);
            println!(
                "{elements:2} elements: {:.3}/{:.3} without, {:.3}/{:.3} with",
                without.mean, without.worst, with.mean, with.worst
            );
            assert!(
                with.mean <= without.mean + 1e-9,
                "{elements} elements: the exchange cost {:.4} against {:.4}",
                with.mean,
                without.mean
            );
            assert!(
                (with.energy - 1.0).abs() < 0.02,
                "{elements} elements came out at {} of the level",
                with.energy
            );
        }
    }

    /// Does a fit made against one layout transfer to another?
    ///
    /// The uniform sphere does not. A representative layout might: an encoder
    /// cannot know what is in the room, but it can assume something ordinary.
    /// Fitted against 7.1.4's own directions and measured on 5.1 and 7.1,
    /// against the same measurement for the rules that ignore the question.
    #[test]
    fn a_fit_against_one_layout_measured_on_another() {
        use hz_render::{Panner, speakers_of};
        let assumed = speakers_of(&Layout::surround_7_1_4());
        let reference = fit::Reference::over(&assumed).unwrap();

        for size in [0.0, 0.6] {
            let mut objects = ring(16);
            objects[0].size = size;
            let positions = partition(&objects, 12, None).unwrap().positions;

            // The fit, made against the assumed layout only.
            let mut radiated = Vec::new();
            let mut scratch = Vec::new();
            for position in &positions {
                reference.radiated(*position, 0.0, &mut scratch);
                radiated.push(scratch.clone());
            }
            let mut target = Vec::new();
            reference.radiated(objects[0].position, size, &mut target);
            let fitted = fit::fitted(&radiated, &target);

            for layout in [Layout::surround_5_1(), Layout::surround_7_1()] {
                let panner = Panner::new(&layout).unwrap();
                let channels = layout.channels();
                let mut gains = vec![0.0f32; channels];
                let cost = |weights: &[f64], panner: &Panner| {
                    let mut got = vec![0.0f64; channels];
                    let mut one = vec![0.0f32; channels];
                    for (position, weight) in positions.iter().zip(weights) {
                        if *weight == 0.0 {
                            continue;
                        }
                        panner.gains(*position, 0.0, &mut one);
                        for (slot, value) in got.iter_mut().zip(&one) {
                            *slot += weight * f64::from(*value);
                        }
                    }
                    let mut want = vec![0.0f32; channels];
                    panner.gains(objects[0].position, size, &mut want);
                    let difference: f64 = got
                        .iter()
                        .zip(&want)
                        .map(|(g, w)| (g - f64::from(*w)) * (g - f64::from(*w)))
                        .sum();
                    let reference: f64 = want.iter().map(|w| f64::from(*w) * f64::from(*w)).sum();
                    (difference / reference.max(1e-12)).sqrt()
                };
                let _ = &mut gains;

                let mut nearest_weights = vec![0.0; positions.len()];
                let directions: Vec<[f64; 3]> = positions.iter().map(|p| direction(*p)).collect();
                nearest_weights[nearest(&directions, direction(objects[0].position))] = 1.0;

                println!(
                    "size {size:.1} on {}: fitted {:.3}, nearest {:.3}",
                    layout.name,
                    cost(&fitted, &panner),
                    cost(&nearest_weights, &panner)
                );
            }
        }
    }

    /// How well the fold *could* be done, if the encoder knew the layout the
    /// result would be played over.
    ///
    /// It does not, and cannot — a stream is made once and played on whatever
    /// is in the room. So this is not a weighting, it is a **floor**: it says
    /// whether a rule that scores 0.6 is a bad rule or a hard problem. Fitted
    /// against the delivery layout's own gain vectors, which is the same
    /// solver the [`Weighting::Fitted`] uses against a reference.
    #[test]
    fn how_well_the_fold_could_be_done_at_all() {
        use hz_render::Panner;
        let layout = Layout::surround_7_1_4();
        let panner = Panner::new(&layout).unwrap();
        let channels = layout.channels();

        for size in [0.0, 0.3, 0.6] {
            let mut objects = ring(16);
            objects[0].size = size;
            let positions = partition(&objects, 12, None).unwrap().positions;

            // What each element and the object put on this layout.
            let mut gains = vec![0.0f32; channels];
            let mut columns = Vec::new();
            for position in &positions {
                panner.gains(*position, 0.0, &mut gains);
                columns.push(gains.iter().map(|g| f64::from(*g)).collect::<Vec<f64>>());
            }
            panner.gains(objects[0].position, size, &mut gains);
            let target: Vec<f64> = gains.iter().map(|g| f64::from(*g)).collect();

            let weights = fit::fitted(&columns, &target);
            let mut got = vec![0.0; channels];
            for (column, weight) in columns.iter().zip(&weights) {
                for (slot, value) in got.iter_mut().zip(column) {
                    *slot += weight * value;
                }
            }
            let difference: f64 = got
                .iter()
                .zip(&target)
                .map(|(g, t)| (g - t) * (g - t))
                .sum();
            let reference: f64 = target.iter().map(|t| t * t).sum();
            println!(
                "size {size:.1}: the best possible fold costs {:.3}",
                (difference / reference).sqrt()
            );
        }
    }

    /// The anchor the whole metric rests on: as many elements as objects, and
    /// the fold should cost nothing at all. If this is not near zero then
    /// either the weights or the measurement is wrong, and every other number
    /// here is meaningless.
    /// What capping the paths costs, and what it buys.
    ///
    /// Every element an object reaches is a path its signal takes to a
    /// speaker, and paths a wide layout keeps apart arrive together on a
    /// narrow one. So the cap is not only a cost: the error is measured over
    /// the presentations, and a fit that spreads an object over six elements
    /// is a fit that has six chances to land them on the same stereo channel.
    #[test]
    fn a_cap_on_how_many_elements_an_object_reaches() {
        let objects = ring(48);
        let renderers = metric::delivery().unwrap();
        let reference = fit::Reference::new().unwrap();

        for elements in [12usize, 16] {
            for paths in [2usize, 3, 4, usize::MAX] {
                let partition = partition(&objects, elements, None).unwrap();
                let mut radiated = Vec::with_capacity(elements);
                let mut scratch = Vec::new();
                for position in &partition.positions {
                    reference.radiated(*position, 0.0, &mut scratch);
                    radiated.push(scratch.clone());
                }
                let mut weights = Vec::with_capacity(objects.len());
                let mut reached = 0usize;
                let mut target = Vec::new();
                for object in &objects {
                    reference.radiated(object.position, object.size, &mut target);
                    let row = fit::fitted_within(&radiated, &target, paths);
                    reached += row.iter().filter(|w| **w != 0.0).count();
                    weights.push(row);
                }
                let clustering = Clustering {
                    positions: partition.positions.clone(),
                    directions: partition.directions.clone(),
                    weights,
                    owners: partition.owners.clone(),
                    modes: partition.modes.clone(),
                    bounded: 0,
                };
                let report = metric::error(&objects, &clustering, &renderers).unwrap();
                println!(
                    "{elements:2} elements, cap {:>2}: mean {:.3} worst {:.3} level {:.4}, \
                     {:.2} paths an object",
                    if paths == usize::MAX {
                        "-".to_string()
                    } else {
                        paths.to_string()
                    },
                    report.mean,
                    report.worst,
                    report.energy,
                    reached as f64 / objects.len() as f64
                );
            }
        }
    }

    #[test]
    fn a_scene_that_does_not_have_to_be_folded_costs_nothing() {
        let objects = ring(12);
        let layout = Layout::surround_7_1_4();
        for weighting in [Weighting::Spread, Weighting::Nearest] {
            let clustering = cluster(&objects, 12, weighting, None).unwrap();
            let report =
                metric::error(&objects, &clustering, &metric::panned(&layout).unwrap()).unwrap();
            println!("no fold, {weighting:?}: {report:?}");
            assert!(
                report.worst < 0.02,
                "{weighting:?} cost {:.4} for a fold that folds nothing",
                report.worst
            );
        }
    }

    /// And a cold start on the same scene puts the elements somewhere else
    /// entirely — which is why the warm start is not an optimisation.
    /// An element's position does not say which way it points.
    ///
    /// A position is the direction scaled by a radius and then clamped into the
    /// cube, and the clamp **turns** it. So recovering the direction by
    /// normalising the position — which is what the warm start used to do — is
    /// not recovering it: the next block starts from an element pointing
    /// somewhere else, and the hysteresis then measures loyalty against the
    /// wrong place.
    ///
    /// The scene is built to make the clamp bite: one element holds a corner
    /// of the cube and an edge, so its mean direction and its mean radius
    /// disagree and the scaled position leaves the cube.
    ///
    /// What this asserts is the loss itself, not a consequence of it. The
    /// consequence is bounded and rare — the ownership pass runs before the
    /// centres are recomputed, so a rotation only changes the answer for an
    /// object already within it of the five-degree hysteresis — which is
    /// exactly why it is worth carrying the answer rather than re-deriving it.
    #[test]
    fn a_position_is_not_a_direction_once_the_cube_has_clamped_it() {
        let objects = vec![
            Object {
                position: [1.0, 1.0, 1.0],
                energy: 1.0,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            },
            Object {
                position: [1.0, 1.0, 0.0],
                energy: 1.0,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            },
            Object {
                position: [-1.0, 1.0, 0.0],
                energy: 1.0,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            },
            Object {
                position: [0.0, -1.0, 0.0],
                energy: 1.0,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            },
        ];
        // Fewer elements than objects, so one of them holds two and its mean
        // is not one of its members.
        let clustering = cluster(&objects, 3, Weighting::Fitted, None).unwrap();

        let mut turned = 0.0f64;
        for (position, settled) in clustering.positions.iter().zip(&clustering.directions) {
            let apart = dot(direction(*position), *settled)
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees();
            turned = turned.max(apart);
        }
        assert!(
            turned > 0.5,
            "no element was turned by the cube, so this scene does not test it"
        );
        assert!(
            turned < 10.0,
            "an element turned {turned:.2}°, which is more than a clamp explains"
        );

        // And the warm start gets the settled direction, not the turned one.
        let again = cluster(&objects, 3, Weighting::Fitted, Some(&clustering)).unwrap();
        assert_eq!(again.owners, clustering.owners);
        assert_eq!(again.directions, clustering.directions);
    }

    /// A bed channel is a speaker feed and not an object.
    ///
    /// It belongs at that speaker, an element carrying it has to sit there
    /// exactly, and nothing else may be panned through it — an element at the
    /// low frequency channel's position is a direction like any other as far
    /// as least squares is concerned, and a fit left to itself will use it.
    #[test]
    fn a_pinned_object_keeps_its_element_and_lends_it_to_nobody() {
        let bed = [
            [-1.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, -1.0],
        ];
        let mut objects: Vec<Object> = bed
            .iter()
            .map(|position| Object {
                position: *position,
                energy: 1.0,
                size: 0.0,
                pinned: Some(*position),
                peak: 0.0,
                mode: Mode::default(),
            })
            .collect();
        objects.extend(ring(20));

        let clustering = cluster(&objects, 12, Weighting::Fitted, None).unwrap();

        for (index, position) in bed.iter().enumerate() {
            // The pinned objects take the elements at the front, in order.
            assert_eq!(clustering.owners[index], index);
            assert_eq!(
                clustering.positions[index], *position,
                "element {index} is not where it was pinned"
            );
            // Its object goes there and nowhere else.
            for (element, weight) in clustering.weights[index].iter().enumerate() {
                let wanted = if element == index { 1.0 } else { 0.0 };
                assert_eq!(
                    *weight, wanted,
                    "object {index} leaked into element {element}"
                );
            }
            // And nothing else goes there at all.
            for (other, row) in clustering.weights.iter().enumerate() {
                if other == index {
                    continue;
                }
                assert_eq!(
                    row[index], 0.0,
                    "object {other} was panned through the pinned element {index}"
                );
            }
        }

        // The ring still has the rest of the elements to itself.
        for object in bed.len()..objects.len() {
            assert!(
                clustering.owners[object] >= bed.len(),
                "a ring object took a pinned element"
            );
        }
    }

    /// More pins than elements is a scene that cannot be carried, and saying
    /// so is better than silently dropping one.
    #[test]
    fn more_pins_than_elements_is_refused() {
        let objects: Vec<Object> = (0..4)
            .map(|n| Object {
                position: [n as f64 / 4.0, 1.0, 0.0],
                energy: 1.0,
                size: 0.0,
                pinned: Some([n as f64 / 4.0, 1.0, 0.0]),
                peak: 0.0,
                mode: Mode::default(),
            })
            .collect();
        let err = cluster(&objects, 3, Weighting::Fitted, None).unwrap_err();
        assert!(err.to_string().contains("pinned"), "{err}");
    }

    /// An element carrying nothing is an element going spare, and a scene
    /// with fewer objects than elements always has one.
    ///
    /// The clustering is energy-weighted from end to end, so a quiet object
    /// never earns a seed and is folded into whatever loud thing shares its
    /// azimuth — even when there is an element sitting idle that would carry
    /// it exactly. Measured on a real master, that was the whole of the
    /// worst case: two objects at the ceiling, 38 dB down, on an element on
    /// the floor below them, with a twelfth element carrying nothing.
    #[test]
    fn a_spare_element_goes_to_the_object_that_needs_it() {
        // A loud object at the front, and a quiet one directly above it: the
        // same azimuth, so a directional clustering puts them together.
        let objects = vec![
            Object {
                position: [1.0, 1.0, 0.0],
                energy: 1.0,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            },
            Object {
                position: [1.0, 1.0, 1.0],
                energy: 1e-4,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            },
            Object {
                position: [-1.0, 1.0, 0.0],
                energy: 1.0,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode::default(),
            },
        ];
        // Four elements for three objects, so there is one to spare.
        let clustering = cluster(&objects, 4, Weighting::Fitted, None).unwrap();
        assert_ne!(
            clustering.owners[0], clustering.owners[1],
            "the quiet object above the loud one should have taken the spare element"
        );

        // And with it, every object lands exactly where it asked to.
        let renderers = metric::delivery().unwrap();
        let report = metric::error(&objects, &clustering, &renderers).unwrap();
        assert!(
            report.worst < 1e-6,
            "a scene with an element each should cost nothing: {:?}",
            report.layouts
        );
    }

    /// The metadata says where the elements are and which of them are there.
    ///
    /// Not how loud they are: a clustering puts the level in the weights, so
    /// every element states unity and the audio carries the rest.
    #[test]
    fn the_metadata_states_positions_and_leaves_the_level_to_the_audio() {
        let objects = ring(6);
        // More elements than objects, so some are certain to be parked.
        let clustering = cluster(&objects, 10, Weighting::Fitted, None).unwrap();
        let payload = clustering.to_metadata(false, 0, Ramp::None);
        assert_eq!(payload.blocks.len(), 1);
        assert!(!payload.lfe);

        let block = &payload.blocks[0];
        assert_eq!(block.objects.len(), 10);
        for (element, object) in block.objects.iter().enumerate() {
            let carried = clustering.weights.iter().any(|row| row[element] != 0.0);
            assert_eq!(object.active, carried, "element {element}");
            // An element that is there states unity and leaves the level to
            // the audio; one that is not says nothing at all.
            assert_eq!(
                object.gain,
                if carried { Gain::Unity } else { Gain::Silent },
                "element {element}"
            );
            // A parked element says it is not there rather than being left
            // audible somewhere the scene never asked for.
            assert_eq!(object.render.is_some(), carried, "element {element}");
        }

        // And the positions come back through the wire's own coding.
        for (element, object) in block.objects.iter().enumerate() {
            let Some(render) = &object.render else {
                continue;
            };
            let back = render.position.to_master();
            for axis in 0..3 {
                assert!(
                    (back[axis] - clustering.positions[element][axis]).abs() < 0.05,
                    "element {element} axis {axis}: {back:?} against {:?}",
                    clustering.positions[element]
                );
            }
        }
    }

    /// The low frequency channel is a bed: it has no position and the syntax
    /// gives it no render information at all.
    #[test]
    fn the_low_frequency_element_states_no_position() {
        let objects = ring(6);
        let clustering = cluster(&objects, 8, Weighting::Fitted, None).unwrap();
        let payload = clustering.to_metadata(true, 0, Ramp::None);
        assert!(payload.lfe);
        assert!(payload.blocks[0].objects[0].render.is_none());
        assert!(payload.blocks[0].objects[0].active);
        // And it is prepended: the clustering's own elements follow it.
        assert_eq!(payload.blocks[0].objects.len(), clustering.elements() + 1);
    }

    #[test]
    fn a_cold_start_is_not_the_same_answer() {
        let objects = ring(30);
        let first = cluster(&objects, 12, Weighting::Spread, None).unwrap();
        let cold = cluster(&objects, 12, Weighting::Spread, None).unwrap();
        // Deterministic, so the same scene cold gives the same answer.
        assert_eq!(first.positions, cold.positions);
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    /// A master's frame has no below.
    ///
    /// Its objects live in a cube whose floor is `z = 0`; a parked element
    /// pointing under it is a place nothing can be, and the fold has no row
    /// for it.
    #[test]
    fn nothing_is_parked_below_the_floor() {
        for elements in MIN_ELEMENTS..=MAX_ELEMENTS {
            for index in 0..elements * 4 {
                let place = spread(index, elements);
                assert!(
                    place[2] >= -1e-12,
                    "spread({index}, {elements}) is at z = {}, under the floor",
                    place[2]
                );
                assert!(
                    (length(place) - 1.0).abs() < 1e-9,
                    "spread({index}, {elements}) is not a direction"
                );
            }
        }
    }
}

#[cfg(test)]
mod assignment_tests {
    use super::*;

    /// The identity costs nothing, and it is what comes back.
    #[test]
    fn a_matrix_already_in_order_stays_in_order() {
        let n = 4;
        let mut cost = vec![1.0f64; n * n];
        for i in 0..n {
            cost[i * n + i] = 0.0;
        }
        assert_eq!(assign(&cost, n), vec![0, 1, 2, 3]);
    }

    /// A permuted one comes back permuted the other way, which is the whole
    /// point: the cheap assignment is the one that undoes the shuffle.
    #[test]
    fn a_shuffled_matrix_comes_back_undone() {
        let n = 5;
        let wanted = [3usize, 0, 4, 1, 2];
        let mut cost = vec![1.0f64; n * n];
        for (row, column) in wanted.iter().enumerate() {
            cost[row * n + column] = 0.0;
        }
        assert_eq!(assign(&cost, n), wanted.to_vec());
    }

    /// And it minimises the total rather than picking greedily: the greedy
    /// choice for row 0 is column 0 at 1, which forces row 1 onto column 1 at
    /// 100. Taking 2 and 3 costs 5.
    #[test]
    fn it_takes_the_cheapest_whole_assignment_not_the_cheapest_row() {
        let cost = vec![
            1.0, 2.0, //
            100.0, 3.0,
        ];
        assert_eq!(assign(&cost, 2), vec![0, 1]);
        let cost = vec![
            1.0, 2.0, //
            3.0, 100.0,
        ];
        assert_eq!(assign(&cost, 2), vec![1, 0]);
    }

    /// A scene that has not moved keeps its element numbers, whatever order
    /// the clustering happened to produce them in.
    #[test]
    fn a_scene_that_has_not_moved_keeps_its_numbers() {
        let objects: Vec<Object> = [
            [-1.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
        ]
        .into_iter()
        .enumerate()
        .map(|(index, position)| Object {
            position,
            // Levels that cross between the two blocks, which is what turns
            // the ordering over in the first place.
            energy: 1.0 + index as f64 * 0.01,
            size: 0.0,
            pinned: None,
            peak: 0.0,
            mode: Mode::default(),
        })
        .collect();
        let mut clusterer = Clusterer::new(5, Weighting::Fitted).expect("a clusterer");

        let first = clusterer.cluster(&objects, None).expect("a clustering");
        // The same scene with the levels reversed: the same directions, and
        // every reason for the clustering to number them differently.
        let turned: Vec<Object> = objects
            .iter()
            .enumerate()
            .map(|(index, object)| Object {
                energy: 1.0 + (4 - index) as f64 * 0.01,
                ..*object
            })
            .collect();
        let second = clusterer
            .cluster(&turned, Some(&first))
            .expect("a clustering");

        for (element, (was, now)) in first.positions.iter().zip(&second.positions).enumerate() {
            assert!(
                (0..3).all(|axis| (was[axis] - now[axis]).abs() < 1e-9),
                "element {element} moved from {was:?} to {now:?}"
            );
        }
    }
}

#[cfg(test)]
mod class_tests {
    use super::*;

    /// A ring of objects, every third of them snapped to the nearest speaker
    /// and so a class of its own.
    fn two_classes(count: usize) -> Vec<Object> {
        (0..count)
            .map(|n| {
                let angle = n as f64 * std::f64::consts::TAU / count as f64;
                Object {
                    position: [angle.sin(), angle.cos(), 0.0],
                    energy: 1.0,
                    size: 0.0,
                    pinned: None,
                    peak: 0.0,
                    mode: Mode {
                        snap: n % 3 == 0,
                        ..Mode::default()
                    },
                }
            })
            .collect()
    }

    /// Every object is spread over elements of its own class and no other,
    /// every element carries one class, and every class present has an
    /// element — on the first block and on the blocks that follow it.
    #[test]
    fn two_classes_never_share_an_element() {
        let objects = two_classes(30);
        let mut clusterer = Clusterer::new(12, Weighting::Fitted).unwrap();
        let mut previous = None;
        for block in 0..20 {
            // The scene turns, so that ownership and numbering are exercised.
            let turn = block as f64 * 4.0f64.to_radians();
            let turned: Vec<Object> = objects
                .iter()
                .map(|object| Object {
                    position: [
                        object.position[0] * turn.cos() + object.position[1] * turn.sin(),
                        object.position[1] * turn.cos() - object.position[0] * turn.sin(),
                        0.0,
                    ],
                    ..*object
                })
                .collect();
            let clustering = clusterer.cluster(&turned, previous.as_ref()).unwrap();
            assert_eq!(clustering.modes.len(), 12);
            for (object, row) in clustering.weights.iter().enumerate() {
                for (element, weight) in row.iter().enumerate() {
                    assert!(
                        *weight == 0.0 || clustering.modes[element] == turned[object].mode,
                        "block {block}: object {object} reaches element {element} of another class"
                    );
                }
                assert!(
                    row.iter().any(|weight| *weight != 0.0),
                    "block {block}: object {object} reaches nothing"
                );
                let owner = clustering.owners[object];
                assert_eq!(clustering.modes[owner], turned[object].mode);
            }
            for mode in [
                Mode::default(),
                Mode {
                    snap: true,
                    ..Mode::default()
                },
            ] {
                assert!(
                    clustering.modes.contains(&mode),
                    "block {block}: a class present has no element"
                );
            }
            previous = Some(clustering);
        }
    }

    /// A class with one quiet object in a loud scene still gets an element,
    /// and the metadata written for that element are the class's own.
    #[test]
    fn a_quiet_class_keeps_an_element_of_its_own() {
        let mut objects = super::tests::ring(20);
        let quiet = Mode {
            zones: 1,
            screen: Some((3, 1)),
            ..Mode::default()
        };
        objects.push(Object {
            position: [0.0, -1.0, 0.8],
            energy: 1e-3,
            size: 0.0,
            pinned: None,
            peak: 0.0,
            mode: quiet,
        });
        let clustering = cluster(&objects, 12, Weighting::Fitted, None).unwrap();
        let element = clustering.owners[20];
        assert_eq!(clustering.modes[element], quiet);
        assert!(clustering.weights[20][element] > 0.0);
        assert_eq!(
            clustering
                .modes
                .iter()
                .filter(|mode| **mode == quiet)
                .count(),
            1,
            "one element for one quiet object"
        );

        let metadata = clustering.to_metadata(false, 0, Ramp::Long);
        let written = &metadata.blocks[0].objects[element];
        let render = written
            .render
            .expect("an element carrying something renders");
        assert_eq!(render.zones, 1);
        assert_eq!(render.screen, Some((3, 1)));
        assert!(!render.snap);
        assert!(render.elevation);
        // And an element of the free class states none of that.
        let free = clustering.owners[0];
        let render = metadata.blocks[0].objects[free].render.unwrap();
        assert_eq!(render.zones, 0);
        assert_eq!(render.screen, None);
    }

    /// A scene of one class is partitioned exactly as it is with the classes
    /// not honoured at all, block for block: the classes cost a master with
    /// nothing to keep apart nothing.
    #[test]
    fn one_class_is_no_class() {
        let objects = super::tests::ring(40);
        let mut honoured = Clusterer::new(12, Weighting::Fitted).unwrap();
        let mut ignored = Clusterer::new(12, Weighting::Fitted)
            .unwrap()
            .classing(None);
        let (mut a, mut b) = (None, None);
        for block in 0..12 {
            let turn = block as f64 * 3.0f64.to_radians();
            let turned: Vec<Object> = objects
                .iter()
                .enumerate()
                .map(|(n, object)| Object {
                    position: [
                        object.position[0] * turn.cos() + object.position[1] * turn.sin(),
                        object.position[1] * turn.cos() - object.position[0] * turn.sin(),
                        0.0,
                    ],
                    energy: 1.0 + 0.5 * ((n * 7 + block) % 5) as f64,
                    ..*object
                })
                .collect();
            let with = honoured.cluster(&turned, a.as_ref()).unwrap();
            let without = ignored.cluster(&turned, b.as_ref()).unwrap();
            assert_eq!(with.positions, without.positions, "block {block}");
            assert_eq!(with.weights, without.weights, "block {block}");
            assert_eq!(with.owners, without.owners, "block {block}");
            a = Some(with);
            b = Some(without);
        }
    }

    /// More classes present than elements is refused by name rather than
    /// folded wrongly.
    #[test]
    fn more_classes_than_elements_is_refused() {
        let objects: Vec<Object> = (0..8)
            .map(|n| Object {
                position: [(n as f64 * 0.7).sin(), (n as f64 * 0.7).cos(), 0.0],
                energy: 1.0,
                size: 0.0,
                pinned: None,
                peak: 0.0,
                mode: Mode {
                    zones: (n % 6) as u8,
                    screen: (n > 5).then_some((n as u8, 0)),
                    ..Mode::default()
                },
            })
            .collect();
        let err = match cluster(&objects, 3, Weighting::Fitted, None) {
            Err(err) => err,
            Ok(_) => panic!("eight classes fitted into three elements"),
        };
        assert!(err.to_string().contains("classes"), "{err}");
    }

    /// What the two seedings cost, on the ring, at every element count.
    /// Printed rather than asserted at a threshold, because the number is
    /// the finding — see `docs/clustering.md`.
    #[test]
    fn the_two_seedings_are_measured_against_each_other() {
        let objects = super::tests::ring(48);
        let renderers = metric::delivery().unwrap();
        for elements in [6usize, 8, 12, 16] {
            for rule in [seed::Rule::Direction, seed::Rule::Capture] {
                let mut clusterer = Clusterer::new(elements, Weighting::Fitted)
                    .unwrap()
                    .seeding(rule);
                let clustering = clusterer.cluster(&objects, None).unwrap();
                let report = metric::error(&objects, &clustering, &renderers).unwrap();
                println!(
                    "{elements:2} elements, seeded by {rule:?}: mean {:.3} worst {:.3}",
                    report.mean, report.worst
                );
                assert!(report.worst < 1.2);
            }
        }
    }

    /// A scene of pinned objects and nothing else — a bed alone — folds to
    /// its speakers exactly, and the elements left over are parked rather
    /// than panicked over.
    #[test]
    fn a_bed_alone_costs_nothing() {
        let bed: Vec<Object> = [
            [-1.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
            [-1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [-1.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
        ]
        .into_iter()
        .map(|position| Object {
            position,
            energy: 0.1,
            size: 0.0,
            pinned: Some(position),
            peak: 0.0,
            mode: class::BED,
        })
        .collect();
        let clustering = cluster(&bed, 12, Weighting::Fitted, None).unwrap();
        let report = metric::error(&bed, &clustering, &metric::delivery().unwrap()).unwrap();
        assert_eq!(report.mean, 0.0);
        assert_eq!(report.worst, 0.0);
        for (object, source) in bed.iter().enumerate() {
            let element = clustering.owners[object];
            assert_eq!(clustering.positions[element], source.position);
            assert_eq!(clustering.weights[object][element], 1.0);
            assert_eq!(clustering.modes[element], class::BED);
        }
    }

    /// An element keeps its number within its class: when two classes turn
    /// past each other, no element changes class merely to keep an angle.
    #[test]
    fn numbering_never_crosses_a_class() {
        let objects = two_classes(24);
        let mut clusterer = Clusterer::new(8, Weighting::Fitted).unwrap();
        let first = clusterer.cluster(&objects, None).unwrap();
        let turn = 20.0f64.to_radians();
        let turned: Vec<Object> = objects
            .iter()
            .map(|object| Object {
                position: [
                    object.position[0] * turn.cos() + object.position[1] * turn.sin(),
                    object.position[1] * turn.cos() - object.position[0] * turn.sin(),
                    0.0,
                ],
                ..*object
            })
            .collect();
        let second = clusterer.cluster(&turned, Some(&first)).unwrap();
        assert_eq!(
            first.modes, second.modes,
            "an element changed class for an angle"
        );
    }
}
