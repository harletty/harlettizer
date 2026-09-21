//! Fold a master set's objects into the elements a bitstream carries, and say
//! what it cost.
//!
//! ```text
//! cargo xtask cluster programme.atmos --elements 12
//! ```
//!
//! The number this prints is the one thing about clustering that can be argued
//! with rather than asserted: how far every object lands from where the mix put
//! it, once its energy has been shared out and the result panned onto a real
//! layout. It needs no listener and no reference engine, and it runs over a
//! whole programme from the metadata and the audio's levels alone.
//!
//! # Two rulers
//!
//! The metric weighs each object's error by the same energy the fold was
//! steered by. So a rule that changes what an object weighs changes the ruler
//! with it, and a fold that looks better under its own rule may only be
//! measuring itself more kindly. Every run here is therefore judged twice —
//! by the rule it was steered by and by the other one — and both are printed.
//! What the rule gains is what moves on the *other* ruler.

use hz_cluster::floor::Floor;
use hz_cluster::scene::{Loudness, Masker, Perception, Scene, Source};
use hz_cluster::smooth::{Rule, Smoother, Smoothing};
use hz_cluster::{Clusterer, Clustering, Object, Weighting, metric};
use hz_core::{Error, Result};
use hz_io::container::caf::CafReader;
use hz_io::master::{Event, MasterSet, screen_codes};
use hz_render::Mode;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Options {
    pub input: PathBuf,
    /// Elements to fold into. A bitstream carries twelve, fourteen or sixteen.
    pub elements: usize,
    /// Metadata blocks per second to reconsider the elements at.
    pub rate: u32,
    /// Samples a block, when the caller would rather state that than a rate —
    /// which is what comparing this against `harlettizer encode` needs, since
    /// its block is 1280 samples and no whole rate is that.
    pub block: Option<u64>,
    /// How to share an object's energy out between the elements.
    pub weighting: String,
    /// Passes of the search that places each element on the fold's own cost —
    /// `harlettizer encode --fold-search` is the same knob.
    pub search: usize,
    /// How a block's power is weighed before it steers the fold — see
    /// `hz_cluster::scene`.
    pub loudness: String,
    /// Bands of the perceptual rule.
    pub bands: usize,
    /// What masks an object under it.
    pub masker: String,
    /// How the energies the placement is steered by are read from the blocks
    /// around each one — see `hz_cluster::smooth`.
    pub smooth: String,
    pub behind: usize,
    pub ahead: usize,
    pub arriving: f64,
    /// Whether the classes are honoured — `on` or `off`, see
    /// `hz_cluster::class` — and by how much a class has to gain before an
    /// element changes class.
    pub classes: String,
    pub class_worth: f64,
    /// What to do with a bed channel other than the LFE: `pinned` to its
    /// speaker's place with an element of its own, or `free` as an object of
    /// the bed class.
    pub beds: String,
    /// Where the elements start from on a cold block: `direction` or
    /// `capture`. See `hz_cluster::seed`.
    pub seeding: String,
    /// The dead band on the placement — see `hz_cluster::place::HOLDING`.
    pub holding: f64,
    /// Whether the placement search offers each element the position of its
    /// dominant member as a candidate: `on` or `off`. See
    /// `hz_cluster::place::DOMINANT`.
    pub dominant: String,
    /// The programme's dialnorm, which sets the absolute floor — see
    /// `hz_cluster::floor` — or `relative` for the floor as it was.
    pub floor: String,
    /// Whether the fit bounds an element's coherent peak to the domain: `on`
    /// or `off`. See `hz_cluster::headroom`.
    pub headroom: String,
    /// Report every block rather than the summary.
    pub all: bool,
}

/// What a fold cost on one ruler, summed over the blocks.
struct Tally {
    mean: f64,
    worst: f64,
    energy_low: f64,
    energy_high: f64,
    per_layout: Vec<(f64, f64)>,
}

impl Tally {
    fn new(layouts: usize) -> Self {
        Self {
            mean: 0.0,
            worst: 0.0,
            energy_low: f64::INFINITY,
            energy_high: 0.0,
            per_layout: vec![(0.0, 0.0); layouts],
        }
    }

    fn add(&mut self, report: &metric::Report) {
        self.mean += report.mean;
        self.worst = self.worst.max(report.worst);
        self.energy_low = self.energy_low.min(report.energy);
        self.energy_high = self.energy_high.max(report.energy);
        for (slot, layer) in self.per_layout.iter_mut().zip(&report.layouts) {
            slot.0 += layer.mean;
            slot.1 = slot.1.max(layer.worst);
        }
    }

    fn print(&self, blocks: u64, renderers: &[Box<dyn hz_render::Renderer>]) {
        println!(
            "    {:.3} mean over the presentations, {:.3} at its worst, \
             as a fraction of the object's own gains",
            self.mean / blocks as f64,
            self.worst
        );
        for (layer, (sum, worst)) in renderers.iter().zip(&self.per_layout) {
            println!(
                "    {:<8} {:.5} mean, {:.3} at its worst",
                layer.name(),
                sum / blocks as f64,
                worst
            );
        }
        println!(
            "    energy   {:.4} to {:.4} of the scene's own",
            self.energy_low, self.energy_high
        );
    }
}

