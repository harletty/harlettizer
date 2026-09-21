// SPDX-License-Identifier: GPL-3.0-or-later

//! Putting an object on a layout.
//!
//! This is the half of presentation rendering that a matrix cannot do. An
//! object has a position, not a channel, and turning one into the other is
//! vector-base amplitude panning over a triangulated speaker layout — real
//! geometry, with real ways to be subtly wrong.
//!
//! It is written here from the published method and from nothing else:
//! Pulkki, *Virtual sound source positioning using vector base amplitude
//! panning*, JAES 45(6), 1997, and his multiple-direction extension of 1999
//! for a source with an extent. The speaker directions are the vertices of a
//! convex hull; each face of the hull is a base of three; the base whose
//! inverse gives the source three non-negative gains is the one that
//! surrounds it; and the gains are normalised to constant power. Where a
//! layout leaves the sphere open — every one does at the nadir, and the ones
//! without height at the zenith — a virtual speaker closes it, and what a
//! source sends to a virtual speaker is spread, at equal power, over the real
//! speakers that share a hull edge with it, which is the ring nearest the
//! pole. A source at an uncovered pole therefore comes from the ring around
//! it rather than from nowhere, which is what BS.2127 asks of a renderer.
//!
//! # The three conventions
//!
//! **Positions are ADM Cartesian**, which is also what a master set carries —
//! established in `hz-io`'s projection, and the identity between them is
//! derived there rather than assumed. X is positive to the right, Y to the
//! front, Z up, and front left sits at negative X.
//!
//! **Azimuth is not.** The BS.2094 labels this engine uses count azimuth
//! anticlockwise, so front left is `M+030`, and a speaker's direction vector
//! is built with that sign: `x = sin(-azimuth)`. Get it backwards and the
//! whole programme is mirrored without anything failing; the test that checks
//! left is left is the one that guards it.
//!
//! **An LFE is not panned to.** It has no direction, VBAP needs at least three
//! that do, and an object steered into the LFE is an object that arrives an
//! octave of bandwidth short. It is excluded from the triangulation and always
//! gets zero.
//!
//! # A position is read as a direction, and a master's positions are a cube
//!
//! A position is turned into a direction and its radius thrown away. So it is
//! only ever read as the angle `atan2(x, y)`: `[-1, 1, 0]` is −45° and
//! `[-0.577, 1, 0]` is −30°, and only the second is the direction of front
//! left.
//!
//! A master's positions are not directions. They are a **cube**: `x` and `y`
//! run wall to wall, `z` floor to ceiling, and the corner `[-1, 1, 0]` is the
//! front-left *corner of the room*, which BS.2127 §7.3's allocentric panner
//! maps onto front left exactly. Reading it as a direction puts it at −45°,
//! between L at −30° and Ls at −110°, and [`Panner`] renders it as
//! `M+030 = 0.962`, `M+110 = 0.275` — L three tenths of a decibel down with
//! eleven decibels of leak behind the listener. [`corner_tests`] pins those
//! numbers so the approximation is a measured quantity rather than a surprise.
//!
//! **This is not corrected here**, for two reasons. The clustering metric in
//! `hz-cluster` is calibrated on this reading — every number in
//! `docs/clustering.md` would have to be re-taken — and the fold does read the
//! cube: [`crate::fold::ObjectFold`] puts that corner on L with a gain of one,
//! so the 2.0 / 5.1 / 7.1 layers of the metric are allocentric already and
//! only the 7.1.4 layer is not. Changing that is a decision about the metric,
//! not about the panner.
//!
//! What it costs, stated so it can be weighed: an object on a wall is
//! unaffected (`[0, 1, 0]` is `M+000 = 1.000`), an object in a corner is a
//! third of a decibel down with a quiet image behind it, and the error is
//! largest at the corners and zero on the axes.
//!
//! # Size
//!
//! An object with a size is panned as a ring of sources around its direction
//! — eight of them and the centre — at an angular radius of the size times a
//! quarter turn, their powers summed and normalised. That is the shape of
//! Pulkki's multiple-direction panning and not BS.2127's extent panner, whose
//! spreading is defined over the cube; it is here so that the metric sees a
//! wide object as wide, and it is the only place size reaches a presentation
//! at all, since a fold ignores it.

use crate::layout::Layout;
use hz_core::{Error, Result, speakers};
use std::f64::consts::{FRAC_PI_2, TAU};
use std::path::Path;

/// The most directions a hull holds, real and virtual together; the scratch a
/// panning is computed in is this wide, on the stack.
const MAX_DIRECTIONS: usize = 64;

/// How close a real direction has to be to a pole, as a cosine, for the pole
/// to need no virtual speaker: ten degrees.
const POLE_COS: f64 = 0.984_807_753;

