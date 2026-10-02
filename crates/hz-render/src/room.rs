//! Panning in the room's own frame: the cube a master's positions live in.
//!
//! A master's positions are not directions. `[-1, 1, 0]` is the front-left
//! corner of the room, where front left stands, and `[-1, 0, 0]` the middle
//! of the left wall, where the side surround stands — see
//! [`crate::panner`]'s module note for what reading them as directions
//! costs. A bed delivered to listeners has to put a master's front left on
//! front left, so it is panned here, on the cube.
//!
//! The law is the one [`crate::fold`] measured off shipped streams, taken
//! one axis further. Speakers sit at their cube positions
//! ([`crate::fold::bed_position`]) in **layers** by height, each layer in
//! **rows** by depth, each row in order across the width. An object is
//! faded between the two layers around its height, within each between the
//! two rows around its depth, within each of those between the two speakers
//! around its width — every fade equal power and linear in the cube
//! coordinate, `cos(π/2·t)` against `sin(π/2·t)`. Each fade preserves
//! power, so their product does: the gains of any position square-sum to
//! one, and a position on a speaker gives that speaker alone.
//!
//! Two things it leaves out, both stated rather than approximated. An
//! object's **size** does not reach it: the measured law has nowhere to put
//! one, and widening an object is a renderer's decision the master does not
//! spell out. And the **elevation scale** the fold applies is not applied:
//! it is a level one stream's narrow presentations carry, measured where
//! there are no height speakers to take the energy a raised object loses,
//! and here there are.

use crate::fold::bed_position;
use crate::layout::Layout;
use hz_core::{Error, Result, speakers};
use std::f64::consts::FRAC_PI_2;
use std::path::Path;

/// Pans onto one layout in the room's frame.
#[derive(Debug, Clone)]
pub struct Room {
    channels: usize,
    /// Ascending in height.
    layers: Vec<Layer>,
}

#[derive(Debug, Clone)]
struct Layer {
    z: f64,
    /// Ascending in depth.
    rows: Vec<Row>,
}

#[derive(Debug, Clone)]
struct Row {
    y: f64,
    /// `(x, channel)`, ascending in `x`.
    speakers: Vec<(f64, usize)>,
}