/// How a rule is named in the report.
fn describe(loudness: Loudness) -> String {
    match loudness {
        Loudness::Flat => "the plain power".to_string(),
        Loudness::KWeighted => "the K-weighted power".to_string(),
        Loudness::Perceptual(Perception { bands, masker }) => format!(
            "the perceptual importance, {bands} band{} and a {} masker",
            if bands == 1 { "" } else { "s" },
            match masker {
                Masker::Global => "global",
                Masker::Local => "local",
            }
        ),
    }
}

/// Cluster a master's objects and report what it costs.
///
/// The cost is measured on every presentation the stream will be played
/// through — 2.0, 5.1 and 7.1 as the reference *folds* them, and 7.1.4 as a
/// renderer pans it. Three of the four are folds, and a clustering judged on
/// the render alone is judged on the one presentation most listeners will not
/// hear. See `hz_cluster::metric::delivery`.
pub fn run(options: Options) -> Result<()> {
    let master = MasterSet::open(&options.input)?;
    let events = master.read_events(0)?;
    let programme = &master.config.presentations[0];
    let ids: Vec<u32> = programme.objects.iter().map(|object| object.id).collect();
    let pin_beds = match options.beds.as_str() {
        "pinned" => true,
        "free" => false,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the beds are pinned or free"),
            ));
        }
    };

    // Bed channels are speaker feeds, not objects: each is pinned to its own
    // speaker's place in the cube and takes an element of its own. The low
    // frequency channel is left out entirely — it is not a direction, and an
    // element carrying it is not something a clustering has any say about.
    //
    // The audio channel is counted along with them, because a bed channel that
    // is silent this block should pull an element no harder than a silent
    // object does — and the channel it sits in is its place in the config's
    // own order, LFE included, which is the order the master's audio uses.
    let mut bed: Vec<(usize, [f64; 3])> = Vec::new();
    // The low frequency channel is not clustered, but it does take one of the
    // elements the bitstream carries — see `clustered` below.
    let mut lfe = 0usize;
    let mut unplaced: Vec<&str> = Vec::new();
    let mut channel_index = 0usize;
    for instance in &programme.bed_instances {
        for channel in &instance.channels {
            let here = channel_index;
            channel_index += 1;
            if hz_core::speakers::is_lfe_label(&channel.channel) {
                lfe += 1;
                continue;
            }
            match hz_render::fold::bed_position(&channel.channel) {
                Some(position) => bed.push((here, position)),
                None => unplaced.push(&channel.channel),
            }
        }
    }
    if !unplaced.is_empty() {
        eprintln!(
            "note: no measured cube position for {}; those bed channels are not pinned",
            unplaced.join(", ")
        );
    }
    if ids.is_empty() && bed.is_empty() {
        return Err(Error::unsupported(
            &options.input,
            "a master with no objects and no bed has nothing to cluster",
        ));
    }

    let audio = programme_audio(&master, 0)?;
    let bed_channels = audio.channels as usize - ids.len();
    let named = programme
        .bed_instances
        .iter()
        .map(|instance| instance.channels.len())
        .sum::<usize>();
    if bed_channels > named {
        // `scBedConfiguration` is a code for a bed layout and nothing in this
        // project decodes it, so a config that names fewer channels than the
        // audio carries is a config whose bed cannot be placed. Saying so is
        // the only honest thing: guessing which speakers the unnamed channels
        // are would put a bed element at a speaker it is not.
        eprintln!(
            "note: the audio carries {bed_channels} bed channels and the config names {named}; \
             the rest are not pinned, because `scBedConfiguration` is a layout code this does \
             not decode"
        );
    }
    let sample_rate = events.sample_rate.unwrap_or(audio.rate);
    let step = options
        .block
        .filter(|block| *block > 0)
        .unwrap_or_else(|| u64::from(sample_rate / options.rate.max(1)));

    println!("{}", options.input.display());
    println!(
        "  in           {} objects, {} bed channels ({} of them placed, {}), {:.1} s",
        ids.len(),
        audio.channels - ids.len(),
        bed.len(),
        if pin_beds {
            "pinned to their speakers"
        } else {
            "free objects of the bed class"
        },
        audio.frames as f64 / f64::from(sample_rate)
    );
    let weighting = match options.weighting.as_str() {
        "fitted" => Weighting::Fitted,
        "spread" => Weighting::Spread,
        "nearest" => Weighting::Nearest,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the weightings are fitted, spread and nearest"),
            ));
        }
    };
    let masker = match options.masker.as_str() {
        "global" => Masker::Global,
        "local" => Masker::Local,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the maskers are global and local"),
            ));
        }
    };
    let perceptual = Loudness::Perceptual(Perception {
        bands: options.bands.max(1),
        masker,
    });
    // The rule the fold is steered by, and the other ruler it is judged on.
    let (steer, other) = match options.loudness.as_str() {
        "flat" => (Loudness::Flat, perceptual),
        "kweighted" | "k" => (Loudness::KWeighted, perceptual),
        "perceptual" => (perceptual, Loudness::Flat),
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the rules are flat, kweighted and perceptual"),
            ));
        }
    };
    println!(
        "  folding to   {} elements{}, {weighting:?}, reconsidered every {} samples, \
         {} passes of placement",
        options.elements,
        match lfe {
            0 => String::new(),
            1 => " — one of them the low frequency channel's, which is not clustered".to_string(),
            many => format!(" — {many} of them low frequency channels, which are not clustered"),
        },
        step,
        options.search
    );
    println!("  weighed by   {}", describe(steer));
    let rule = match options.smooth.as_str() {
        "hold" => Rule::Hold {
            arriving: options.arriving,
        },
        "mean" => Rule::Mean,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the smoothings are hold and mean"),
            ));
        }
    };
    let smoothing = Smoothing {
        behind: options.behind,
        ahead: options.ahead,
        rule,
    };
    println!(
        "  steadied by  {} over {} block{} behind and {} ahead{}",
        match rule {
            Rule::Hold { .. } => "a hold",
            Rule::Mean => "a mean",
        },
        smoothing.behind,
        if smoothing.behind == 1 { "" } else { "s" },
        smoothing.ahead,
        match rule {
            Rule::Hold { arriving } if smoothing.ahead > 0 =>
                format!(", a block ahead counting from {arriving:.2} times the energy"),
            _ => String::new(),
        }
    );
    // What is left to cluster into, once the low frequency channel has taken
    // its element.
    //
    // `--elements` is what the **bitstream** carries, which is what
    // `harlettizer encode --cluster` means, and the low frequency channel is
    // one of them: it is not a direction and the clustering has no say about
    // it, but it occupies a slot all the same. Counting it out here is the
    // only way a report from this harness is a report about that stream —
    // without it the two folded the same master one element apart, and on a
    // contested fold one element is the whole difference. See
    // `docs/clustering.md`.
    let clustered = match options.elements.checked_sub(lfe) {
        Some(left) if left > 0 => left,
        _ => {
            return Err(Error::unsupported(
                &options.input,
                format!(
                    "{} elements, {lfe} of them the low frequency channel's, and nothing left \
                     to cluster into",
                    options.elements
                ),
            ));
        }
    };
    // A pinned object takes an element outright, so the objects share what is
    // left. Fewer elements than pinned bed channels is not a fold, it is a
    // contradiction.
    if pin_beds && bed.len() >= clustered {
        return Err(Error::unsupported(
            &options.input,
            format!(
                "{} elements, {lfe} of them the low frequency channel's, for {} pinned bed \
                 channels and {} objects",
                options.elements,
                bed.len(),
                ids.len()
            ),
        ));
    }

    // The audio channel each object's signal is in. A master set's audio has
    // the bed first and the objects after it, in the order the config lists
    // them.
    let first_object = audio.channels - ids.len();

    let mut state: Vec<Track> = ids.iter().map(|id| Track::new(*id)).collect();
    let mut next_event = 0usize;
    // The last block's answer, and what each of its elements carried.
    let mut previous: Option<(Clustering, Vec<f64>)> = None;

    let mut blocks = 0u64;
    let mut travel = 0.0;
    let mut travelled = 0u64;
    let mut jumps = 0u64;
    // The ruler that has a time axis: what of the elements' movement a
    // listener would notice, and which half of it is the fold following the
    // scene. See `hz_cluster::motion`.
    let mut written: Vec<[f64; 3]> = Vec::new();
    let mut motion = hz_cluster::motion::Motion::new(
        hz_cluster::motion::HEARING,
        step.max(1) as f64 / f64::from(sample_rate),
    );

    // Split, because they scale differently: the clustering is per object and
    // the measurement is per object per speaker, and a programme is long
    // enough that the difference decides whether this can run at all.
    // Built once and held across the blocks, which is what an encoder would
    // do and what the timing below is therefore of.
    let classes = match options.classes.as_str() {
        "on" => true,
        "off" => false,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the classes are on or off"),
            ));
        }
    };
    let seeding = match options.seeding.as_str() {
        "direction" => hz_cluster::seed::Rule::Direction,
        "capture" => hz_cluster::seed::Rule::Capture,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the seedings are direction and capture"),
            ));
        }
    };
    let dominant = match options.dominant.as_str() {
        "on" => true,
        "off" => false,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the dominant candidate is on or off"),
            ));
        }
    };
    let floor = match options.floor.as_str() {
        "relative" => Floor::relative_only(),
        dialnorm => match dialnorm.parse::<f64>() {
            Ok(dialnorm) => Floor::for_dialnorm(dialnorm),
            Err(_) => {
                return Err(Error::unsupported(
                    &options.input,
                    format!("`{dialnorm}` as a floor; a dialnorm in decibels, or `relative`"),
                ));
            }
        },
    };
    println!(
        "  floor        {}, and forty decibels under the block's loudest",
        if floor.absolute > 0.0 {
            format!(
                "{:.1} dBFS at the playback level the dialnorm implies",
                floor.threshold_dbfs()
            )
        } else {
            "no absolute floor".to_string()
        }
    );
    let mut inaudible = 0u64;
    let mut silenced = 0u64;
    let bounding = match options.headroom.as_str() {
        "on" => true,
        "off" => false,
        other => {
            return Err(Error::unsupported(
                &options.input,
                format!("`{other}`; the headroom is on or off"),
            ));
        }
    };
    let mut bounded_blocks = 0u64;
    let mut bounded_elements = 0u64;
    let mut clusterer = Clusterer::new(clustered, weighting)?
        .searching(hz_cluster::place::Search {
            passes: options.search,
            dominant,
            holding: options.holding,
            ..hz_cluster::place::Search::default()
        })
        .flooring(floor)
        .bounding(bounding)
        .classing(classes.then_some(options.class_worth))
        .seeding(seeding);
    println!(
        "  held by      {}",
        if options.holding > 0.0 {
            format!(
                "a dead band: an element stays where it was unless moving pays {:.0} % of what \
                 staying costs",
                100.0 * options.holding
            )
        } else {
            "nothing: an element moves for any gain the metric sees".to_string()
        }
    );
    println!("  seeded by    {seeding:?} on a cold block");
    println!(
        "  placed with  {} dominant member as a candidate",
        if dominant { "each element's" } else { "no" }
    );
    // How far each element carrying something sits from the member that
    // carries most of it, which is what the dominant candidate is for.
    let mut dominant_apart = 0.0;
    let mut dominant_worst = 0.0f64;
    let mut dominant_counted = 0u64;
    println!(
        "  classes      {}",
        if classes {
            format!(
                "honoured, an element changing class when the taker gains {:.0} % more than the \
                 giver loses",
                options.class_worth * 100.0
            )
        } else {
            "not honoured: every object folded as one class".to_string()
        }
    );
    // What the classes are meant to prevent, counted anyway: an object
    // reaching an element of another class, and a class present in the block
    // with no element carrying it.
    let mut crossings = 0u64;
    let mut unhoused = 0u64;
    let mut classes_seen = 0usize;
    // The scene builder, held across the blocks because a weighted energy may
    // carry filter state. The same one `harlettizer encode` holds, which is
    // what makes this a report about the fold that command would run.
    let mut scene = Scene::weighed(f64::from(sample_rate), steer)?;
    let beds = Placing { pin: pin_beds };
    // The same thing walking ahead of the fold, block by block, so the
    // placement can see a sound coming: the same scene construction, so the
    // same energy, and a contiguous stream of samples on its side too. What it
    // reads goes into the smoother, which is what the placement is steered
    // by — see `hz_cluster::smooth`.
    let mut ahead = Scene::weighed(f64::from(sample_rate), steer)?;
    let mut smoother = Smoother::new(smoothing);
    let total_blocks = audio.frames.div_ceil(step.max(1));
    // The block as it arrives: the last block's scene, whose positions are
    // where the objects are at this block's first sample, and the clustering
    // the last payload left in force. See `hz_cluster::anticipating` for why
    // this is measured beside the departure.
    let mut previous_scene: Vec<Object> = Vec::new();
    let mut arriving: Vec<Object> = Vec::new();
    let mut arrival_mean = 0.0;
    let mut arrival_worst = 0.0f64;
    let mut arrivals = 0u64;
    // And the same block weighed the other way, which is the other ruler.
    let mut judge = Scene::weighed(f64::from(sample_rate), other)?;
    // Every presentation the stream will be played through, built once: three
    // folds and a render. See `metric::delivery`.
    let renderers = metric::delivery()?;
    let mut own = Tally::new(renderers.len());
    let mut theirs = Tally::new(renderers.len());
    let mut paths = 0.0f64;
    let mut worst_paths = 0usize;
    let mut flips = 0u64;
    // Elements carrying less than the audibility floor of the loudest one.
    //
    // Counted separately because every other stability figure here excludes
    // them: travel ignores an element carrying nothing and so does the flip
    // count, on the argument that relocating an empty element is the feature
    // working. That argument is right and it leaves the case unwatched — see
    // `hz_cluster::earn`.
    let mut under_floor = 0u64;
    // Objects under the same floor, which is what the fold gives up on.
    let mut objects_under = 0u64;
    let mut analysis_time = Duration::ZERO;
    let mut clustering_time = Duration::ZERO;
    // The scene as the placement sees it, reused across blocks.
    let mut placing: Vec<hz_cluster::Object> = Vec::new();
    let mut metric_time = Duration::ZERO;

    let mut at = 0u64;
    while at < audio.frames {
        // Everything the mix said up to the **end** of this block, which is
        // the state `harlettizer encode` folds by: it reads a span and holds
        // where every object was when the span ended. Taking the state at the
        // block's start instead left the two folding different scenes — most
        // visibly at the first block, where a master whose first statements
        // land a few samples in was folded with every object at the default
        // centre, and the warm start then carried a different numbering
        // through the whole programme. See `docs/clustering.md`.
        while next_event < events.events.len() {
            let event = &events.events[next_event];
            if event.sample_pos.unwrap_or(0) > at + step {
                break;
            }
            if let Some(track) = event
                .id
                .and_then(|id| state.iter_mut().find(|track| track.id == id))
            {
                track.apply(event);
            }
            next_event += 1;
        }

        // The bed first, pinned, then the objects — the same order the encoder
        // builds its scene in, so the two agree about which element is which.
        //
        // Built by `hz_cluster::scene` and not here, because the energy it
        // computes is what the whole clustering is steered by and `harlettizer
        // encode` folds by the same number. Two places computing it is two
        // scenes, and this one would then be reporting on a fold nobody ran.
        //
        // Timed, because a scene is now something that can cost: a band
        // analysis of every object is the price of the perceptual rule, and
        // an encoder pays it once ahead and once to fold.
        let started = Instant::now();
        build(
            &mut scene,
            &bed,
            beds,
            &state,
            &audio,
            first_object,
            at,
            step,
        );
        let objects = scene.finish();
        analysis_time += started.elapsed();
        build(
            &mut judge,
            &bed,
            beds,
            &state,
            &audio,
            first_object,
            at,
            step,
        );
        let judged = judge.finish();

        // The elements are placed for the energies of the window around this
        // block, so that one moving because a sound *appears* is already
        // there when it does. Only the energy looks around — the geometry is
        // this block's own, so nothing that merely moves leads the audio. See
        // `hz_cluster::smooth`. The blocks the window reaches ahead are read
        // here, in order, with the objects as they stand now.
        while smoother.recorded() <= blocks + smoothing.ahead as u64
            && smoother.recorded() < total_blocks
        {
            build(
                &mut ahead,
                &bed,
                beds,
                &state,
                &audio,
                first_object,
                smoother.recorded() * step,
                step,
            );
            let read: Vec<f64> = ahead.finish().iter().map(|object| object.energy).collect();
            smoother.record(&read);
        }
        smoother.place(objects, &mut placing);

        // As the block arrives: the objects where the last block left them,
        // carrying this block's energies, against the clustering in force.
        if let Some((in_force, _)) = &previous
            && previous_scene.len() == objects.len()
        {
            arriving.clear();
            arriving.extend(previous_scene.iter().zip(objects).map(|(was, now)| Object {
                position: was.position,
                ..*now
            }));
            let report = metric::error_with(&arriving, in_force, &renderers, &floor)?;
            arrival_mean += report.mean;
            arrival_worst = arrival_worst.max(report.worst);
            arrivals += 1;
        }

        let started = Instant::now();
        let clustering = clusterer.cluster(
            &placing,
            previous.as_ref().map(|(clustering, _)| clustering),
        )?;
        clustering_time += started.elapsed();

        if clustering.bounded > 0 {
            bounded_blocks += 1;
            bounded_elements += clustering.bounded as u64;
        }
        let started = Instant::now();
        let report = metric::error_with(objects, &clustering, &renderers, &floor)?;
        let other_report = metric::error_with(judged, &clustering, &renderers, &floor)?;
        metric_time += started.elapsed();
        inaudible += report.inaudible as u64;

        // How much energy each element ended up carrying, so that an element
        // nobody is listening to does not count as having moved. Elements the
        // scene did not ask for are relocated on purpose — see `separate` —
        // and counting those would report a feature as a fault.
        //
        // From the **weights**, not the owners: an object is spread over
        // several elements and reaches every one of them, while `owners` names
        // only the one it was counted towards when the positions settled. An
        // element that carries a third of a loud object and owns nothing is an
        // element a listener hears.
        let mut carried = vec![0.0f64; clustering.elements()];
        for (object, row) in clustering.weights.iter().enumerate() {
            for (element, weight) in row.iter().enumerate() {
                carried[element] += objects[object].energy * weight * weight;
            }
        }

        // How often an object's share of an element goes from something to
        // nothing or back. A weight that flips is a signal that appears or
        // disappears from a channel between two blocks, which is the kind of
        // instability a mean error does not show at all.
        //
        // Counted on the objects the metric counts — those within forty
        // decibels of the loudest thing in the block — and on the elements the
        // travel count uses. A near-silent object trading places with another
        // near-silent one is not instability anybody hears, and reporting it
        // beside the ones that are drowns them.
        let loudest = objects
            .iter()
            .fold(0.0f64, |m, object| m.max(object.energy.max(0.0)));
        let audible = loudest * hz_cluster::metric::AUDIBLE_FLOOR;
        objects_under += objects
            .iter()
            .filter(|object| !floor.audible(object.energy.max(0.0), loudest))
            .count() as u64;
        // What the room's floor takes that the relative one would have kept:
        // an object the block's loudest does not drown and the room does.
        silenced += objects
            .iter()
            .filter(|object| {
                let energy = object.energy.max(0.0);
                !floor.heard(energy) && energy > loudest * floor.relative
            })
            .count() as u64;
        if let Some((previous, before_carried)) = &previous
            && previous.weights.len() == clustering.weights.len()
        {
            for (object, (before, after)) in
                previous.weights.iter().zip(&clustering.weights).enumerate()
            {
                if before.len() != after.len()
                    || !floor.audible(objects[object].energy.max(0.0), loudest)
                {
                    continue;
                }
                for (element, (was, now)) in before.iter().zip(after).enumerate() {
                    let heard = before_carried.get(element).is_some_and(|e| *e > 0.0)
                        || carried.get(element).is_some_and(|e| *e > 0.0);
                    if heard && (*was == 0.0) != (*now == 0.0) {
                        flips += 1;
                    }
                }
            }
        }

        // Cross-class merges and classes without an element. Counted on the
        // objects the metric counts, since where a silent object's nothing
        // goes is not a merge anybody hears.
        let mut modes: Vec<Mode> = Vec::new();
        for (object, row) in clustering.weights.iter().enumerate() {
            let source = &objects[object];
            if source.energy <= audible || source.pinned.is_some() {
                continue;
            }
            if !modes.contains(&source.mode) {
                modes.push(source.mode);
            }
            if row
                .iter()
                .enumerate()
                .any(|(element, weight)| *weight != 0.0 && clustering.modes[element] != source.mode)
            {
                crossings += 1;
            }
        }
        classes_seen = classes_seen.max(modes.len());
        for mode in &modes {
            let housed = clustering
                .modes
                .iter()
                .enumerate()
                .any(|(element, known)| known == mode && carried[element] > 0.0);
            if !housed {
                unhoused += 1;
            }
        }

        for (element, position) in clustering.positions.iter().enumerate() {
            if carried[element] <= 0.0 {
                continue;
            }
            let dominant = (0..objects.len())
                .map(|object| {
                    let weight = clustering.weights[object][element];
                    (object, objects[object].energy.max(0.0) * weight * weight)
                })
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .filter(|(_, share)| *share > 0.0);
            if let Some((object, _)) = dominant {
                let apart = angle_between(*position, objects[object].position);
                dominant_apart += apart;
                dominant_worst = dominant_worst.max(apart);
                dominant_counted += 1;
            }
        }

        let loudest_element = carried.iter().fold(0.0f64, |m, e| m.max(*e));
        under_floor += carried
            .iter()
            .filter(|energy| **energy <= loudest_element * hz_cluster::metric::AUDIBLE_FLOOR)
            .count() as u64;

        // Through the wire's own grid, not the fold's own numbers. A
        // position is written on 62 steps an axis, which near the walls is a
        // degree and a half to two degrees of angle, and an element whose
        // centre jitters by a tenth of a degree across a code boundary is
        // written as a position that toggles between two codes on most
        // blocks. What a listener hears is what the stream carries, so that
        // is what this ruler is handed — the amplification included.
        written.clear();
        written.extend(
            clustering
                .positions
                .iter()
                .map(|at| hz_meta::oamd::Position::from_master(*at).to_master()),
        );
        motion.record(&written, &carried);

        if let Some((previous, before_carried)) = &previous {
            for (element, (before, after)) in previous
                .positions
                .iter()
                .zip(&clustering.positions)
                .enumerate()
            {
                let occupied: &f64 = &before_carried[element];
                if *occupied <= 0.0 || carried[element] <= 0.0 {
                    continue;
                }
                let moved = angle_between(*before, *after);
                travel += moved;
                travelled += 1;
                // Far enough that a listener would hear the element move
                // rather than the mix.
                if moved > 15.0 {
                    jumps += 1;
                }
            }
        }

        if options.all {
            // Which object fared worst, and what it is, since a summary
            // cannot say.
            let mut worst_object: Option<(usize, f64)> = None;
            for renderer in &renderers {
                for (object, relative) in metric::errors(objects, &clustering, renderer.as_ref())?
                    .into_iter()
                    .enumerate()
                {
                    if objects[object].energy > audible
                        && worst_object.is_none_or(|(_, so_far)| relative > so_far)
                    {
                        worst_object = Some((object, relative));
                    }
                }
            }
            let worst_object = worst_object.map_or(String::new(), |(object, relative)| {
                let mode = objects[object].mode;
                format!(
                    "object {object} ({}{}{}, size {:.1}) at {relative:.3}",
                    if mode.snap { "s" } else { "-" },
                    mode.zones,
                    if mode.screen.is_some() { "S" } else { "-" },
                    objects[object].size
                )
            });
            // The share of the elements between the classes, as the modes the
            // elements carry, so that a share changing hands can be seen.
            let mut share: Vec<(Mode, usize)> = Vec::new();
            for mode in &clustering.modes {
                match share.iter_mut().find(|(known, _)| known == mode) {
                    Some((_, count)) => *count += 1,
                    None => share.push((*mode, 1)),
                }
            }
            println!(
                "  {:>8.3}s   mean {:.3} worst {:.3}   on the other ruler {:.3} / {:.3}   worst {worst_object}   share {}   elements {}",
                at as f64 / f64::from(sample_rate),
                report.mean,
                report.worst,
                other_report.mean,
                other_report.worst,
                share
                    .iter()
                    .map(|(mode, count)| format!(
                        "{}{}{}:{count}",
                        if mode.snap { "s" } else { "-" },
                        mode.zones,
                        if mode.screen.is_some() { "S" } else { "-" }
                    ))
                    .collect::<Vec<_>>()
                    .join(" "),
                clustering
                    .positions
                    .iter()
                    .map(|p| format!("[{:.3} {:.3} {:.3}]", p[0], p[1], p[2]))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }

        blocks += 1;
        own.add(&report);
        theirs.add(&other_report);
        paths += report.paths;
        worst_paths = worst_paths.max(report.worst_paths);
        previous = Some((clustering, carried));
        previous_scene.clear();
        previous_scene.extend_from_slice(objects);
        at += step;
    }

    if blocks == 0 {
        return Err(Error::unsupported(&options.input, "nothing to cluster"));
    }

    println!();
    println!("  blocks       {blocks}");
    println!(
        "  error        judged by {}, which is what the fold was steered by",
        describe(steer)
    );
    own.print(blocks, &renderers);
    if arrivals > 0 {
        println!(
            "    arriving {:.3} mean, {:.3} at its worst, as each block arrives: the objects where \
             the last block left them, the elements where the last payload did",
            arrival_mean / arrivals as f64,
            arrival_worst
        );
    }
    println!(
        "  error        judged by {}, the other ruler",
        describe(other)
    );
    theirs.print(blocks, &renderers);
    println!(
        "  paths        {:.2} elements an object on average, {worst_paths} at the most",
        paths / blocks as f64
    );
    // What this would cost over a programme, which is the only length that
    // decides whether an encoder can do it at all.
    const FILM_SECONDS: f64 = 2.0 * 3600.0;
    let film_blocks = FILM_SECONDS * f64::from(sample_rate) / step.max(1) as f64;
    let per_block = analysis_time.as_secs_f64() / blocks as f64;
    println!(
        "  analysis     {:.1} µs a block to weigh the scene, {:.1} s for a two-hour programme, \
         which the encoder pays twice over — once ahead and once to fold",
        per_block * 1e6,
        per_block * film_blocks,
    );
    let per_block = clustering_time.as_secs_f64() / blocks as f64;
    println!(
        "  clustering   {:.1} µs a block, {:.0} blocks a second of audio, \
         {:.1} s for a two-hour programme",
        per_block * 1e6,
        1.0 / per_block,
        per_block * film_blocks,
    );
    println!(
        "  measuring    {:.1} µs a block, which is the harness and not the encoder",
        metric_time.as_secs_f64() / blocks as f64 * 1e6
    );
    if travelled > 0 {
        println!(
            "  elements     {:.2}° moved a block on average, {jumps} jumps over 15°, \
             counting only the ones carrying something",
            travel / travelled as f64
        );
    }
    println!(
        "  flips        {flips} weights appeared or vanished between blocks, {:.2} a block",
        flips as f64 / blocks as f64
    );
    if motion.windows() > 0 {
        println!(
            "  noticed      {:.2} % of the element-windows wobble past the blur — the element \
             left its place and came back inside the {:.0} ms a window spans, the ear \
             taking about {:.0} ms to hear a \
             movement — by {:.1}° past it on average and {:.1}° at worst, and {:.2} % of what \
             is playing sits in one; {:.2} % went somewhere, which is the fold following the \
             scene",
            100.0 * motion.wobbling(),
            // The window the figures are really over — a whole number of
            // blocks — and the ear's own, which it rounds to.
            1000.0 * motion.window_seconds(),
            1000.0 * hz_cluster::motion::HEARING.window,
            motion.over(),
            motion.worst(),
            100.0 * motion.wobbling_energy(),
            100.0 * motion.moving(),
        );
        println!(
            "  invented     {:.2}°/s of movement the mix never asked for, against {:.2}°/s it \
             did, weighted by what each element carries and with no threshold in either — see \
             hz_cluster::motion::invented for what a threshold could and could not say here",
            motion.invented(),
            motion.asked(),
        );
    }
    if dominant_counted > 0 {
        println!(
            "  dominant     {:.2}° from its element on average, {:.1}° at most, over the \
             elements carrying something",
            dominant_apart / dominant_counted as f64,
            dominant_worst
        );
    }
    println!(
        "  under floor  {:.2} elements a block carrying less than a ten-thousandth of the \
         loudest, which every other figure here ignores; {:.2} objects a block under either \
         floor, which the fold gives up on; {:.2} of them a block under the room's, which \
         nobody hears at all",
        under_floor as f64 / blocks as f64,
        objects_under as f64 / blocks as f64,
        inaudible as f64 / blocks as f64
    );
    println!(
        "  silenced     {:.2} objects a block the room's floor takes that the relative one \
         kept",
        silenced as f64 / blocks as f64
    );
    println!(
        "  headroom     {}: {bounded_blocks} blocks where an element's coherent peak was bounded \
         in the fit, {bounded_elements} elements in all",
        if bounding { "bounded" } else { "not bounded" }
    );
    println!(
        "  classes      {classes_seen} at the most in a block; {crossings} objects reached an \
         element of another class ({:.2} a block); {unhoused} times a class present had no \
         element carrying it ({:.2} a block)",
        crossings as f64 / blocks as f64,
        unhoused as f64 / blocks as f64
    );
    println!(
        "  exchanges    {} blocks where an element was worth more somewhere else",
        clusterer.earned()
    );
    println!(
        "  restarts     {} blocks where a cold partition was worth its own transition",
        clusterer.restarts()
    );
    Ok(())
}