/// Points closer to a face's plane than this are on it.
const PLANE_EPS: f64 = 1e-6;

/// Sources on the ring a size is panned as, the centre not counted.
const RING: usize = 8;

/// Pans objects onto one layout.
pub struct Panner {
    layout: Layout,
    hull: Hull,
    /// For each direction of the hull's real ones, its index in the layout.
    /// The LFE is not in here, which is what keeps it out of the
    /// triangulation.
    panned_to_layout: Vec<usize>,
}

impl Panner {
    /// Build a panner for `layout`.
    ///
    /// Fails rather than guesses if a speaker's direction is unknown: a
    /// triangulation built around a speaker put in the wrong place is wrong
    /// everywhere near it, and nothing downstream would say so.
    pub fn new(layout: &Layout) -> Result<Self> {
        let path = Path::new(layout.name);
        let mut directions = Vec::with_capacity(layout.channels());
        let mut panned_to_layout = Vec::with_capacity(layout.channels());

        for (index, label) in layout.speakers.iter().enumerate() {
            if speakers::is_lfe_label(label) {
                continue;
            }
            let (azimuth, elevation) = speakers::direction_of(label).ok_or_else(|| {
                Error::unsupported(path, format!("no direction known for speaker `{label}`"))
            })?;
            directions.push(direction_vector(azimuth, elevation));
            panned_to_layout.push(index);
        }

        let hull = Hull::new(&directions, path)?;
        Ok(Self {
            layout: layout.clone(),
            hull,
            panned_to_layout,
        })
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Gains for an object at an ADM Cartesian position, one per layout
    /// channel, in layout order.
    ///
    /// The position is read as a **direction** — see the module header, which
    /// says what that costs at the corners of a master's cube.
    ///
    /// `gains` is filled, not appended to, and nothing is allocated: this is
    /// called once per object per metadata update over a whole programme.
    pub fn gains(&self, position: [f64; 3], spread: f64, gains: &mut [f32]) {
        gains.fill(0.0);
        let mut power = [0.0f64; MAX_DIRECTIONS];
        self.hull.pan(position, spread, &mut power);
        for (slot, &channel) in self.panned_to_layout.iter().enumerate() {
            if let Some(out) = gains.get_mut(channel) {
                *out = power[slot].sqrt() as f32;
            }
        }
    }

    /// The same gains in double precision.
    ///
    /// For the measurement side, which compares a panner against a fold and
    /// wants one number type rather than a conversion at every use. Nothing is
    /// allocated here either.
    pub fn gains_f64(&self, position: [f64; 3], spread: f64, gains: &mut [f64]) {
        gains.fill(0.0);
        let mut power = [0.0f64; MAX_DIRECTIONS];
        self.hull.pan(position, spread, &mut power);
        for (slot, &channel) in self.panned_to_layout.iter().enumerate() {
            if let Some(out) = gains.get_mut(channel) {
                *out = power[slot].sqrt();
            }
        }
    }
}

impl crate::Renderer for Panner {
    fn channels(&self) -> usize {
        self.layout.channels()
    }

    fn gains(&self, position: [f64; 3], size: f64, out: &mut [f64]) {
        self.gains_f64(position, size, out);
    }

    fn name(&self) -> &str {
        self.layout.name
    }
}

/// A layout's speakers as Cartesian directions, the ones with no direction of
/// their own left out.
///
/// For treating a layout as a *set of points* — fitting against it, say —
/// rather than as a set of channels.
pub fn speakers_of(layout: &Layout) -> Vec<[f64; 3]> {
    layout
        .speakers
        .iter()
        .filter(|label| !speakers::is_lfe_label(label))
        .filter_map(|label| speakers::direction_of(label))
        .map(|(azimuth, elevation)| direction_vector(azimuth, elevation))
        .collect()
}

/// A unit vector in the object frame from a label's azimuth and elevation in
/// degrees, the azimuth counted anticlockwise as the labels do.
fn direction_vector(azimuth: f64, elevation: f64) -> [f64; 3] {
    let (azimuth, elevation) = ((-azimuth).to_radians(), elevation.to_radians());
    [
        azimuth.sin() * elevation.cos(),
        azimuth.cos() * elevation.cos(),
        elevation.sin(),
    ]
}

/// Pans onto a set of arbitrary points rather than onto a named layout.
///
/// A cluster is not a speaker: it has a position and no label, and where it
/// sits changes from one block to the next. Sharing the layout panner is not
/// possible for that reason, and sharing the *geometry* is the whole point —
/// the alternative is a second, unexercised triangulation.
///
/// The property this exists for: distributing an object over a set of points
/// with these gains preserves its energy and its energy-weighted direction.
/// That is what makes a cluster stand in for the objects folded into it, and
/// it is a property of vector-base amplitude panning rather than something
/// chosen here.
pub struct PointPanner {
    hull: Hull,
    points: usize,
}

impl PointPanner {
    /// Build a panner over `points`, in the same ADM Cartesian frame objects
    /// use.
    ///
    /// Fails rather than guesses when the points cannot carry a panner: fewer
    /// than three, more than the hull holds, or a set that does not surround
    /// the listener. A clusterer that silently fell back to something else
    /// would be one whose output nobody could account for.
    pub fn new(points: &[[f64; 3]]) -> Result<Self> {
        let path = Path::new("cluster positions");
        let directions: Vec<[f64; 3]> = points.iter().map(|point| direction_of(*point)).collect();
        let hull = Hull::new(&directions, path)?;
        Ok(Self {
            hull,
            points: points.len(),
        })
    }

