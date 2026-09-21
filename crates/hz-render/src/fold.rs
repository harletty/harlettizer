//! Folding an object onto a channel layout the way a reference stream does.
//!
//! [`panner`](crate::panner) puts an object on a layout by vector-base
//! amplitude panning over a triangulated hull. That is the right answer to
//! "where does this object belong", and it is not what a TrueHD stream's
//! narrower presentations are made with.
//!
//! What they are made with was read out of one. A shipped stream carries the
//! object elements and the encoder's own 7.1 fold of them side by side, so
//! decoding both and solving for the gains recovers the law itself rather than
//! a plausible substitute — [`docs/fold.md`](../../../docs/fold.md) has the
//! measurement and its provenance. It is separable, and it is not VBAP:
//!
//! * The speakers sit at **room corners and wall midpoints**, not at their
//!   nominal azimuths. Front left is at `x = -1`, not at the projection of
//!   30 degrees onto the cube, which would be `-0.577`. The positions are the
//!   ones the stream's own bed elements declare.
//! * They form three **rows** across `y` — front, side, rear — and an object
//!   is faded between rows on `y` alone and between the speakers of a row on
//!   `x` alone. The gain is the product.
//! * Both fades are equal power and **linear in the cube coordinate**:
//!   `cos(pi/2 * t)` against `sin(pi/2 * t)`, with `t` the distance to the
//!   speaker divided by that row's spacing.
//! * `z` does not steer. It scales, by [`elevation_scale`].
//!
//! # The spacing is per row, and getting it wrong is quiet
//!
//! The front row holds three speakers one unit apart; the side and rear rows
//! hold two, two units apart. Dividing by a single radius instead makes an
//! object at `x = 0` in the side row land as `cos(pi/2) = 0` in *both*
//! speakers rather than `0.707` in each. The vector is then renormalised and
//! the output still looks like a pan — it is just wrong everywhere except the
//! speakers themselves, which is exactly where a spot check would look.
//!
//! # The narrower presentations are this fold, then a matrix
//!
//! 5.1 and 2.0 are not rendered on their own geometry — rendering directly
//! onto a 5.1 layout scores 0.143 where taking the 7.1 gains through a fixed
//! matrix scores 0.060. So [`ObjectFold`] always renders at 7.1 and folds
//! from there, with the matrices in [`presentation_matrix`].
//!
//! Which stream you measure that on matters, and the major sync says which:
//! in streams whose `substream_info` is `0xcc` with extended info `1`, the
//! narrow presentations are **separately authored** and are not a function of
//! the elements at all — a fit on half a restart interval predicts the other
//! half to 0.89 of the signal, which is to say not at all. In `0xfc`/`3`
//! streams the same test lands at 0.03. The matrices here come from the
//! second kind.
//!
//! And there is more than one matrix. Two of the three streams measured send
//! a rear surround into *both* sides; the third sends it to its own side and
//! uses the published stereo fold throughout. The two are not close — each
//! scores about 0.25 of signal on the other's stream — and each stream holds
//! to one across both its narrow presentations, so it is a declaration and
//! not a drift.
//!
//! Where it is declared is **not** in the major sync. Nine streams were
//! measured and their channel meaning decodes to identical presentation
//! fields — same dialogue norms, mix levels and source formats for 2ch, 6ch
//! and 8ch throughout. The only structural field that varies is
//! `twoch_control_enabled`, and it does not predict the fold: thirteen streams
//! in, it has coincided with `SameSide` in all three that set it, and the ten
//! that leave it clear give seven `Spread`, two `SameSide` and one behaviour
//! this does not implement. So [`FoldMode`] is a parameter.
//!
//! The likelier answer is that nothing declares it, because nothing needs to:
//! a narrow presentation *is* the earlier substreams' matrices, and those are
//! in the bitstream already. An encoder picks, and both picks are attested in
//! shipped streams. So this one picks too, and says so.
//!
//! # Bed elements are routed, not folded
//!
//! An element parked on a speaker position is not run through this. The
//! reference gives those fixed gains — unity on the front row and the LFE,
//! `0.8665` on the sides, `0.8419` on the rears — and scoring them as though
//! they had been panned is what makes a correct implementation look wrong.