/// How the bed channels go into the scene.
#[derive(Debug, Clone, Copy)]
struct Placing {
    pin: bool,
}

/// One block's scene: the bed first, pinned or free, then the objects.
#[allow(clippy::too_many_arguments)]
fn build(
    scene: &mut Scene,
    bed: &[(usize, [f64; 3])],
    beds: Placing,
    state: &[Track],
    audio: &Audio,
    first_object: usize,
    at: u64,
    step: u64,
) {
    scene.start();
    for (channel, position) in bed {
        scene.push(
            &Source {
                position: *position,
                pinned: beds.pin.then_some(*position),
                mode: hz_cluster::class::BED,
                ..Source::default()
            },
            audio.samples_of(*channel, at, step),
        );
    }
    for (index, track) in state.iter().enumerate() {
        scene.push(
            &Source {
                position: track.position,
                gain: track.gain,
                size: track.size,
                importance: track.importance,
                pinned: None,
                mode: track.mode,
            },
            audio.samples_of(first_object + index, at, step),
        );
    }
}

/// One object's state, walked forward through the delta stream.
pub(crate) struct Track {
    pub(crate) id: u32,
    pub(crate) position: [f64; 3],
    pub(crate) gain: f64,
    pub(crate) size: f64,
    /// What the master says this object is worth, 0 to 1. Absent means one:
    /// an object a mix says nothing about is an object that matters.
    pub(crate) importance: f64,
    /// How the master says it is to be rendered, in the payload's codes, and
    /// the two screen factors it is made from.
    pub(crate) mode: Mode,
    screen_factor: f64,
    depth_factor: f64,
}

