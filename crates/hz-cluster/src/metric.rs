//! What a clustering costs, in the only terms that are checkable without a
//! listener.
//!
//! Rendering is linear in the object signals, so the whole difference between
//! a scene and its clustering is a difference of *gain vectors*, per object,
//! on whatever layout the result is played over:
//!
//! ```text
//! wanted  = g(p_i)                 the object, panned
//! got     = Σ_k w_ik · g(q_k)      the elements it was folded into, panned
//! ```
//!
//! Nothing here needs the audio. That is the point: a clustering can be judged
//! from the metadata alone, over a whole programme, in the time it takes to
//! pan a few thousand positions — which is what makes it possible to measure
//! a change rather than argue about it.
//!
//! # What the numbers mean
//!
//! The error is reported as a fraction of the object's own gain vector, so 0.1
//! is "a tenth of this object landed somewhere else". It is weighted by the
//! object's energy, because an object nobody can hear landing in the wrong
//! place is not a defect anyone can hear either.
//!
//! Two objects can also *cancel*: two loud objects folded into elements either
//! side of them each spill, and the spill does not necessarily cancel. That is
//! why the error is summed per object rather than over the scene: the
//! per-object figure is the one that bounds what a listener can localise.

use crate::floor::Floor;
use crate::{Clustering, Object};
use hz_core::Result;
use hz_render::{Layout, ObjectFold, Panner, Renderer};

/// The presentations a delivery stream is actually played through.
///
/// Three of them are **folds** and one is a render, and that is not a detail:
/// a decoder stopping at the two-, six- or eight-channel presentation gets the
/// reference's matrices applied to a fixed cube interpolation, and only the
/// widest presentation is panned onto a room. A clustering measured on the
/// renderer alone is measured on the one presentation most listeners will not
/// hear.
pub fn delivery() -> Result<Vec<Box<dyn Renderer>>> {
    Ok(vec![
        Box::new(ObjectFold::for_layout(&Layout::stereo())?),
        Box::new(ObjectFold::for_layout(&Layout::surround_5_1())?),
        Box::new(ObjectFold::for_layout(&Layout::surround_7_1())?),
        Box::new(Panner::new(&Layout::surround_7_1_4())?),
    ])
}

/// The single-layout renderer a test or a comparison asks for by name.
pub fn panned(layout: &Layout) -> Result<Vec<Box<dyn Renderer>>> {
    Ok(vec![Box::new(Panner::new(layout)?)])
}

/// What a clustering costs on one presentation.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub name: String,
    pub mean: f64,
    pub worst: f64,
    pub energy: f64,
    /// How much the objects radiate on this presentation at all, as the
    /// energy-weighted mean of their own gain vectors' lengths.
    ///
    /// **The denominator of [`Layer::mean`] and [`Layer::worst`]**, which are
    /// relative errors. It is reported because without it a cost cannot be
    /// read: a presentation an object barely reaches has a small own vector,
    /// and a small absolute error over it is a large relative one. Whether
    /// that is what a given figure means is then a matter of looking rather
    /// than of guessing — and on the two positions it was guessed about, it
    /// was not: both cost most on the presentation whose own norm is largest.
    pub own: f64,
}

/// What a clustering costs, over every presentation it was measured on.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Energy-weighted mean of the per-object error, as a fraction of the
    /// object's own gain vector.
    pub mean: f64,
    /// The worst any object that carries energy fared — the bound on what a
    /// listener could localise wrongly. An object that is silent in this block
    /// is left out: where silence lands is not something anyone can hear, and
    /// counting it turns the figure into a report about the quietest thing in
    /// the scene.
    pub worst: f64,
    /// How loud each object comes out against how loud it was, as rendered,
    /// weighted by its energy. One is exact; below one is level lost, above is
    /// level invented.
    ///
    /// Not the power of the weights, which was what this measured first. That
    /// is only the same thing when the elements do not overlap on the layout —
    /// true of a weighting built from panning gains, and not true of one fitted
    /// to a rendering. Measuring the proxy made a weighting that preserves the
    /// level look as though it did not.
    pub energy: f64,
    pub objects: usize,
    /// How many objects sit under the absolute floor — inaudible at the
    /// playback level, whatever their neighbours — and so set no worst case
    /// and are not counted. See [`crate::floor`].
    pub inaudible: usize,
    /// How many elements an object reaches, on average and at its most.
    ///
    /// Every element an object is spread over is a path its signal takes to a
    /// speaker, and paths a wide layout keeps apart arrive together on a
    /// narrow one — so this is a property worth reporting beside the error
    /// rather than inferring from it. It is a property of the weights and not
    /// of a presentation, so it is here and not in a [`Layer`].
    pub paths: f64,
    pub worst_paths: usize,
    /// Per presentation, in the order they were given.
    pub layouts: Vec<Layer>,
}