    pub fn points(&self) -> usize {
        self.points
    }

    /// Gains for an object at `position`, one per point, in the order given.
    pub fn gains(&self, position: [f64; 3], spread: f64, gains: &mut [f32]) {
        gains.fill(0.0);
        let mut power = [0.0f64; MAX_DIRECTIONS];
        self.hull.pan(position, spread, &mut power);
        for (out, power) in gains.iter_mut().zip(&power[..self.points]) {
            *out = power.sqrt() as f32;
        }
    }
}

/// The direction of a position: the unit vector along it, and straight ahead
/// for a position with no length to have a direction.
fn direction_of(position: [f64; 3]) -> [f64; 3] {
    let length = norm(position);
    if length < 1e-12 {
        [0.0, 1.0, 0.0]
    } else {
        scale(position, 1.0 / length)
    }
}

/// The triangulated hull of a set of directions, with the virtual poles that
/// close it and the inverse of every face's base, computed once.
struct Hull {
    /// Unit vectors, the real ones first and the virtual poles after. Only
    /// the test that checks the hull is a hull reads them back; panning
    /// needs the inverse bases and nothing else.
    #[cfg(test)]
    directions: Vec<[f64; 3]>,
    /// How many directions are real.
    real: usize,
    triangles: Vec<Triangle>,
    /// For each virtual direction, in order after `real`, the real ones it
    /// shares a hull edge with: the ring nearest that pole.
    spread_to: Vec<Vec<usize>>,
}

/// One face of the hull: its corners, and the rows of the inverse of the
/// matrix whose columns are their vectors, so that a corner's gain for a
/// source is one dot product.
#[derive(Clone, Copy)]
struct Triangle {
    corners: [usize; 3],
    inverse: [[f64; 3]; 3],
}

impl Hull {
    fn new(real: &[[f64; 3]], what: &Path) -> Result<Self> {
        if real.len() < 3 {
            return Err(Error::unsupported(
                what,
                format!(
                    "panning needs at least three directions; this has {}",
                    real.len()
                ),
            ));
        }
        if real.len() + 2 > MAX_DIRECTIONS {
            return Err(Error::unsupported(
                what,
                format!(
                    "panning over {} directions; the hull holds {}",
                    real.len(),
                    MAX_DIRECTIONS - 2
                ),
            ));
        }

        let mut directions: Vec<[f64; 3]> = real.iter().map(|d| direction_of(*d)).collect();
        for pole in [[0.0, 0.0, 1.0], [0.0, 0.0, -1.0]] {
            if !directions[..real.len()]
                .iter()
                .any(|d| dot(*d, pole) > POLE_COS)
            {
                directions.push(pole);
            }
        }

        let triangles = triangulate(&directions).ok_or_else(|| {
            Error::unsupported(what, "these directions do not surround the listener")
        })?;

        let mut spread_to = vec![Vec::new(); directions.len() - real.len()];
        for triangle in &triangles {
            for &corner in &triangle.corners {
                if corner < real.len() {
                    continue;
                }
                let ring = &mut spread_to[corner - real.len()];
                for &other in &triangle.corners {
                    if other < real.len() && !ring.contains(&other) {
                        ring.push(other);
                    }
                }
            }
        }

        Ok(Self {
            #[cfg(test)]
            directions,
            real: real.len(),
            triangles,
            spread_to,
        })
    }