use crate::layout::Layout;
use hz_core::{Error, Result, speakers};
use std::path::Path;

/// Where a speaker sits in the normalised room cube.
///
/// Not derived from the label's azimuth: see the module note. These are the
/// positions a stream's bed elements declare for themselves.
fn cube_position(label: &str) -> Option<(f64, f64)> {
    Some(match label {
        "M+030" => (-1.0, 1.0),
        "M-030" => (1.0, 1.0),
        "M+000" => (0.0, 1.0),
        "M+090" => (-1.0, 0.0),
        "M-090" => (1.0, 0.0),
        "M+135" => (-1.0, -1.0),
        "M-135" => (1.0, -1.0),
        _ => return None,
    })
}

/// Where a bed channel sits in the cube, as an object position.
///
/// The positions above with the height the label's tier implies: the middle
/// row on the floor, the upper row at the ceiling. What a clustering pins a
/// bed channel to — see `hz_cluster::Object::pinned` — since a bed channel is
/// a speaker feed and belongs at its speaker.
///
/// The low frequency channel has no position and is not a direction; a caller
/// that carries one pins it wherever its own model puts it. `None` for any
/// label this has no measurement for, which is a label to refuse rather than
/// to guess at.
///
/// Either spelling. This workspace names a channel two ways — the ADM label
/// `M+030` and the master format's `L` — and a master set's bed channels are
/// named the second way, so answering only for the first meant every caller
/// walking a master got `None` for a channel this does have a position for.
pub fn bed_position(name: &str) -> Option<[f64; 3]> {
    let label = speakers::by_master_name(name).map_or(name, |speaker| speaker.label);
    let upper = label.starts_with('U');
    let floor = if upper {
        // The upper row's cube positions are the middle row's, one tier up.
        cube_position(&format!("M{}", &label[1..]))?
    } else {
        cube_position(label)?
    };
    Some([floor.0, floor.1, if upper { 1.0 } else { 0.0 }])
}

/// The scale an object's gain vector carries at height `z`.
///
/// Flat near the floor, falling through the middle, flat again from about two
/// thirds of the way up. Read off 186 922 measured objects; the knots are the
/// height codes the metadata actually uses, so this interpolates between
/// measurements rather than through a curve nobody fitted.
///
/// **This curve is not universal.** Two streams give it, a third gives a flat
/// 0.8418 — the value this curve reaches only at its top — at every height it
/// visits. Its objects are otherwise panned by exactly the law above: the
/// direction of its gain vectors agrees at a cosine of 0.99992, and swapping
/// this curve for that constant takes the error against it from 0.123 to
/// 0.015. So the *direction* looks like a property of the format and the
/// *scale* looks like a property of the stream. Only three streams have been
/// measured for it; see [`docs/fold.md`](../../../docs/fold.md).
pub fn elevation_scale(z: f64) -> f64 {
    /// `(height, scale)`, ascending.
    const TABLE: [(f64, f64); 11] = [
        (0.00, 0.9487),
        (0.20, 0.9470),
        (0.27, 0.9445),
        (0.33, 0.9295),
        (0.40, 0.8926),
        (0.47, 0.8849),
        (0.53, 0.8610),
        (0.60, 0.8514),
        (0.67, 0.8431),
        (0.73, 0.8419),
        (1.00, 0.8418),
    ];
    let z = z.clamp(0.0, 1.0);
    let mut previous = TABLE[0];
    for point in TABLE.into_iter().skip(1) {
        if z <= point.0 {
            let span = point.0 - previous.0;
            let t = if span > 0.0 {
                (z - previous.0) / span
            } else {
                0.0
            };
            return previous.1 + (point.1 - previous.1) * t;
        }
        previous = point;
    }
    previous.1
}

/// One row of speakers at a fixed depth.
#[derive(Debug)]
struct Row {
    y: f64,
    /// `(x, channel)`, ascending in `x`.
    speakers: Vec<(f64, usize)>,
    /// Distance between adjacent speakers in this row.
    spacing: f64,
}

/// How many channels the fold renders on before any matrix: 7.1.
const RENDERED: usize = 8;