/// Measure a clustering by rendering both the scene and its elements, on every
/// presentation it will be played through.
///
/// The overall figures are the mean of the presentations' means and the worst
/// of their worsts: a clustering that is exact on 7.1.4 and wrong in stereo is
/// wrong, and averaging the objects across layouts instead would let one
/// presentation cover for another.
pub fn error(
    objects: &[Object],
    clustering: &Clustering,
    renderers: &[Box<dyn Renderer>],
) -> Result<Report> {
    error_with(objects, clustering, renderers, &Floor::default())
}

/// As [`error`], with a stated floor under what counts — see
/// [`crate::floor`].
pub fn error_with(
    objects: &[Object],
    clustering: &Clustering,
    renderers: &[Box<dyn Renderer>],
    floor: &Floor,
) -> Result<Report> {
    let mut layouts = Vec::with_capacity(renderers.len());
    for renderer in renderers {
        layouts.push(one(objects, clustering, renderer.as_ref(), floor)?);
    }
    let inaudible = objects
        .iter()
        .filter(|object| !floor.heard(object.energy.max(0.0)))
        .count();
    // Coherent paths: how many elements each object actually reaches.
    let mut paths = 0usize;
    let mut worst_paths = 0usize;
    for row in &clustering.weights {
        let reached = row.iter().filter(|weight| **weight != 0.0).count();
        paths += reached;
        worst_paths = worst_paths.max(reached);
    }
    let paths = if clustering.weights.is_empty() {
        0.0
    } else {
        paths as f64 / clustering.weights.len() as f64
    };

    let count = layouts.len().max(1) as f64;
    let mean = layouts.iter().map(|layer| layer.mean).sum::<f64>() / count;
    let energy = layouts.iter().map(|layer| layer.energy).sum::<f64>() / count;
    let worst = layouts.iter().fold(0.0f64, |m, layer| m.max(layer.worst));

    Ok(Report {
        mean,
        worst,
        energy,
        objects: objects.len(),
        inaudible,
        paths,
        worst_paths,
        layouts,
    })
}

/// One presentation's figures.
fn one(
    objects: &[Object],
    clustering: &Clustering,
    renderer: &dyn Renderer,
    floor: &Floor,
) -> Result<Layer> {
    let each = errors(objects, clustering, renderer)?;
    let levels = levels(objects, clustering, renderer)?;
    let own = own_norms(objects, renderer);

    let mut weighted = 0.0;
    let mut weighted_level = 0.0;
    let mut weighted_own = 0.0;
    let mut total_energy = 0.0;
    let mut worst: f64 = 0.0;
    // What the loudest object in the scene carries, so that an object far
    // below it is not allowed to set the worst-case figure: where something
    // forty decibels down lands is not something a listener localises. And
    // the room's own floor under that — see [`crate::floor`].
    let loudest = objects
        .iter()
        .fold(0.0f64, |m, object| m.max(object.energy.max(0.0)));
    for (((object, relative), level), own) in objects.iter().zip(&each).zip(&levels).zip(&own) {
        let energy = object.energy.max(0.0);
        weighted += energy * relative;
        weighted_level += energy * level;
        weighted_own += energy * own;
        total_energy += energy;
        if floor.audible(energy, loudest) {
            worst = worst.max(*relative);
        }
    }

    Ok(Layer {
        name: renderer.name().to_string(),
        mean: if total_energy > 0.0 {
            weighted / total_energy
        } else {
            0.0
        },
        worst,
        energy: if total_energy > 0.0 {
            weighted_level / total_energy
        } else {
            1.0
        },
        own: if total_energy > 0.0 {
            weighted_own / total_energy
        } else {
            0.0
        },
    })
}

/// How much each object radiates on one presentation: the length of its own
/// gain vector there, which is what its error is measured against.
pub fn own_norms(objects: &[Object], renderer: &dyn Renderer) -> Vec<f64> {
    let mut gains = vec![0.0f64; renderer.channels()];
    objects
        .iter()
        .map(|object| {
            renderer.gains(object.position, object.size, &mut gains);
            gains.iter().map(|g| g * g).sum::<f64>().sqrt()
        })
        .collect()
}