    /// The power each real direction receives for an object at `position`
    /// with `spread`, normalised to a total of one. `power` must be at least
    /// `real` long; only that much of it is written.
    fn pan(&self, position: [f64; 3], spread: f64, power: &mut [f64]) {
        let power = &mut power[..self.real];
        power.fill(0.0);
        let source = direction_of(position);
        let spread = spread.clamp(0.0, 1.0);

        let mut scratch = [0.0f64; MAX_DIRECTIONS];
        self.add_source(source, power, &mut scratch);
        if spread > 0.0 {
            let radius = spread * FRAC_PI_2;
            let (u, v) = basis_around(source);
            for step in 0..RING {
                let angle = step as f64 * TAU / RING as f64;
                let around = add(
                    scale(u, angle.cos() * radius.sin()),
                    scale(v, angle.sin() * radius.sin()),
                );
                let direction = add(scale(source, radius.cos()), around);
                self.add_source(direction, power, &mut scratch);
            }
        }

        let total: f64 = power.iter().sum();
        if total > 0.0 {
            for p in power.iter_mut() {
                *p /= total;
            }
        }
    }

    /// One source, at unit power, added to `power`.
    fn add_source(&self, direction: [f64; 3], power: &mut [f64], scratch: &mut [f64]) {
        let scratch = &mut scratch[..self.real];
        scratch.fill(0.0);
        self.power_of(direction, scratch);
        let total: f64 = scratch.iter().sum();
        if total > 0.0 {
            for (out, p) in power.iter_mut().zip(scratch.iter()) {
                *out += p / total;
            }
        }
    }

    /// The unnormalised power of one source over the real directions: the
    /// gains of the base that surrounds it, squared, with what fell on a
    /// virtual pole spread over the ring nearest it.
    fn power_of(&self, direction: [f64; 3], power: &mut [f64]) {
        // The base that surrounds the source is the one whose gains are all
        // non-negative; the one with the largest smallest gain is that one
        // when it exists and the least wrong one when the hull has a gap.
        let mut best: Option<(f64, [f64; 3], [usize; 3])> = None;
        for triangle in &self.triangles {
            let gains = [
                dot(triangle.inverse[0], direction),
                dot(triangle.inverse[1], direction),
                dot(triangle.inverse[2], direction),
            ];
            let least = gains[0].min(gains[1]).min(gains[2]);
            if best.is_none_or(|(so_far, _, _)| least > so_far) {
                best = Some((least, gains, triangle.corners));
            }
        }
        let Some((_, gains, corners)) = best else {
            return;
        };

        for (gain, corner) in gains.into_iter().zip(corners) {
            let p = gain.max(0.0) * gain.max(0.0);
            if corner < self.real {
                power[corner] += p;
            } else {
                let ring = &self.spread_to[corner - self.real];
                let share = p / ring.len() as f64;
                for &real in ring {
                    power[real] += share;
                }
            }
        }
    }
}

/// The faces of the convex hull of `points`, as triangles with their inverse
/// bases, or `None` when the points do not surround the origin.
///
/// Brute force over every triple: a plane through three points is a face
/// when every point lies on the origin's side of it. A face with more than
/// three points on it — a ring of heights at one elevation, the trapezoid
/// between a front pair and the heights above them — is one polygon and is
/// fanned into triangles once, rather than being found once per triple and
/// covered twice.
fn triangulate(points: &[[f64; 3]]) -> Option<Vec<Triangle>> {
    let n = points.len();
    let mut planes: Vec<([f64; 3], f64)> = Vec::new();
    for i in 0..n {
        for j in i + 1..n {
            for k in j + 1..n {
                let a = points[i];
                let mut normal = cross(sub(points[j], a), sub(points[k], a));
                let length = norm(normal);
                if length < 1e-9 {
                    continue;
                }
                normal = scale(normal, 1.0 / length);
                let mut offset = dot(normal, a);
                if offset < 0.0 {
                    normal = scale(normal, -1.0);
                    offset = -offset;
                }
                // A plane through the listener is not a face of a hull that
                // surrounds them.
                if offset < PLANE_EPS {
                    continue;
                }
                if !points.iter().all(|p| dot(normal, *p) <= offset + PLANE_EPS) {
                    continue;
                }
                if !planes
                    .iter()
                    .any(|(seen, _)| dot(*seen, normal) > 1.0 - PLANE_EPS)
                {
                    planes.push((normal, offset));
                }
            }
        }
    }
    if planes.is_empty() {
        return None;
    }

    let mut triangles = Vec::new();
    for (normal, offset) in planes {
        let mut on: Vec<usize> = (0..n)
            .filter(|&m| (dot(normal, points[m]) - offset).abs() < PLANE_EPS)
            .collect();
        if on.len() < 3 {
            continue;
        }
        // Around the face's centre, so the fan is a fan and not a bow tie.
        let mut centre = [0.0; 3];
        for &m in &on {
            centre = add(centre, points[m]);
        }
        centre = scale(centre, 1.0 / on.len() as f64);
        let u = direction_of(sub(points[on[0]], centre));
        let v = cross(normal, u);
        let angle = |m: usize| {
            let from = sub(points[m], centre);
            dot(from, v).atan2(dot(from, u))
        };
        on.sort_by(|&a, &b| angle(a).total_cmp(&angle(b)));
        for w in 1..on.len() - 1 {
            if let Some(triangle) = Triangle::new(points, [on[0], on[w], on[w + 1]]) {
                triangles.push(triangle);
            }
        }
    }
    (!triangles.is_empty()).then_some(triangles)
}

impl Triangle {
    /// The face over these corners, or `None` if their vectors do not span.
    fn new(points: &[[f64; 3]], corners: [usize; 3]) -> Option<Self> {
        let [a, b, c] = [points[corners[0]], points[corners[1]], points[corners[2]]];
        // Cramer's rule over the matrix whose columns are the corners: the
        // rows of its inverse are the cross products of the other two columns
        // over the determinant.
        let determinant = dot(a, cross(b, c));
        if determinant.abs() < 1e-9 {
            return None;
        }
        let inverse = [
            scale(cross(b, c), 1.0 / determinant),
            scale(cross(c, a), 1.0 / determinant),
            scale(cross(a, b), 1.0 / determinant),
        ];
        Some(Self { corners, inverse })
    }
}

/// Two unit vectors at right angles to `direction` and to each other.
fn basis_around(direction: [f64; 3]) -> ([f64; 3], [f64; 3]) {
    let helper = if direction[2].abs() < 0.9 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let u = direction_of(cross(helper, direction));
    let v = cross(direction, u);
    (u, v)
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f64; 3], by: f64) -> [f64; 3] {
    [a[0] * by, a[1] * by, a[2] * by]
}

fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panning onto a set of points preserves the object's energy and its
    /// energy-weighted direction. Those two are the whole reason a cluster can
    /// stand in for the objects folded into it, so they are asserted rather
    /// than assumed.
    #[test]
    fn spreading_an_object_over_points_keeps_its_energy_and_its_direction() {
        // Eight points around the room and one overhead, which is a plausible
        // shape for a small cluster set and not a degenerate one.
        let mut points: Vec<[f64; 3]> = (0..8)
            .map(|n| {
                let angle = f64::from(n) * TAU / 8.0;
                [angle.sin(), angle.cos(), 0.0]
            })
            .collect();
        points.push([0.0, 0.0, 1.0]);

        let panner = PointPanner::new(&points).expect("nine points carry a panner");
        assert_eq!(panner.points(), 9);

        let mut gains = vec![0.0f32; points.len()];
        for object in [
            [0.0, 1.0, 0.0],
            [-0.7, 0.7, 0.0],
            [0.3, -0.9, 0.2],
            [0.0, 0.2, 0.9],
        ] {
            panner.gains(object, 0.0, &mut gains);

            let power: f64 = gains.iter().map(|g| f64::from(*g) * f64::from(*g)).sum();
            assert!((power - 1.0).abs() < 1e-3, "{object:?} came to {power}");

            // The energy-weighted sum of the point directions should point
            // where the object does.
            let mut summed = [0.0f64; 3];
            for (point, gain) in points.iter().zip(&gains) {
                let weight = f64::from(*gain) * f64::from(*gain);
                let length = norm(*point).max(1e-12);
                for axis in 0..3 {
                    summed[axis] += weight * point[axis] / length;
                }
            }
            let wanted = direction_of(object);
            let summed = direction_of(summed);
            let cosine = dot(summed, wanted);
            assert!(
                cosine > 0.98,
                "{object:?}: the energy landed {:.1}° away",
                cosine.clamp(-1.0, 1.0).acos().to_degrees()
            );
        }
    }

    /// Fewer than three points cannot carry a panner, and saying so beats
    /// falling back to something nobody could account for.
    #[test]
    fn too_few_points_are_refused() {
        let err = match PointPanner::new(&[[0.0, 1.0, 0.0], [1.0, 0.0, 0.0]]) {
            Err(err) => err,
            Ok(_) => panic!("two points were accepted for panning"),
        };
        assert!(err.to_string().contains("at least three"), "{err}");
    }

    fn gains_at(panner: &Panner, position: [f64; 3]) -> Vec<f32> {
        let mut gains = vec![0.0f32; panner.layout().channels()];
        panner.gains(position, 0.0, &mut gains);
        gains
    }

    fn power(gains: &[f32]) -> f64 {
        gains.iter().map(|g| f64::from(*g) * f64::from(*g)).sum()
    }

    #[test]
    fn a_layout_without_enough_directions_is_refused() {
        let err = match Panner::new(&Layout::stereo()) {
            Err(e) => e,
            Ok(_) => panic!("a two-speaker layout was accepted for panning"),
        };
        assert!(err.to_string().contains("at least three"), "{err}");
    }

    /// An object sitting on a speaker should be that speaker and nothing else.
    /// If the direction table and the panner disagree about where a speaker
    /// is, this is where it shows.
    #[test]
    fn an_object_at_a_speaker_lands_on_that_speaker() {
        let layout = Layout::surround_7_1_4();
        let panner = Panner::new(&layout).unwrap();

        // Front left. ADM Cartesian has X to the right and Y to the front, so
        // a speaker 30° to the left sits at negative X.
        let angle = 30f64.to_radians();
        let gains = gains_at(&panner, [-angle.sin(), angle.cos(), 0.0]);

        let left = layout.index_of("M+030").unwrap();
        assert!(gains[left] > 0.95, "front left got {}", gains[left]);
        for (channel, gain) in gains.iter().enumerate() {
            if channel != left {
                assert!(*gain < 0.1, "channel {channel} leaked {gain}");
            }
        }
    }

    /// Between two speakers, both should sound, and the pair should carry the
    /// same power one of them alone would.
    #[test]
    fn an_object_between_speakers_splits_at_constant_power() {
        let layout = Layout::surround_7_1_4();
        let panner = Panner::new(&layout).unwrap();

        // Straight ahead, between front left and front right.
        let gains = gains_at(&panner, [0.0, 1.0, 0.0]);
        let centre = layout.index_of("M+000").unwrap();
        assert!(gains[centre] > 0.5, "centre got {}", gains[centre]);
        assert!(
            (power(&gains) - 1.0).abs() < 0.05,
            "power {}",
            power(&gains)
        );

        // And half way to the left surround, which no single speaker covers.
        let angle = 60f64.to_radians();
        let gains = gains_at(&panner, [-angle.sin(), angle.cos(), 0.0]);
        let sounding = gains.iter().filter(|g| **g > 0.01).count();
        assert!(sounding >= 2, "only {sounding} speakers sounding");
        assert!(
            (power(&gains) - 1.0).abs() < 0.05,
            "power {}",
            power(&gains)
        );
    }

    /// Overhead is what a 7.1.4 layout exists for.
    #[test]
    fn an_object_overhead_reaches_the_height_layer() {
        let layout = Layout::surround_7_1_4();
        let panner = Panner::new(&layout).unwrap();
        let gains = gains_at(&panner, [0.0, 0.0, 1.0]);

        let heights: f64 = ["U+030", "U-030", "U+135", "U-135"]
            .iter()
            .filter_map(|label| layout.index_of(label))
            .map(|channel| f64::from(gains[channel]))
            .sum();
        assert!(heights > 0.9, "heights got {heights}");
        assert!(
            (power(&gains) - 1.0).abs() < 0.05,
            "power {}",
            power(&gains)
        );
    }

    /// The zenith of a 7.1.4 is not a speaker, so it is a virtual one, and
    /// what lands on it is shared at equal power over the ring nearest it —
    /// the four heights, each at half amplitude, and nothing below them.
    #[test]
    fn what_lands_on_a_virtual_pole_is_shared_over_the_ring_nearest_it() {
        let layout = Layout::surround_7_1_4();
        let panner = Panner::new(&layout).unwrap();

        let gains = gains_at(&panner, [0.0, 0.0, 1.0]);
        for label in ["U+030", "U-030", "U+135", "U-135"] {
            let gain = gains[layout.index_of(label).unwrap()];
            assert!((f64::from(gain) - 0.5).abs() < 1e-5, "{label} got {gain}");
        }
        let below: f64 = gains
            .iter()
            .enumerate()
            .filter(|(channel, _)| !layout.speakers[*channel].starts_with('U'))
            .map(|(_, gain)| f64::from(*gain))
            .sum();
        assert!(below < 1e-5, "the floor got {below}");

        // And the nadir, which no layout covers, comes from the whole main
        // ring: seven speakers at equal power.
        let gains = gains_at(&panner, [0.0, 0.0, -1.0]);
        let ring: Vec<f32> = layout
            .speakers
            .iter()
            .zip(&gains)
            .filter(|(label, _)| label.starts_with('M'))
            .map(|(_, gain)| *gain)
            .collect();
        assert_eq!(ring.len(), 7);
        for gain in &ring {
            assert!(
                (f64::from(*gain) - (1.0f64 / 7.0).sqrt()).abs() < 1e-5,
                "ring got {gain}"
            );
        }
    }

    /// The LFE is not a direction and must never receive a panned object.
    #[test]
    fn nothing_is_ever_panned_into_the_lfe() {
        let layout = Layout::surround_7_1_4();
        let panner = Panner::new(&layout).unwrap();
        let lfe = layout.index_of("LFE1").unwrap();

        for position in [
            [0.0, 1.0, 0.0],
            [0.0, -1.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
            [0.7, -0.7, 0.3],
        ] {
            let gains = gains_at(&panner, position);
            assert_eq!(gains[lfe], 0.0, "LFE got signal at {position:?}");
        }
    }

    /// Every direction has to produce sound somewhere. A hole in the sphere is
    /// an object that vanishes when it passes over it.
    #[test]
    fn no_direction_falls_through_the_layout() {
        for layout in [
            Layout::surround_5_1(),
            Layout::surround_7_1(),
            Layout::surround_7_1_4(),
        ] {
            let panner = Panner::new(&layout).unwrap();
            for azimuth in (0..360).step_by(15) {
                for elevation in [-60, -30, 0, 30, 60, 89] {
                    let (a, e) = (
                        f64::from(azimuth).to_radians(),
                        f64::from(elevation).to_radians(),
                    );
                    let position = [-a.sin() * e.cos(), a.cos() * e.cos(), e.sin()];
                    let gains = gains_at(&panner, position);
                    assert!(
                        (power(&gains) - 1.0).abs() < 1e-3,
                        "{}: az {azimuth} el {elevation}: power {}",
                        layout.name,
                        power(&gains)
                    );
                }
            }
        }
    }

    /// The check that the azimuth sign is the right way round. Left has to
    /// come out left: get this backwards and the whole programme is mirrored,
    /// every test above still passes, and only a listener notices.
    #[test]
    fn left_is_left_and_right_is_right() {
        let layout = Layout::surround_7_1_4();
        let panner = Panner::new(&layout).unwrap();
        let angle = 30f64.to_radians();

        let left_side = gains_at(&panner, [-angle.sin(), angle.cos(), 0.0]);
        let right_side = gains_at(&panner, [angle.sin(), angle.cos(), 0.0]);

        let left = layout.index_of("M+030").unwrap();
        let right = layout.index_of("M-030").unwrap();

        assert!(left_side[left] > 0.9, "left object: {}", left_side[left]);
        assert!(left_side[right] < 0.1, "left object leaked right");
        assert!(
            right_side[right] > 0.9,
            "right object: {}",
            right_side[right]
        );
        assert!(right_side[left] < 0.1, "right object leaked left");
    }

    /// Gains are amplitudes and must never be negative: a negative one would
    /// subtract an object from a speaker instead of placing it.
    #[test]
    fn gains_are_never_negative() {
        let layout = Layout::surround_7_1();
        let panner = Panner::new(&layout).unwrap();
        for azimuth in (0..360).step_by(7) {
            let a = f64::from(azimuth).to_radians();
            for gain in gains_at(&panner, [-a.sin(), a.cos(), 0.0]) {
                assert!(gain >= 0.0, "negative gain {gain}");
            }
        }
    }

    /// A size widens the image at constant power and leaves the loudest
    /// speaker where it was; no size is exactly the point.
    #[test]
    fn a_size_widens_the_image_at_constant_power() {
        let layout = Layout::surround_7_1_4();
        let panner = Panner::new(&layout).unwrap();
        let angle = 30f64.to_radians();
        let position = [-angle.sin(), angle.cos(), 0.0];
        let left = layout.index_of("M+030").unwrap();

        let mut point = vec![0.0f32; layout.channels()];
        panner.gains(position, 0.0, &mut point);
        let mut wide = vec![0.0f32; layout.channels()];
        panner.gains(position, 0.5, &mut wide);

        assert!(point.iter().filter(|g| **g > 0.01).count() == 1);
        assert!(wide.iter().filter(|g| **g > 0.05).count() >= 3, "{wide:?}");
        assert!((power(&wide) - 1.0).abs() < 1e-3, "power {}", power(&wide));
        let loudest = wide
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(channel, _)| channel);
        assert_eq!(loudest, Some(left));
        assert!(wide[left] < point[left]);
    }

    /// The hull is a hull: every face has every other direction on the
    /// listener's side of it, and a coplanar ring is fanned once rather than
    /// covered twice.
    #[test]
    fn the_hull_of_a_layout_is_closed_and_covers_each_direction_once() {
        let directions = speakers_of(&Layout::surround_7_1_4());
        let hull = Hull::new(&directions, Path::new("7.1.4")).unwrap();
        assert_eq!(hull.real, 11);
        // Two virtual poles: nothing in a 7.1.4 is within ten degrees of
        // either.
        assert_eq!(hull.directions.len(), 13);

        for triangle in &hull.triangles {
            let [a, b, c] = triangle.corners.map(|corner| hull.directions[corner]);
            let normal = direction_of(cross(sub(b, a), sub(c, a)));
            let offset = dot(normal, a);
            let (normal, offset) = if offset < 0.0 {
                (scale(normal, -1.0), -offset)
            } else {
                (normal, offset)
            };
            for direction in &hull.directions {
                assert!(dot(normal, *direction) <= offset + 1e-6, "{triangle:?}");
            }
        }

        // Euler: a closed triangulated sphere over V vertices has 2V − 4
        // faces.
        assert_eq!(hull.triangles.len(), 2 * hull.directions.len() - 4);
    }

    impl std::fmt::Debug for Triangle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "triangle {:?}", self.corners)
        }
    }
}