impl Track {
    pub(crate) fn new(id: u32) -> Self {
        Self {
            id,
            position: [0.0, 1.0, 0.0],
            gain: 1.0,
            size: 0.0,
            importance: 1.0,
            mode: Mode::default(),
            screen_factor: 0.0,
            depth_factor: 0.25,
        }
    }

    /// An entry states what changed; what it omits stays in force.
    pub(crate) fn apply(&mut self, event: &Event) {
        if let Some(position) = &event.pos
            && position.len() == 3
        {
            self.position = [position[0].0, position[1].0, position[2].0];
        }
        if let Some(gain) = &event.gain {
            // Decibels, and `-inf` for silence.
            self.gain = if gain.0.is_finite() {
                10f64.powf(gain.0 / 20.0)
            } else {
                0.0
            };
        }
        if let Some(size) = &event.size {
            self.size = size.0;
        }
        if let Some(importance) = &event.importance {
            self.importance = importance.0.clamp(0.0, 1.0);
        }
        if let Some(snap) = event.snap {
            self.mode.snap = snap;
        }
        if let Some(elevation) = event.elevation {
            self.mode.elevation = elevation;
        }
        if let Some(zones) = event.zones {
            self.mode.zones = zones.code();
        }
        if let Some(factor) = &event.screen_factor {
            self.screen_factor = factor.0;
        }
        if let Some(factor) = &event.depth_factor {
            self.depth_factor = factor.0;
        }
        self.mode.screen = screen_codes(self.screen_factor, self.depth_factor);
        if event.active == Some(false) {
            self.gain = 0.0;
        }
    }
}