/// How far below the loudest object something has to be before where it lands
/// stops counting towards the worst case.
///
/// Forty decibels **in power**, which is what [`Object::energy`] carries
/// under the plain rule: a gain squared times a block's power times the
/// master's importance — see [`crate::scene`] for the weighting. Getting
/// the unit wrong here is not a rounding matter — as an amplitude ratio `1e-4`
/// would be eighty decibels, a floor nothing ever reaches, and the objects
/// setting the worst case on a real master sat at −76 dB.
///
/// Under the perceptual rule the energy is a loudness, and the same fraction
/// of it means something else: forty decibels of *power* under a masker in
/// the same bands puts an object about here, because the compressive law is
/// flat up there, while forty decibels under in bands of its own leaves it
/// at a seventh of the loudest and well above it. That is the rule doing the
/// discounting the floor was written to do, and the floor is left where it
/// is — see `masking_is_by_band` in [`crate::scene`].
///
/// `worst` is the bound on what a listener could localise wrongly, and an
/// object forty decibels under the loudest thing in the block is not something
/// anyone localises at all — but it is exactly the kind of object a fold moves
/// furthest, which is why the floor has to be there and has to be in the right
/// unit.
///
/// [`Object::energy`]: crate::Object::energy
pub const AUDIBLE_FLOOR: f64 = 1e-4;

/// How loud each object comes out against how loud it was, as rendered.
fn levels(
    objects: &[Object],
    clustering: &Clustering,
    renderer: &dyn Renderer,
) -> Result<Vec<f64>> {
    let channels = renderer.channels();
    let mut element_gains = vec![vec![0.0f64; channels]; clustering.elements()];
    for (position, gains) in clustering.positions.iter().zip(&mut element_gains) {
        renderer.gains(*position, 0.0, gains);
    }

    let mut wanted = vec![0.0f64; channels];
    let mut got = vec![0.0f64; channels];
    let mut out = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        renderer.gains(object.position, object.size, &mut wanted);
        got.fill(0.0);
        for (element, weight) in clustering.weights[index].iter().enumerate() {
            if *weight == 0.0 {
                continue;
            }
            for (channel, gain) in element_gains[element].iter().enumerate() {
                got[channel] += weight * gain;
            }
        }
        let reached: f64 = got.iter().map(|v| v * v).sum::<f64>().sqrt();
        let asked: f64 = wanted.iter().map(|v| v * v).sum::<f64>().sqrt();
        out.push(if asked > 1e-12 { reached / asked } else { 1.0 });
    }
    Ok(out)
}

/// The same measurement, per object, before it is summarised.
///
/// Which object fared worst is a more useful question than how badly the scene
/// did, and a summary cannot answer it. It is also the only way to ask about
/// one object in particular — a wide one, say, whose width the fold may have
/// thrown away while everything around it stayed put.
pub fn errors(
    objects: &[Object],
    clustering: &Clustering,
    renderer: &dyn Renderer,
) -> Result<Vec<f64>> {
    let channels = renderer.channels();

    // Every element's gain vector, once: they are the same for every object.
    let mut element_gains = vec![vec![0.0f64; channels]; clustering.elements()];
    for (position, gains) in clustering.positions.iter().zip(&mut element_gains) {
        renderer.gains(*position, 0.0, gains);
    }

    let mut wanted = vec![0.0f64; channels];
    let mut got = vec![0.0f64; channels];
    let mut each = Vec::with_capacity(objects.len());

    for (index, object) in objects.iter().enumerate() {
        // The object as the mix made it, **with its size**; the elements as
        // point sources, because the output will not carry one. Panning both
        // as points would make a fold that flattens a wide object look free,
        // which is the one thing this has to be able to see. A fold ignores
        // the size on both sides, so a width is seen only on the presentation
        // that is rendered.
        renderer.gains(object.position, object.size, &mut wanted);

        got.fill(0.0);
        let weights = &clustering.weights[index];
        for (element, weight) in weights.iter().enumerate() {
            if *weight == 0.0 {
                continue;
            }
            for (channel, gain) in element_gains[element].iter().enumerate() {
                got[channel] += weight * gain;
            }
        }

        let mut difference = 0.0;
        let mut reference = 0.0;
        for (want, reached) in wanted.iter().zip(&got) {
            difference += (reached - want) * (reached - want);
            reference += want * want;
        }
        // An object that pans to silence everywhere — an LFE-only layout, say
        // — has nothing to be wrong about.
        each.push(if reference > 1e-12 {
            (difference / reference).sqrt()
        } else {
            0.0
        });
    }

    Ok(each)
}