/// Which of the two folds a stream declares.
///
/// Named for what they do to a rear surround, which is the audible half of
/// the difference. The stereo levels differ with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FoldMode {
    /// A rear surround reaches both side surrounds, at `(sqrt(3)/2, 1/2)`,
    /// and the stereo fold takes the surrounds at 0.909 with the whole scaled
    /// by 0.52. Two of the three streams measured; the default for that
    /// reason and no better one.
    #[default]
    Spread,
    /// A rear surround reaches its own side only, at unity, and the stereo
    /// fold is the published one: `0.5 * (L + 0.707 * C + Ls)`. The third
    /// stream, which it fits to 0.008 and 0.019 of signal.
    SameSide,
}

/// Folds objects onto a layout the way a reference stream's presentations are.
#[derive(Debug)]
pub struct ObjectFold {
    /// Always the 7.1 rows. Narrower presentations are a matrix away.
    rows: Vec<Row>,
    /// `None` when the target *is* 7.1.
    matrix: Option<[[f64; RENDERED]; RENDERED]>,
    channels: usize,
    /// The layout it folds to, for reports.
    name: &'static str,
}

/// The matrix the reference folds a rendered 7.1 presentation to `to` with.
///
/// Rows are 7.1 channels in `Layout::surround_7_1()` order, columns are
/// channels of `to`. Measured over 187 k objects; the clean forms below cost
/// about a point of agreement against a freely fitted matrix and are kept
/// because a fitted matrix at this accuracy is partly fitting the renderer's
/// own error.
///
/// The rear surrounds do **not** fold onto the same side at half power the
/// way ATSC A/52 has them. The reference spreads each rear across *both*
/// side surrounds at `(sqrt(3)/2, 1/2)`, whose squares sum to one. It
/// preserves power, like everything else in this fold.
pub fn presentation_matrix(
    to: &Layout,
    mode: FoldMode,
) -> Result<Option<[[f64; RENDERED]; RENDERED]>> {
    const ROOT3_2: f64 = 0.866_025_403_784_438_6;
    // How much of a rear surround reaches its own side and the far one.
    let (near, far) = match mode {
        FoldMode::Spread => (ROOT3_2, 0.5),
        FoldMode::SameSide => (1.0, 0.0),
    };
    // What the stereo fold gives the surrounds, and what it scales by.
    let (stereo_surround, stereo_scale) = match mode {
        FoldMode::Spread => (0.909, 0.52),
        FoldMode::SameSide => (1.0, 0.5),
    };

    let path = Path::new(to.name);
    let from = Layout::surround_7_1();
    if to.speakers == from.speakers {
        return Ok(None);
    }
    let mut matrix = [[0.0f64; RENDERED]; RENDERED];
    let mut put = |source: &str, target: &str, gain: f64| {
        if let (Some(i), Some(j)) = (from.index_of(source), to.index_of(target)) {
            matrix[i][j] += gain;
        }
    };
    match to.speakers.len() {
        6 => {
            for label in ["M+030", "M-030", "M+000", "LFE1"] {
                put(label, label, 1.0);
            }
            put("M+090", "M+110", 1.0);
            put("M-090", "M-110", 1.0);
            put("M+135", "M+110", near);
            put("M+135", "M-110", far);
            put("M-135", "M-110", near);
            put("M-135", "M+110", far);
        }
        2 => {
            let half_power = std::f64::consts::FRAC_1_SQRT_2 * stereo_scale;
            let surround = stereo_surround * stereo_scale;
            put("M+030", "M+030", stereo_scale);
            put("M-030", "M-030", stereo_scale);
            put("M+000", "M+030", half_power);
            put("M+000", "M-030", half_power);
            put("M+090", "M+030", surround);
            put("M-090", "M-030", surround);
            put("M+135", "M+030", surround * near);
            put("M+135", "M-030", surround * far);
            put("M-135", "M-030", surround * near);
            put("M-135", "M+030", surround * far);
        }
        _ => {
            return Err(Error::unsupported(
                path,
                "no measured fold from 7.1 to this layout",
            ));
        }
    }
    Ok(Some(matrix))
}

impl ObjectFold {
    /// Build the fold for a layout, or say which speaker has no known position.
    /// Build the fold with the mode two of the three measured streams use.
    ///
    /// Say which with [`ObjectFold::with_mode`] when the stream is known.
    pub fn for_layout(layout: &Layout) -> Result<Self> {
        Self::with_mode(layout, FoldMode::default())
    }