#[cfg(test)]
mod corner_tests {
    use super::*;
    use crate::Renderer;
    use crate::fold::ObjectFold;
    use crate::layout::Layout;

    /// Everything above `-80 dB`, named, so a failure says where the energy
    /// went rather than which index disagreed.
    fn audible(layout: &Layout, gains: &[f64]) -> Vec<(String, f64)> {
        layout
            .speakers
            .iter()
            .zip(gains)
            .filter(|(_, gain)| **gain > 1e-4)
            .map(|(label, gain)| ((*label).to_string(), *gain))
            .collect()
    }

    /// The corner of the room is not the direction of the corner speaker, and
    /// this is what that is worth.
    ///
    /// BS.2127 §7.3's allocentric panner sends `[-1, 1, 0]` — the front-left
    /// corner of a master's cube — to front left with a gain of one. Reading
    /// it as a direction sends it to −45°, which is between L and Ls. The
    /// numbers below are the approximation's cost, measured; they are here so
    /// that a panner change shows up as a failing test rather than as a
    /// quietly different mix.
    #[test]
    fn the_corner_of_the_cube_is_not_the_direction_of_the_corner_speaker() {
        let layout = Layout::surround_5_1();
        let panner = Panner::new(&layout).expect("a 5.1 panner");
        let mut gains = vec![0.0f64; layout.channels()];

        panner.gains_f64([-1.0, 1.0, 0.0], 0.0, &mut gains);
        let corner = audible(&layout, &gains);
        assert_eq!(corner.len(), 2, "{corner:?}");
        assert_eq!(corner[0].0, "M+030");
        assert!((corner[0].1 - 0.962).abs() < 5e-3, "{corner:?}");
        assert_eq!(corner[1].0, "M+110");
        assert!((corner[1].1 - 0.275).abs() < 5e-3, "{corner:?}");

        // The position that *does* land on front left is the corner pulled
        // onto that speaker's own direction: thirty degrees, not forty-five.
        panner.gains_f64([-(30f64.to_radians().tan()), 1.0, 0.0], 0.0, &mut gains);
        let aimed = audible(&layout, &gains);
        assert_eq!(aimed.len(), 1, "{aimed:?}");
        assert_eq!(aimed[0].0, "M+030");
        assert!((aimed[0].1 - 1.0).abs() < 1e-3, "{aimed:?}");

        // And an object on a wall costs nothing: the error is a corner effect.
        panner.gains_f64([0.0, 1.0, 0.0], 0.0, &mut gains);
        let front = audible(&layout, &gains);
        assert_eq!(front.len(), 1, "{front:?}");
        assert_eq!(front[0].0, "M+000");
        assert!((front[0].1 - 1.0).abs() < 1e-3, "{front:?}");
    }

    /// The fold reads the cube, so the same corner lands on L exactly.
    ///
    /// Which is the other half of the statement above: the 2.0, 5.1 and 7.1
    /// layers of the clustering metric are allocentric and the 7.1.4 layer is
    /// not, and a test that says so keeps the two from being conflated.
    #[test]
    fn the_fold_puts_that_corner_on_front_left() {
        let layout = Layout::surround_5_1();
        let fold = ObjectFold::for_layout(&layout).expect("a 5.1 fold");
        let mut gains = vec![0.0f64; fold.channels()];
        Renderer::gains(&fold, [-1.0, 1.0, 0.0], 0.0, &mut gains);
        let corner = audible(&layout, &gains);
        assert_eq!(corner.len(), 1, "L alone, and nothing behind: {corner:?}");
        assert_eq!(corner[0].0, "M+030");
        // Not one: the fold scales by the reference's own measured level for
        // an object on the floor, `elevation_scale(0.0) == 0.9487`. That is a
        // level, not a spread — the corner reaches one speaker and no other,
        // which is the claim being made here.
        assert!((corner[0].1 - 0.9487).abs() < 1e-4, "{corner:?}");
    }
}