/// The audio component, held in memory so a block's levels can be read
/// straight out of it.
///
/// A programme would not fit, and this is a development harness measuring
/// scenes that do. Streaming it would mean walking the metadata twice or the
/// audio out of order, and neither buys anything here.
pub(crate) struct Audio {
    pub(crate) samples: Vec<i32>,
    pub(crate) channels: usize,
    pub(crate) frames: u64,
    pub(crate) rate: u32,
}

impl Audio {
    /// One channel's samples over one block, as linear amplitude.
    ///
    /// Handed to `hz_cluster::scene`, which is what turns them into an energy:
    /// the level is no longer computed here, because the encoder computes one
    /// too and two of them is two scenes.
    pub(crate) fn samples_of(
        &self,
        channel: usize,
        at: u64,
        frames: u64,
    ) -> impl Iterator<Item = f64> + '_ {
        let start = (at as usize * self.channels + channel).min(self.samples.len());
        let end = ((at + frames) as usize * self.channels).min(self.samples.len());
        self.samples[start..end]
            .iter()
            .step_by(self.channels)
            .map(|sample| f64::from(*sample) / f64::from(1 << 23))
    }
}

pub(crate) fn programme_audio(master: &MasterSet, presentation: usize) -> Result<Audio> {
    let path = master.components[presentation]
        .audio
        .path()
        .ok_or_else(|| Error::malformed(&master.config_path, "the audio component is not on disk"))?
        .to_path_buf();

    let mut reader = CafReader::open(&path)?;
    let format = *reader.format();
    let channels = format.channels as usize;
    let mut samples = Vec::new();
    let mut block = vec![0i32; 8192 * channels];
    loop {
        let frames = reader.read_frames(&mut block)?;
        if frames == 0 {
            break;
        }
        samples.extend_from_slice(&block[..frames * channels]);
    }

    let frames = (samples.len() / channels) as u64;
    Ok(Audio {
        samples,
        channels,
        frames,
        rate: format.sample_rate_hz(),
    })
}

fn angle_between(a: [f64; 3], b: [f64; 3]) -> f64 {
    let unit = |v: [f64; 3]| {
        let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-12);
        [v[0] / length, v[1] / length, v[2] / length]
    };
    let (a, b) = (unit(a), unit(b));
    (a[0] * b[0] + a[1] * b[1] + a[2] * b[2])
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}