    /// Build the fold for a layout and a declared mode.
    pub fn with_mode(layout: &Layout, mode: FoldMode) -> Result<Self> {
        let matrix = presentation_matrix(layout, mode)?;
        // The rows are always 7.1's: a narrower presentation is this render
        // taken through `matrix`, not a render on its own geometry.
        let rendered = Layout::surround_7_1();
        let path = Path::new(rendered.name);
        let mut placed: Vec<(f64, f64, usize)> = Vec::new();
        for (channel, label) in rendered.speakers.iter().enumerate() {
            if speakers::is_lfe_label(label) {
                continue;
            }
            let (x, y) = cube_position(label).ok_or_else(|| {
                Error::unsupported(
                    path,
                    format!("no measured cube position for speaker `{label}`"),
                )
            })?;
            placed.push((x, y, channel));
        }
        let mut depths: Vec<f64> = placed.iter().map(|(_, y, _)| *y).collect();
        depths.sort_by(|a, b| b.partial_cmp(a).expect("cube depths are finite"));
        depths.dedup();
        let mut rows = Vec::with_capacity(depths.len());
        for y in depths {
            let mut speakers: Vec<(f64, usize)> = placed
                .iter()
                .filter(|(_, sy, _)| *sy == y)
                .map(|(x, _, c)| (*x, *c))
                .collect();
            speakers.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("cube widths are finite"));
            // Adjacent speakers in a row are evenly spaced, so any gap gives
            // the spacing; a row of one is never faded across.
            let spacing = if speakers.len() > 1 {
                speakers[1].0 - speakers[0].0
            } else {
                1.0
            };
            rows.push(Row {
                y,
                speakers,
                spacing,
            });
        }
        Ok(Self {
            rows,
            matrix,
            channels: layout.channels(),
            name: layout.name,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The layout it folds to.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Write the gains for an object at `position` into `gains`.
    ///
    /// `position` is ADM Cartesian, the frame a master set carries. Allocates
    /// nothing: the 7.1 render is a fixed-size array on the stack whatever the
    /// target layout is.
    pub fn gains(&self, position: [f64; 3], gains: &mut [f64]) {
        debug_assert_eq!(gains.len(), self.channels);
        let rendered = self.render(position);
        match &self.matrix {
            None => gains.copy_from_slice(&rendered[..self.channels]),
            Some(matrix) => {
                gains.fill(0.0);
                for (source, row) in rendered.iter().zip(matrix) {
                    if *source == 0.0 {
                        continue;
                    }
                    for (out, gain) in gains.iter_mut().zip(row) {
                        *out += source * gain;
                    }
                }
            }
        }
    }

    /// The 7.1 gains, before any presentation matrix.
    fn render(&self, position: [f64; 3]) -> [f64; RENDERED] {
        let mut rendered = [0.0f64; RENDERED];
        let [x, y, z] = position;

        // Across the rows on y, equal power.
        let mut depth = [0.0f64; 4];
        let mut sum = 0.0;
        for (index, row) in self.rows.iter().enumerate().take(depth.len()) {
            let t = ((y - row.y).abs()).clamp(0.0, 1.0);
            let w = (std::f64::consts::FRAC_PI_2 * t).cos();
            depth[index] = w;
            sum += w * w;
        }
        let depth_norm = if sum > 0.0 { sum.sqrt() } else { 1.0 };

        // Within each row on x, equal power, divided by that row's spacing.
        let mut total = 0.0;
        for (index, row) in self.rows.iter().enumerate().take(depth.len()) {
            let across = depth[index] / depth_norm;
            if across == 0.0 {
                continue;
            }
            let mut width_sum = 0.0;
            for (sx, _) in &row.speakers {
                let t = ((x - sx).abs() / row.spacing).clamp(0.0, 1.0);
                let w = (std::f64::consts::FRAC_PI_2 * t).cos();
                width_sum += w * w;
            }
            let width_norm = if width_sum > 0.0 {
                width_sum.sqrt()
            } else {
                1.0
            };
            for (sx, channel) in &row.speakers {
                let t = ((x - sx).abs() / row.spacing).clamp(0.0, 1.0);
                let w = (std::f64::consts::FRAC_PI_2 * t).cos() / width_norm;
                let g = across * w;
                rendered[*channel] = g;
                total += g * g;
            }
        }

        let scale = elevation_scale(z) / if total > 0.0 { total.sqrt() } else { 1.0 };
        for g in rendered.iter_mut() {
            *g *= scale;
        }
        rendered
    }
}

impl crate::Renderer for ObjectFold {
    fn channels(&self) -> usize {
        self.channels
    }

    /// The size is ignored, and that is what the fold does.
    ///
    /// A presentation is rows and columns of a cube with a matrix after them;
    /// there is nowhere in it for a width. A wide object arrives at a
    /// presentation as a point, and the only place its width can go is the
    /// clustering — spread over several elements rather than given a width the
    /// output will not carry. So a metric that includes a fold sees a width
    /// thrown away only through whatever renderer it is measured beside, which
    /// is why the 7.1.4 row is panned and not folded.
    fn gains(&self, position: [f64; 3], _size: f64, out: &mut [f64]) {
        ObjectFold::gains(self, position, out);
    }

    fn name(&self) -> &str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_elevation_scale_is_flat_at_both_ends_and_falls_between() {
        assert!((elevation_scale(0.0) - 0.9487).abs() < 1e-9);
        assert!(
            (elevation_scale(-1.0) - 0.9487).abs() < 1e-9,
            "clamped below"
        );
        assert!((elevation_scale(1.0) - 0.8418).abs() < 1e-9);
        assert!(
            (elevation_scale(2.0) - 0.8418).abs() < 1e-9,
            "clamped above"
        );
        // Between two knots it interpolates rather than stepping.
        let middle = elevation_scale(0.365);
        assert!(middle < 0.9295 && middle > 0.8926, "got {middle}");
        // And it never rises with height once it has started falling.
        let mut previous = elevation_scale(0.33);
        for step in 34..=100 {
            let next = elevation_scale(f64::from(step) / 100.0);
            assert!(next <= previous + 1e-12, "rose at {step}");
            previous = next;
        }
    }

    #[test]
    fn a_row_learns_its_spacing_from_the_speakers_in_it() {
        let fold = ObjectFold::for_layout(&Layout::surround_7_1()).expect("7.1 folds");
        let spacing: Vec<f64> = fold.rows.iter().map(|r| r.spacing).collect();
        // Front row: three speakers a unit apart. Side and rear: two, two apart.
        assert_eq!(spacing, vec![1.0, 2.0, 2.0]);
        let depths: Vec<f64> = fold.rows.iter().map(|r| r.y).collect();
        assert_eq!(depths, vec![1.0, 0.0, -1.0]);
    }

    #[test]
    fn a_layout_with_no_measured_fold_is_refused_rather_than_guessed() {
        let why = ObjectFold::for_layout(&Layout::surround_7_1_4())
            .expect_err("nothing was ever measured folding onto height speakers")
            .to_string();
        assert!(why.contains("7.1.4"), "unhelpful error: {why}");
        assert!(why.contains("no measured fold"), "unhelpful error: {why}");
    }

    #[test]
    fn the_presentations_that_were_measured_all_build() {
        for layout in [
            Layout::surround_7_1(),
            Layout::surround_5_1(),
            Layout::stereo(),
        ] {
            let fold = ObjectFold::for_layout(&layout)
                .unwrap_or_else(|e| panic!("{} should fold: {e}", layout.name));
            assert_eq!(fold.channels(), layout.channels());
            // Only 7.1 is rendered directly; the others carry a matrix.
            assert_eq!(fold.matrix.is_none(), layout.channels() == 8);
        }
    }

    #[test]
    fn the_stereo_fold_keeps_a_centred_object_centred() {
        let fold = ObjectFold::for_layout(&Layout::stereo()).expect("stereo folds");
        let mut gains = vec![0.0; 2];
        for y in [-1.0, 0.0, 1.0] {
            fold.gains([0.0, y, 0.0], &mut gains);
            assert!(
                (gains[0] - gains[1]).abs() < 1e-12,
                "a centred object leaned at y = {y}: {gains:?}"
            );
        }
    }
}
