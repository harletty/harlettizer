//! One way of turning an object into channel gains.
//!
//! There are two in this crate and they are not variations on each other.
//! [`Panner`] is vector-base amplitude panning over a triangulated layout: it
//! answers "where would this sound come from in a room with these speakers".
//! [`ObjectFold`] is what a reference stream's presentations actually do:
//! interpolate across fixed rows and columns of a cube and then apply the
//! matrix that narrows 7.1 to 5.1 or to a stereo pair. On the same object they
//! disagree, and the disagreement is not error — it is the difference between
//! a renderer and a fold.
//!
//! Anything that measures a clustering has to be able to ask both, because a
//! delivery stream is played through both: the 7.1.4 presentation is rendered,
//! and the 2.0, 5.1 and 7.1 presentations are folded. A clustering judged only
//! on the renderer is judged on the one presentation most listeners will not
//! hear.
//!
//! [`Panner`]: crate::Panner
//! [`ObjectFold`]: crate::ObjectFold

/// Gains for an object, on one presentation.
/// `Send`, because a renderer is a pure function of a position: it holds a
/// layout and a triangulation and mutates nothing, so there is nothing for a
/// thread boundary to be wrong about. Requiring it here rather than writing
/// `dyn Renderer + Send` at each of the eight places one is named — and it is
/// what lets a folding encode run its clustering on a thread of its own.
pub trait Renderer: Send {
    /// How many channels it writes.
    fn channels(&self) -> usize;

    /// Write the gains for an object at `position` with this `size`.
    ///
    /// `position` is ADM Cartesian — the cube frame a master set carries, not
    /// a direction on the unit sphere. The two agree only on the sphere's own
    /// surface, and a fold reads the cube.
    ///
    /// `out` is filled, not appended to, and must be [`Renderer::channels`]
    /// long.
    fn gains(&self, position: [f64; 3], size: f64, out: &mut [f64]);

    /// What to call this presentation in a report.
    fn name(&self) -> &str;
}