impl Room {
    /// Build the panner for `layout`, or say which speaker has no place in
    /// the cube.
    pub fn new(layout: &Layout) -> Result<Self> {
        let path = Path::new(layout.name);
        let mut layers: Vec<Layer> = Vec::new();
        for (channel, label) in layout.speakers.iter().enumerate() {
            if speakers::is_lfe_label(label) {
                continue;
            }
            let [x, y, z] = bed_position(label).ok_or_else(|| {
                Error::unsupported(path, format!("no place in the cube for speaker `{label}`"))
            })?;
            let layer = match layers.iter().position(|l| l.z == z) {
                Some(at) => &mut layers[at],
                None => {
                    layers.push(Layer {
                        z,
                        rows: Vec::new(),
                    });
                    layers.last_mut().expect("just pushed")
                }
            };
            let row = match layer.rows.iter().position(|r| r.y == y) {
                Some(at) => &mut layer.rows[at],
                None => {
                    layer.rows.push(Row {
                        y,
                        speakers: Vec::new(),
                    });
                    layer.rows.last_mut().expect("just pushed")
                }
            };
            row.speakers.push((x, channel));
        }
        if layers.is_empty() {
            return Err(Error::unsupported(path, "no speaker to pan onto"));
        }
        layers.sort_by(|a, b| a.z.total_cmp(&b.z));
        for layer in &mut layers {
            layer.rows.sort_by(|a, b| a.y.total_cmp(&b.y));
            for row in &mut layer.rows {
                row.speakers.sort_by(|a, b| a.0.total_cmp(&b.0));
            }
        }
        Ok(Self {
            channels: layout.channels(),
            layers,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The gains for an object at an ADM Cartesian `position`, one per
    /// layout channel. `gains` is filled, not appended to; nothing is
    /// allocated.
    pub fn gains(&self, position: [f64; 3], gains: &mut [f64]) {
        debug_assert_eq!(gains.len(), self.channels);
        gains.fill(0.0);
        let [x, y, z] = position;
        let (low, high) = bracket(self.layers.iter().map(|l| l.z), self.layers.len(), z);
        for (layer, weight) in [low, high] {
            if weight == 0.0 {
                continue;
            }
            let rows = &self.layers[layer].rows;
            let (front, back) = bracket(rows.iter().map(|r| r.y), rows.len(), y);
            for (row, depth) in [front, back] {
                if depth == 0.0 {
                    continue;
                }
                let speakers = &rows[row].speakers;
                let (left, right) = bracket(speakers.iter().map(|s| s.0), speakers.len(), x);
                for (speaker, width) in [left, right] {
                    gains[speakers[speaker].1] += weight * depth * width;
                }
            }
        }
    }
}

/// The two neighbours of `value` among `count` ascending coordinates, each
/// with its equal-power weight. Outside the range, or on a coordinate, one
/// neighbour takes everything and the other is weighted nought.
fn bracket(
    coordinates: impl Iterator<Item = f64> + Clone,
    count: usize,
    value: f64,
) -> ((usize, f64), (usize, f64)) {
    let mut below = None;
    for (index, at) in coordinates.enumerate() {
        if at >= value {
            return match below {
                None => ((index, 1.0), (index, 0.0)),
                Some((lower, from)) => {
                    let from: f64 = from;
                    let t = (value - from) / (at - from);
                    (
                        (lower, f64::cos(FRAC_PI_2 * t)),
                        (index, f64::sin(FRAC_PI_2 * t)),
                    )
                }
            };
        }
        below = Some((index, at));
    }
    ((count - 1, 1.0), (count - 1, 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gains(position: [f64; 3]) -> Vec<f64> {
        let room = Room::new(&Layout::surround_7_1_4()).unwrap();
        let mut out = vec![0.0; 12];
        room.gains(position, &mut out);
        out
    }

    fn only(out: &[f64], channel: usize) {
        for (c, g) in out.iter().enumerate() {
            let want = if c == channel { 1.0 } else { 0.0 };
            assert!((g - want).abs() < 1e-12, "channel {c}: {out:?}");
        }
    }

    /// Every speaker's own cube position lands on it alone — the corners
    /// included, which is the point of panning on the cube.
    #[test]
    fn a_bed_position_lands_on_its_speaker() {
        let layout = Layout::surround_7_1_4();
        for (channel, label) in layout.speakers.iter().enumerate() {
            if *label == "LFE1" {
                continue;
            }
            only(&gains(bed_position(label).unwrap()), channel);
        }
    }

    #[test]
    fn left_is_left() {
        let out = gains([-0.5, 1.0, 0.0]);
        let layout = Layout::surround_7_1_4();
        let left = layout.index_of("M+030").unwrap();
        let right = layout.index_of("M-030").unwrap();
        assert!(out[left] > 0.5 && out[right] == 0.0, "{out:?}");
    }

    /// Power is kept everywhere in the room, not just on the speakers.
    #[test]
    fn power_is_kept_everywhere() {
        for x in [-1.0, -0.7, -0.2, 0.0, 0.4, 1.0, 1.3] {
            for y in [-1.0, -0.5, 0.1, 0.9, 1.0] {
                for z in [-0.2, 0.0, 0.3, 0.8, 1.0] {
                    let power: f64 = gains([x, y, z]).iter().map(|g| g * g).sum();
                    assert!((power - 1.0).abs() < 1e-12, "{x} {y} {z}: {power}");
                }
            }
        }
    }

    /// Halfway up the front-left corner: front left and its height speaker,
    /// at −3 dB each.
    #[test]
    fn height_fades_between_the_layers() {
        let out = gains([-1.0, 1.0, 0.5]);
        let layout = Layout::surround_7_1_4();
        let floor = layout.index_of("M+030").unwrap();
        let ceiling = layout.index_of("U+030").unwrap();
        let half = std::f64::consts::FRAC_1_SQRT_2;
        assert!((out[floor] - half).abs() < 1e-12);
        assert!((out[ceiling] - half).abs() < 1e-12);
    }

    /// Overhead: the four height speakers equally.
    #[test]
    fn overhead_is_the_four_heights() {
        let out = gains([0.0, 0.0, 1.0]);
        let layout = Layout::surround_7_1_4();
        for label in ["U+030", "U-030", "U+135", "U-135"] {
            let g = out[layout.index_of(label).unwrap()];
            assert!((g - 0.5).abs() < 1e-12, "{label}: {out:?}");
        }
    }
}
